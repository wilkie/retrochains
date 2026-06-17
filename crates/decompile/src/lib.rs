//! The Borland C++ 2.0 decompiler.
//!
//! Reads a binary back into compiler-accurate C — C source that, recompiled
//! with our byte-exact [`bcc`], reproduces the original bytes. The design lives
//! in [`specs/decompiler/IR.md`](../../../specs/decompiler/IR.md); this crate is
//! its implementation.
//!
//! The first piece built is the **recompile-verify harness** ([`verify`]) — the
//! engine for the spec's §8 correctness contract. It's deliberately decoupled
//! from the IR: it only needs a candidate C string and the target bytes, so the
//! lift can be developed against a concrete pass/fail (and a localizable diff)
//! before any of Lo-IR or Hi-IR exists.

pub mod emit;
pub mod hi_ir;
pub mod lo_ir;
pub mod verify;

pub use emit::{decompile, to_c, to_c_with_form, AccessForm};
pub use hi_ir::{recover, ArrayElem, ArraySpec, Function, Var};
pub use lo_ir::{lift, LoInsn, LoOp};
pub use verify::{
    recompile_text, render_idiomatic, verify, CompileOpts, Diff, HarnessError, Outcome,
};
