#![doc(html_root_url = "https://docs.rs/wasm-bindgen-cli-support/0.2")]

extern crate parity_wasm;
extern crate serde_json;
extern crate wasm_bindgen_shared as shared;
extern crate wasm_gc;
#[macro_use]
extern crate failure;
extern crate wasm_bindgen_wasm_interpreter as wasm_interpreter;

use std::any::Any;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};

use failure::{Error, ResultExt};
use parity_wasm::elements::*;

mod descriptor;
mod js;
pub mod wasm2es6js;

pub struct Bindgen {
    input: Input,
    nodejs: bool,
    nodejs_experimental_modules: bool,
    browser: bool,
    no_modules: bool,
    no_modules_global: Option<String>,
    debug: bool,
    typescript: bool,
    demangle: bool,
    keep_debug: bool,
    // Experimental support for `WeakRefGroup`, an upcoming ECMAScript feature.
    // Currently only enable-able through an env var.
    weak_refs: bool,
}

enum Input {
    Path(PathBuf),
    Bytes(Vec<u8>, String),
    Module(Module, String),
    None,
}

impl Bindgen {
    pub fn new() -> Bindgen {
        Bindgen {
            input: Input::None,
            nodejs: false,
            nodejs_experimental_modules: false,
            browser: false,
            no_modules: false,
            no_modules_global: None,
            debug: false,
            typescript: false,
            demangle: true,
            keep_debug: false,
            weak_refs: env::var("WASM_BINDGEN_WEAKREF").is_ok(),
        }
    }

    pub fn input_path<P: AsRef<Path>>(&mut self, path: P) -> &mut Bindgen {
        self.input = Input::Path(path.as_ref().to_path_buf());
        self
    }

    /// Explicitly specify the already parsed input module.
    ///
    /// Note that this API is a little wonky to avoid tying itself with a public
    /// dependency on the `parity-wasm` crate, what we currently use to parse
    /// wasm mdoules.
    ///
    /// If the `module` argument is a `parity_wasm::Module` then it will be used
    /// directly. Otherwise it will be passed to `into_bytes` to serialize the
    /// module to a vector of bytes, and this will deserialize the module later.
    ///
    /// Note that even if the argument passed in is a `parity_wasm::Module` it
    /// doesn't mean that this won't invoke `into_bytes`, if the `parity_wasm`
    /// crate versions are different we'll have to go through serialization.
    pub fn input_module<T: Any>(
        &mut self,
        name: &str,
        mut module: T,
        into_bytes: impl FnOnce(T) -> Vec<u8>,
    ) -> &mut Bindgen {
        let name = name.to_string();
        if let Some(module) = (&mut module as &mut Any).downcast_mut::<Module>() {
            let blank = Module::new(Vec::new());
            self.input = Input::Module(mem::replace(module, blank), name);
            return self;
        }

        self.input = Input::Bytes(into_bytes(module), name);
        self
    }

    pub fn nodejs(&mut self, node: bool) -> &mut Bindgen {
        self.nodejs = node;
        self
    }

    pub fn nodejs_experimental_modules(&mut self, node: bool) -> &mut Bindgen {
        self.nodejs_experimental_modules = node;
        self
    }

    pub fn browser(&mut self, browser: bool) -> &mut Bindgen {
        self.browser = browser;
        self
    }

    pub fn no_modules(&mut self, no_modules: bool) -> &mut Bindgen {
        self.no_modules = no_modules;
        self
    }

    pub fn no_modules_global(&mut self, name: &str) -> &mut Bindgen {
        self.no_modules_global = Some(name.to_string());
        self
    }

    pub fn debug(&mut self, debug: bool) -> &mut Bindgen {
        self.debug = debug;
        self
    }

    pub fn typescript(&mut self, typescript: bool) -> &mut Bindgen {
        self.typescript = typescript;
        self
    }

    pub fn demangle(&mut self, demangle: bool) -> &mut Bindgen {
        self.demangle = demangle;
        self
    }

    pub fn keep_debug(&mut self, keep_debug: bool) -> &mut Bindgen {
        self.keep_debug = keep_debug;
        self
    }

    pub fn generate<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        self._generate(path.as_ref())
    }

    fn _generate(&mut self, out_dir: &Path) -> Result<(), Error> {
        let (mut module, stem) = match self.input {
            Input::None => bail!("must have an input by now"),
            Input::Module(ref mut m, ref name) => {
                let blank_module = Module::new(Vec::new());
                (mem::replace(m, blank_module), &name[..])
            }
            Input::Bytes(ref b, ref name) => {
                let module = parity_wasm::deserialize_buffer::<Module>(&b)
                    .context("failed to parse input file as wasm")?;
                (module, &name[..])
            }
            Input::Path(ref path) => {
                let contents = fs::read(&path)
                    .with_context(|_| format!("failed to read `{}`", path.display()))?;
                let module = parity_wasm::deserialize_buffer::<Module>(&contents)
                    .context("failed to parse input file as wasm")?;
                let stem = path.file_stem().unwrap().to_str().unwrap();
                (module, stem)
            }
        };
        let programs = extract_programs(&mut module)
            .with_context(|_| "failed to extract wasm-bindgen custom sections")?;

        // Here we're actually instantiating the module we've parsed above for
        // execution. Why, you might be asking, are we executing wasm code? A
        // good question!
        //
        // Transmitting information from `#[wasm_bindgen]` here to the CLI tool
        // is pretty tricky. Specifically information about the types involved
        // with a function signature (especially generic ones) can be hefty to
        // translate over. As a result, the macro emits a bunch of shims which,
        // when executed, will describe to us what the types look like.
        //
        // This means that whenever we encounter an import or export we'll
        // execute a shim function which informs us about its type so we can
        // then generate the appropriate bindings.
        let mut instance = wasm_interpreter::Interpreter::new(&module);

        let (js, ts) = {
            let mut cx = js::Context {
                globals: String::new(),
                imports: String::new(),
                imports_post: String::new(),
                footer: String::new(),
                typescript: format!("/* tslint:disable */\n"),
                exposed_globals: Default::default(),
                required_internal_exports: Default::default(),
                imported_names: Default::default(),
                imported_identifiers: Default::default(),
                exported_classes: Default::default(),
                config: &self,
                module: &mut module,
                function_table_needed: false,
                interpreter: &mut instance,
                memory_init: None,
                imported_functions: Default::default(),
                imported_statics: Default::default(),
            };
            for program in programs.iter() {
                js::SubContext {
                    program,
                    cx: &mut cx,
                    vendor_prefixes: Default::default(),
                }.generate()?;
            }
            cx.finalize(stem)?
        };

        let extension = if self.nodejs_experimental_modules {
            "mjs"
        } else {
            "js"
        };
        let js_path = out_dir.join(stem).with_extension(extension);
        fs::write(&js_path, reset_indentation(&js))
            .with_context(|_| format!("failed to write `{}`", js_path.display()))?;

        if self.typescript {
            let ts_path = out_dir.join(stem).with_extension("d.ts");
            fs::write(&ts_path, ts)
                .with_context(|_| format!("failed to write `{}`", ts_path.display()))?;
        }

        let wasm_path = out_dir.join(format!("{}_bg", stem)).with_extension("wasm");

        if self.nodejs {
            let js_path = wasm_path.with_extension(extension);
            let shim = self.generate_node_wasm_import(&module, &wasm_path);
            fs::write(&js_path, shim)
                .with_context(|_| format!("failed to write `{}`", js_path.display()))?;
        }

        let wasm_bytes = parity_wasm::serialize(module)?;
        fs::write(&wasm_path, wasm_bytes)
            .with_context(|_| format!("failed to write `{}`", wasm_path.display()))?;
        Ok(())
    }

    fn generate_node_wasm_import(&self, m: &Module, path: &Path) -> String {
        let mut imports = BTreeSet::new();
        if let Some(i) = m.import_section() {
            for i in i.entries() {
                imports.insert(i.module());
            }
        }

        let mut shim = String::new();

        if self.nodejs_experimental_modules {
            for (i, module) in imports.iter().enumerate() {
                shim.push_str(&format!("import * as import{} from '{}';\n", i, module));
            }
            // On windows skip the leading `/` which comes out when we parse a
            // url to use `C:\...` instead of `\C:\...`
            shim.push_str(&format!(
                "
                import * as path from 'path';
                import * as fs from 'fs';
                import * as url from 'url';
                import * as process from 'process';

                let file = path.dirname(url.parse(import.meta.url).pathname);
                if (process.platform === 'win32') {{
                    file = file.substring(1);
                }}
                const bytes = fs.readFileSync(path.join(file, '{}'));
            ",
                path.file_name().unwrap().to_str().unwrap()
            ));
        } else {
            shim.push_str(&format!(
                "
                const path = require('path').join(__dirname, '{}');
                const bytes = require('fs').readFileSync(path);
            ",
                path.file_name().unwrap().to_str().unwrap()
            ));
        }
        shim.push_str("let imports = {};\n");
        for (i, module) in imports.iter().enumerate() {
            if self.nodejs_experimental_modules {
                shim.push_str(&format!("imports['{}'] = import{};\n", module, i));
            } else {
                shim.push_str(&format!("imports['{0}'] = require('{0}');\n", module));
            }
        }

        shim.push_str(&format!(
            "
                const wasmModule = new WebAssembly.Module(bytes);
                const wasmInstance = new WebAssembly.Instance(wasmModule, imports);
            ",
        ));

        if self.nodejs_experimental_modules {
            if let Some(e) = m.export_section() {
                for name in e.entries().iter().map(|e| e.field()) {
                    shim.push_str("export const ");
                    shim.push_str(name);
                    shim.push_str(" = wasmInstance.exports.");
                    shim.push_str(name);
                    shim.push_str(";\n");
                }
            }
        } else {
            shim.push_str("module.exports = wasmInstance.exports;\n");
        }

        reset_indentation(&shim)
    }
}

fn extract_programs(module: &mut Module) -> Result<Vec<shared::Program>, Error> {
    let version = shared::version();
    let mut ret = Vec::new();
    let mut to_remove = Vec::new();

    for (i, s) in module.sections().iter().enumerate() {
        let custom = match *s {
            Section::Custom(ref s) => s,
            _ => continue,
        };
        if custom.name() != "__wasm_bindgen_unstable" {
            continue;
        }
        to_remove.push(i);

        let mut payload = custom.payload();
        while payload.len() > 0 {
            let len = ((payload[0] as usize) << 0)
                | ((payload[1] as usize) << 8)
                | ((payload[2] as usize) << 16)
                | ((payload[3] as usize) << 24);
            let (a, b) = payload[4..].split_at(len as usize);
            payload = b;

            let p: shared::ProgramOnlySchema = match serde_json::from_slice(&a) {
                Ok(f) => f,
                Err(e) => bail!("failed to decode what looked like wasm-bindgen data: {}", e),
            };
            if p.schema_version != shared::SCHEMA_VERSION {
                bail!(
                    "

it looks like the Rust project used to create this wasm file was linked against
a different version of wasm-bindgen than this binary:

  rust wasm file: {}
     this binary: {}

Currently the bindgen format is unstable enough that these two version must
exactly match, so it's required that these two version are kept in sync by
either updating the wasm-bindgen dependency or this binary. You should be able
to update the wasm-bindgen dependency with:

    cargo update -p wasm-bindgen

or you can update the binary with

    cargo install -f wasm-bindgen-cli

if this warning fails to go away though and you're not sure what to do feel free
to open an issue at https://github.com/rustwasm/wasm-bindgen/issues!
",
                    p.version,
                    version
                );
            }
            let p: shared::Program = match serde_json::from_slice(&a) {
                Ok(f) => f,
                Err(e) => bail!("failed to decode what looked like wasm-bindgen data: {}", e),
            };
            ret.push(p);
        }
    }

    for i in to_remove.into_iter().rev() {
        module.sections_mut().remove(i);
    }
    Ok(ret)
}

fn reset_indentation(s: &str) -> String {
    let mut indent: u32 = 0;
    let mut dst = String::new();

    for line in s.lines() {
        let line = line.trim();
        if line.starts_with('}') || (line.ends_with('}') && !line.starts_with('*')) {
            indent = indent.saturating_sub(1);
        }
        let extra = if line.starts_with(':') || line.starts_with('?') {
            1
        } else {
            0
        };
        if !line.is_empty() {
            for _ in 0..indent + extra {
                dst.push_str("    ");
            }
            dst.push_str(line);
        }
        dst.push_str("\n");
        if line.ends_with('{') {
            indent += 1;
        }
    }
    return dst;
}
