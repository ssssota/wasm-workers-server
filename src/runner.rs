// Copyright 2022 VMware, Inc.
// SPDX-License-Identifier: Apache-2.0

use actix_web::{http::header::HeaderMap, HttpRequest};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use wasi_common::{pipe::ReadPipe, pipe::WritePipe};
use wasmtime::*;
use wasmtime_wasi::sync::WasiCtxBuilder;

// Load the QuickJS compiled engine from kits/javascript
static JS_ENGINE_WASM: &[u8] =
    include_bytes!("../kits/javascript/wasm-workers-quick-js-engine.wasm");

/// JSON input for wasm modules. This information is passed via STDIN / WASI
/// to the module.
#[derive(Serialize, Deserialize)]
pub struct WasmInput {
    /// Request full URL
    url: String,
    /// Request method
    method: String,
    /// Request headers
    headers: HashMap<String, String>,
    /// Request body
    body: String,
    /// Key / Value store content if available
    kv: HashMap<String, String>,
}

impl WasmInput {
    /// Generates a new struct to pass the data to wasm module. It's based on the
    /// HttpRequest, body and the Key / Value store (if available)
    pub fn new(request: &HttpRequest, body: String, kv: Option<HashMap<String, String>>) -> Self {
        Self {
            url: request.uri().to_string(),
            method: String::from(request.method().as_str()),
            headers: build_headers_hash(request.headers()),
            body: body,
            kv: kv.unwrap_or(HashMap::new()),
        }
    }
}

/// JSON output from a wasm module. This information is passed via STDOUT / WASI
/// from the module.
#[derive(Serialize, Deserialize, Debug)]
pub struct WasmOutput {
    /// Response body
    pub body: String,
    /// Response headers
    pub headers: HashMap<String, String>,
    /// Response HTTP status
    pub status: u16,
    /// New state of the K/V store if available
    pub kv: HashMap<String, String>,
}

/// Builds the JSON string to pass to the Wasm module using WASI STDIO strategy.
pub fn build_wasm_input(
    request: &HttpRequest,
    body: String,
    kv: Option<HashMap<String, String>>,
) -> String {
    serde_json::to_string(&WasmInput::new(request, body, kv)).unwrap()
}

/// Create HashMap from a HeadersMap
pub fn build_headers_hash(headers: &HeaderMap) -> HashMap<String, String> {
    let mut parsed_headers = HashMap::new();

    for (key, value) in headers.iter() {
        parsed_headers.insert(
            String::from(key.as_str()),
            String::from(value.to_str().unwrap()),
        );
    }

    parsed_headers
}

#[derive(Clone)]
pub enum RunnerHandlerType {
    Wasm,
    JavaScript,
}

/// A runner is composed by a Wasmtime engine instance and a preloaded
/// wasm module.
#[derive(Clone)]
pub struct Runner {
    /// Engine that runs the actual Wasm module
    engine: Engine,
    /// The type of the required runner
    runner_type: RunnerHandlerType,
    /// Preloaded Module
    module: Module,
    /// Source code if required
    source: String,
}

impl Runner {
    /// Creates a Runner. It will preload the module from the given wasm file
    pub fn new(path: &PathBuf) -> Result<Self> {
        let engine = Engine::default();
        let (runner_type, module, source) = if Self::is_js_file(path) {
            let module = Module::from_binary(&engine, JS_ENGINE_WASM)?;

            (
                RunnerHandlerType::JavaScript,
                module,
                fs::read_to_string(path)
                    .expect(&format!("Error reading {}", path.display()))
                    .to_string(),
            )
        } else {
            let module = Module::from_file(&engine, path)?;

            (RunnerHandlerType::Wasm, module, String::new())
        };

        Ok(Self {
            engine,
            runner_type,
            module,
            source,
        })
    }

    fn is_js_file(path: &PathBuf) -> bool {
        match path.extension() {
            Some(os_str) => os_str == "js",
            None => false,
        }
    }

    /// Run the wasm module. To inject the data, it already receives the JSON input
    /// from the WasmInput serialization. It initializes a new WASI context with
    /// the required pipes. Then, it sends the data and read the output from the wasm
    /// run.
    pub fn run(&self, input: &str) -> Result<WasmOutput> {
        let stdin = match self.runner_type {
            RunnerHandlerType::Wasm => ReadPipe::from(input),
            RunnerHandlerType::JavaScript => {
                let mut contents = String::new();
                contents.push_str(&self.source);
                // Separator
                contents.push_str("[[[input]]]");
                contents.push_str(input);

                ReadPipe::from(contents)
            }
        };
        let stdout = WritePipe::new_in_memory();
        let stderr = WritePipe::new_in_memory();

        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::add_to_linker(&mut linker, |s| s)?;

        // WASI context
        let wasi = WasiCtxBuilder::new()
            .stdin(Box::new(stdin.clone()))
            .stdout(Box::new(stdout.clone()))
            .stderr(Box::new(stderr.clone()))
            .inherit_args()?
            .build();
        let mut store = Store::new(&self.engine, wasi);

        linker.module(&mut store, "", &self.module)?;
        linker
            .get_default(&mut store, "")?
            .typed::<(), (), _>(&store)?
            .call(&mut store, ())?;

        drop(store);

        let contents: Vec<u8> = stdout
            .try_into_inner()
            .map_err(|_err| anyhow::Error::msg("Nothing to show"))?
            .into_inner();

        let output: WasmOutput = serde_json::from_slice(&contents)?;

        Ok(output)
    }
}
