//! WASM bindings for the MSC compiler, exposed to the `@retrochains/msc`
//! TypeScript package so `cl /c` runs in the browser (or Node) without shelling
//! out. MSC here is compile-only (small model, `/AS`); there is no MSC
//! linker/librarian in the repo, so the surface is a single `compile`.
//!
//! Like `bcc-wasm`, the whole crate is `#![cfg(target_arch = "wasm32")]` with a
//! target-gated dependency, so the host build is an empty lib. Build the real
//! module with `scripts/build-wasm.sh msc-wasm msc msc_wasm`.
#![cfg(target_arch = "wasm32")]
// wasm-bindgen's macro expands `unsafe` glue; the workspace denies `unsafe_code`.
#![allow(unsafe_code)]

use wasm_bindgen::prelude::*;

/// Compile C source to an OMF object file (`cl /c /AS`). `filename` is the
/// source name recorded in the object (e.g. `HELLO.C`).
///
/// # Errors
/// On a source shape the compiler doesn't model yet.
#[wasm_bindgen]
pub fn compile(source: &str, filename: &str) -> Result<Vec<u8>, JsError> {
    msc::compile(source, filename).map_err(|e| JsError::new(&e.to_string()))
}
