//! WASM bindings for the BCC toolchain — `bcc` (compile), `tasm` (assemble),
//! `tlink` (link), `tlib` (librarian) — exposed to the `@retrochains/bcc`
//! TypeScript package so the whole compile → assemble → link → archive flow runs
//! in the browser (or Node) without shelling out.
//!
//! The boundary is deliberately FLAT: positional primitives for the
//! single-input tools (`compile`, `assemble`) and small builder structs for the
//! multi-object tools (`Linker`, `Librarian`). The ergonomic object-style API
//! lives in TypeScript (`packages/bcc/src/index.ts`); keeping the Rust glue dumb
//! avoids a serde dependency and keeps the module small. Each underlying core is
//! already pure byte-in/byte-out, so this only marshals JS values.
//!
//! The entire crate is `#![cfg(target_arch = "wasm32")]`: on the host it's an
//! empty lib (so `cargo build --workspace` never needs the wasm toolchain).
//! Build the real module with `scripts/build-wasm.sh`.
#![cfg(target_arch = "wasm32")]
// wasm-bindgen's macro expands `unsafe` glue in this crate; the workspace denies
// `unsafe_code`, so allow it here for the generated bindings.
#![allow(unsafe_code)]
// The boundary is intentionally flat: the compile entry points take the `bcc`
// flags as positional bools (the TS wrapper bundles them into an options object),
// and the mtime is a clamped `f64`→`SystemTime` conversion.
#![allow(clippy::fn_params_excessive_bools, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::time::{Duration, SystemTime};
use wasm_bindgen::prelude::*;

// ── shared option marshalling ───────────────────────────────────────────────

/// Parse a memory-model name (`tiny|small|compact|medium|large|huge`).
fn model_of(name: &str) -> Result<bcc::MemoryModel, JsError> {
    Ok(match name {
        "tiny" => bcc::MemoryModel::Tiny,
        "small" => bcc::MemoryModel::Small,
        "compact" => bcc::MemoryModel::Compact,
        "medium" => bcc::MemoryModel::Medium,
        "large" => bcc::MemoryModel::Large,
        "huge" => bcc::MemoryModel::Huge,
        other => return Err(JsError::new(&format!("unknown memory model: {other}"))),
    })
}

/// The OBJ embeds the source mtime; the caller supplies it (Unix seconds) so the
/// output can be made byte-exact against a `faketime`-pinned oracle build.
fn mtime_of(unix_seconds: f64) -> SystemTime {
    let secs = if unix_seconds < 0.0 { 0 } else { unix_seconds as u64 };
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// `["NAME=VALUE", "BARE"]` → `[("NAME","VALUE"), ("BARE","")]` (`-D` defines).
fn defines_of(defines: Vec<String>) -> Vec<(String, String)> {
    defines
        .into_iter()
        .map(|d| match d.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (d, String::new()),
        })
        .collect()
}

fn js_err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

// ── bcc: C source → OBJ / ASM ───────────────────────────────────────────────

/// Compile C source to an OMF object file (`bcc -c`). `filename` is the
/// lowercase source name recorded in the OBJ's `THEADR`; `defines` are
/// `NAME=VALUE` strings.
///
/// # Errors
/// On an unknown `model` name or a compile failure (lex/parse/codegen/assemble).
#[wasm_bindgen]
#[allow(clippy::too_many_arguments)] // flat boundary; the TS wrapper bundles these
pub fn compile(
    source: &str,
    filename: &str,
    model: &str,
    mtime_unix: f64,
    merge_strings: bool,
    unsigned_chars: bool,
    optimize: bool,
    target_186: bool,
    stack_check: bool,
    no_reg_vars: bool,
    defines: Vec<String>,
) -> Result<Vec<u8>, JsError> {
    bcc::build_obj(
        source,
        filename,
        mtime_of(mtime_unix),
        model_of(model)?,
        merge_strings,
        &defines_of(defines),
        unsigned_chars,
        optimize,
        target_186,
        stack_check,
        no_reg_vars,
    )
    .map_err(js_err)
}

/// Compile C source to assembly text (`bcc -S`) — the same pipeline as
/// [`compile`] but stopping before TASM. Returns the `.ASM` source.
///
/// # Errors
/// On an unknown `model` name or a compile failure.
#[wasm_bindgen]
#[allow(clippy::too_many_arguments)]
pub fn compile_asm(
    source: &str,
    filename: &str,
    model: &str,
    mtime_unix: f64,
    merge_strings: bool,
    unsigned_chars: bool,
    optimize: bool,
    target_186: bool,
    stack_check: bool,
    no_reg_vars: bool,
    defines: Vec<String>,
) -> Result<String, JsError> {
    let bytes = bcc::build_asm(
        source,
        filename,
        mtime_of(mtime_unix),
        model_of(model)?,
        merge_strings,
        &defines_of(defines),
        unsigned_chars,
        optimize,
        target_186,
        stack_check,
        no_reg_vars,
    )
    .map_err(js_err)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ── tasm: ASM text → OBJ ────────────────────────────────────────────────────

/// Assemble TASM-syntax assembly to an OMF object file (`tasm`).
///
/// # Errors
/// On an assembly error (bad syntax or an unsupported directive/instruction).
#[wasm_bindgen]
pub fn assemble(source: &str) -> Result<Vec<u8>, JsError> {
    bcc_tasm::assemble(source).map_err(js_err)
}

/// Assemble with an explicit BCC memory-model marker COMENT byte — what `bcc`
/// injects between its `-S` and TASM stages (see `assemble`).
///
/// # Errors
/// On an assembly error (see [`assemble`]).
#[wasm_bindgen]
pub fn assemble_with_model(source: &str, model_marker: u8) -> Result<Vec<u8>, JsError> {
    bcc_tasm::assemble_with_model(source, model_marker).map_err(js_err)
}

// ── tlink: OBJ(s) + LIB(s) → EXE ────────────────────────────────────────────

/// Accumulates objects and libraries, then links them to an MZ `.EXE` (`tlink`).
/// Add inputs in link order, then call [`Linker::link`].
#[wasm_bindgen]
#[derive(Debug, Default)]
pub struct Linker {
    objects: Vec<(String, Vec<u8>)>,
    libraries: Vec<(String, Vec<u8>)>,
}

#[wasm_bindgen]
impl Linker {
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Linker {
        Linker::default()
    }

    /// Add an object file (its name as it would appear on the TLINK command line).
    pub fn add_object(&mut self, name: String, bytes: Vec<u8>) {
        self.objects.push((name, bytes));
    }

    /// Add a library searched to resolve externals.
    pub fn add_library(&mut self, name: String, bytes: Vec<u8>) {
        self.libraries.push((name, bytes));
    }

    /// Link the accumulated inputs to a complete `.EXE` image.
    ///
    /// # Errors
    /// On an unresolved external, a malformed input object/library, or any
    /// other link failure.
    pub fn link(&self) -> Result<Vec<u8>, JsError> {
        bcc_tlink::link_objects(&self.objects, &self.libraries).map_err(js_err)
    }
}

// ── tlib: OBJ(s) → LIB ──────────────────────────────────────────────────────

/// Accumulates objects, then builds an OMF library archive (`tlib`).
#[wasm_bindgen]
#[derive(Debug, Default)]
pub struct Librarian {
    objects: Vec<(String, Vec<u8>)>,
}

#[wasm_bindgen]
impl Librarian {
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Librarian {
        Librarian::default()
    }

    /// Add a member object (its archived name).
    pub fn add_object(&mut self, name: String, bytes: Vec<u8>) {
        self.objects.push((name, bytes));
    }

    /// Build the library. `extended` selects the extended-dictionary format.
    ///
    /// # Errors
    /// On a malformed member object or a dictionary/write failure.
    pub fn build(&self, extended: bool) -> Result<Vec<u8>, JsError> {
        bcc_tlib::build_library(&self.objects, extended).map_err(js_err)
    }
}
