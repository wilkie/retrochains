//! The recompile-verify harness — §8 of `specs/decompiler/IR.md`.
//!
//! The decompiler's correctness contract is *decidable*: recovered C is correct
//! iff recompiling it with our byte-exact [`bcc`] reproduces the function's
//! `_TEXT` bytes. This module is that oracle. Given a candidate C string and the
//! target bytes it should reproduce, it
//!
//! 1. compiles the candidate (the same `-c` path `bcc` drives: source → ASM →
//!    TASM → OMF),
//! 2. pulls the first CODE-class segment back out of the OBJ, and
//! 3. diffs it against the target, reporting the **first diverging byte offset**.
//!
//! That offset is what makes the contract a *repair signal* rather than a bare
//! pass/fail: the lift records a byte range on every IR node (the spec's
//! provenance), so the harness's offset maps back to the node that produced the
//! wrong bytes. Mapping the offset to a node is the lift's job; localizing it is
//! this module's.
//!
//! The harness needs nothing from the IR — only `(candidate C, target bytes)` —
//! so it can be exercised directly on hand-written C long before the lift exists
//! (see the tests below, which round-trip C through it).

/// Memory model for the recompile — mirrors BCC's `-m*` knobs. **Owned by this
/// crate** (not re-exported from `bcc`) so the decompiler's option type carries
/// no compiler dependency: a recompiler backend maps it to its own model type
/// (see the `bcc` feature's `From<MemoryModel> for bcc::MemoryModel`). This is
/// what lets the decompiler core link *no* compiler — verification is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryModel {
    Tiny,
    Small,
    Compact,
    Medium,
    Large,
    Huge,
}

/// How to compile the candidate C — mirrors the knobs [`bcc::build_obj`] takes.
///
/// [`Default`] is BCC's out-of-the-box configuration: small model, signed
/// `char`, no optimization, 8086 target, no stack check, register variables on.
/// Recovering a function means matching the model and flags the *original* was
/// built with, so the recovered C only round-trips under the same options.
#[derive(Debug, Clone)]
// The bools mirror `bcc::build_obj`'s flag arguments one-for-one; grouping them
// behind an enum would just obscure that correspondence.
#[allow(clippy::struct_excessive_bools)]
pub struct CompileOpts {
    /// Memory model (`-mt/-ms/-mc/-mm/-ml/-mh`). Decides near vs far code/data
    /// and so the call/return and pointer encodings.
    pub model: MemoryModel,
    /// Merge duplicate string literals (`-d`).
    pub merge_strings: bool,
    /// Treat `char` as unsigned (`-K`). Flips `cbw` (signed) vs zero-extend.
    pub unsigned_chars: bool,
    /// Enable the optimizer (`-O`). BCC 2.0 barely optimizes; off by default.
    pub optimize: bool,
    /// Target the 80186/286 (`-1`) — `enter`/`leave`, `push imm8`, `shl r,imm`.
    pub target_186: bool,
    /// Emit stack-overflow checks (`-N`).
    pub stack_check: bool,
    /// Suppress SI/DI register-variable allocation (`-r-`).
    pub no_reg_vars: bool,
    /// Preprocessor defines (`-Dname=value`).
    pub defines: Vec<(String, String)>,
}

impl Default for CompileOpts {
    fn default() -> Self {
        Self {
            model: MemoryModel::Small,
            merge_strings: false,
            unsigned_chars: false,
            optimize: false,
            target_186: false,
            stack_check: false,
            no_reg_vars: false,
            defines: Vec::new(),
        }
    }
}

impl CompileOpts {
    /// Default options for a given memory model — the common case.
    #[must_use]
    pub fn model(model: MemoryModel) -> Self {
        Self { model, ..Self::default() }
    }
}

/// A failure that isn't a byte mismatch — the candidate C didn't compile.
///
/// Distinct from [`Outcome::Mismatch`]: a compile error means the recovered C is
/// *malformed* (a structuring or emission bug), whereas a mismatch means it's
/// well-formed but encodes to different bytes (a wrong operator, missed
/// promotion, …). The repair loop reacts to the two differently.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// The candidate C failed to lex, parse, or assemble.
    #[error("candidate C did not compile: {0}")]
    Compile(String),
}

/// The result of checking a candidate against its target bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The recompiled `_TEXT` is byte-for-byte the target. The function is done.
    Match,
    /// The bytes diverge; [`Diff`] carries both sides and the first offset.
    Mismatch(Diff),
}

impl Outcome {
    /// `true` iff the candidate reproduced the target exactly.
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Outcome::Match)
    }
}

/// A byte-level divergence between recompiled and target `_TEXT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// The `_TEXT` the candidate C recompiled to.
    pub recovered: Vec<u8>,
    /// The bytes the candidate was supposed to reproduce.
    pub target: Vec<u8>,
    /// Offset of the first diverging byte. When one side is a strict prefix of
    /// the other (equal up to the shorter length, but lengths differ) this is
    /// the shorter length — the point the shorter run runs out.
    pub first_diff: usize,
}

impl Diff {
    /// Render a side-by-side hex window around [`first_diff`](Diff::first_diff)
    /// for debugging — `radius` bytes of context on each side.
    #[must_use]
    pub fn window(&self, radius: usize) -> String {
        let start = self.first_diff.saturating_sub(radius);
        let end = self
            .first_diff
            .saturating_add(radius)
            .saturating_add(1)
            .min(self.recovered.len().max(self.target.len()));
        let hex = |b: &[u8]| {
            (start..end)
                .map(|i| b.get(i).map_or("..".to_string(), |x| format!("{x:02x}")))
                .collect::<Vec<_>>()
                .join(" ")
        };
        format!(
            "@{:#x} (first diff)\n  target:    {}\n  recovered: {}",
            self.first_diff,
            hex(&self.target),
            hex(&self.recovered),
        )
    }
}

// ── Injected-backend core (links no compiler) ──────────────────────────────
//
// `verify_with`/`render_idiomatic_with` take the recompiler as a parameter — a
// `Fn(&str, &CompileOpts) -> Result<Vec<u8>, HarnessError>` that turns candidate
// C into the comparable `_TEXT` bytes. Inject the byte-exact compiler for the
// toolchain that produced the target. The decompiler core itself depends on no
// compiler; the bundled `bcc` backend below is one (default) implementation, and
// a multi-compiler host supplies whichever backend `classify` points at.

/// Verify a candidate reproduces `target` `_TEXT`, recompiling via `recompile`.
///
/// # Errors
/// Propagates [`HarnessError::Compile`] from the backend if the candidate C
/// doesn't compile — distinct from a byte [`Outcome::Mismatch`].
pub fn verify_with(
    candidate_c: &str,
    opts: &CompileOpts,
    target: &[u8],
    recompile: impl Fn(&str, &CompileOpts) -> Result<Vec<u8>, HarnessError>,
) -> Result<Outcome, HarnessError> {
    let recovered = recompile(candidate_c, opts)?;
    Ok(compare(recovered, target.to_vec()))
}

/// Render the recovery as the most *idiomatic* C that still recompiles to `code`
/// byte-for-byte, recompiling via `recompile` (see [`verify_with`]).
///
/// The recovery is form-neutral; this tries each [`AccessForm`] in preference
/// order (subscript, then pointer arithmetic) and returns the first whose bytes
/// match. The recompile is the *oracle*: a form that doesn't reproduce the bytes
/// — or that the backend can't even build (a hard gap may surface as a panic,
/// caught and treated as "rejected") — is skipped.
///
/// Returns `None` if the function isn't fully recovered, or if no form matches.
#[must_use]
pub fn render_idiomatic_with(
    code: &[u8],
    opts: &CompileOpts,
    recompile: impl Fn(&str, &CompileOpts) -> Result<Vec<u8>, HarnessError>,
) -> Option<String> {
    let func = crate::hi_ir::recover(code);
    // Two presentation axes, each gated by the recompile oracle. Initializer
    // folding (`<type> v = expr;`) is preferred over split decl-then-store, and
    // within that the subscript access form over pointer arithmetic — so the
    // first match is the most idiomatic rendering that still reproduces the bytes.
    // Folding here is `Aggressive`: the oracle below rejects any fold that isn't
    // byte-exact, so it can attempt globals/derefs/narrowing the unverified path
    // declines.
    for mode in [crate::emit::FoldMode::Aggressive, crate::emit::FoldMode::None] {
        for form in [crate::AccessForm::Subscript, crate::AccessForm::PointerArith] {
            let Some(candidate) = crate::emit::to_c_full(&func, form, mode) else {
                return None; // incomplete — no form will render it
            };
            // The backend is the oracle. Tolerate a build that panics on an
            // unsupported shape (a known compiler gap): a crash is just a rejection.
            let matched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                matches!(verify_with(&candidate, opts, code, &recompile), Ok(Outcome::Match))
            }))
            .unwrap_or(false);
            if matched {
                return Some(candidate);
            }
        }
    }
    None
}

// ── Bundled `bcc` backend (default feature) ────────────────────────────────
//
// The default recompiler: our byte-exact BCC. Behind the `bcc` feature so a
// compiler-free build (the universal-decompiler wasm module) can drop it and use
// `verify_with`/`render_idiomatic_with` with an injected backend instead.

#[cfg(feature = "bcc")]
impl From<MemoryModel> for bcc::MemoryModel {
    fn from(m: MemoryModel) -> Self {
        match m {
            MemoryModel::Tiny => bcc::MemoryModel::Tiny,
            MemoryModel::Small => bcc::MemoryModel::Small,
            MemoryModel::Compact => bcc::MemoryModel::Compact,
            MemoryModel::Medium => bcc::MemoryModel::Medium,
            MemoryModel::Large => bcc::MemoryModel::Large,
            MemoryModel::Huge => bcc::MemoryModel::Huge,
        }
    }
}

#[cfg(feature = "bcc")]
impl From<bcc::MemoryModel> for MemoryModel {
    fn from(m: bcc::MemoryModel) -> Self {
        match m {
            bcc::MemoryModel::Tiny => MemoryModel::Tiny,
            bcc::MemoryModel::Small => MemoryModel::Small,
            bcc::MemoryModel::Compact => MemoryModel::Compact,
            bcc::MemoryModel::Medium => MemoryModel::Medium,
            bcc::MemoryModel::Large => MemoryModel::Large,
            bcc::MemoryModel::Huge => MemoryModel::Huge,
        }
    }
}

/// Compile candidate C with the bundled [`bcc`] and return its `_TEXT` (first
/// CODE-class segment) bytes — the default recompiler backend.
///
/// # Errors
/// [`HarnessError::Compile`] if the candidate fails to lex, parse, or assemble.
#[cfg(feature = "bcc")]
pub fn recompile_text(candidate_c: &str, opts: &CompileOpts) -> Result<Vec<u8>, HarnessError> {
    let obj = bcc::build_obj(
        candidate_c,
        "a.c",
        std::time::SystemTime::UNIX_EPOCH,
        opts.model.into(),
        opts.merge_strings,
        &opts.defines,
        opts.unsigned_chars,
        opts.optimize,
        opts.target_186,
        opts.stack_check,
        opts.no_reg_vars,
    )
    .map_err(|e| HarnessError::Compile(e.to_string()))?;
    Ok(fingerprint::idioms::code_of_obj(&obj))
}

/// Verify a candidate reproduces `target` `_TEXT`, recompiling with the bundled
/// [`bcc`] backend — [`verify_with`] wired to [`recompile_text`].
///
/// # Errors
/// [`HarnessError::Compile`] if the candidate C doesn't compile.
#[cfg(feature = "bcc")]
pub fn verify(
    candidate_c: &str,
    opts: &CompileOpts,
    target: &[u8],
) -> Result<Outcome, HarnessError> {
    verify_with(candidate_c, opts, target, recompile_text)
}

/// Render the most idiomatic byte-exact C, recompiling with the bundled [`bcc`]
/// backend — [`render_idiomatic_with`] wired to [`recompile_text`].
#[cfg(feature = "bcc")]
#[must_use]
pub fn render_idiomatic(code: &[u8], opts: &CompileOpts) -> Option<String> {
    render_idiomatic_with(code, opts, recompile_text)
}

/// Diff two `_TEXT` byte runs. Pure, so it's testable without compiling.
fn compare(recovered: Vec<u8>, target: Vec<u8>) -> Outcome {
    if recovered == target {
        return Outcome::Match;
    }
    let first_diff = recovered
        .iter()
        .zip(target.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| recovered.len().min(target.len()));
    Outcome::Mismatch(Diff { recovered, target, first_diff })
}

#[cfg(all(test, feature = "bcc"))]
mod tests {
    use super::*;

    /// The C the harness round-trips below. A bare function whose body is a
    /// single `return` keeps the `_TEXT` short and stable across small model.
    const RETURN_ZERO: &str = "int f() { return 0; }\n";

    #[test]
    fn identical_bytes_match() {
        // compare() is the diff core; equal runs are a Match regardless of how
        // they were produced.
        assert_eq!(compare(vec![1, 2, 3], vec![1, 2, 3]), Outcome::Match);
    }

    /// The presentation seam: `render_idiomatic` re-renders the form-neutral
    /// recovery under each access form and returns the first that still
    /// recompiles byte-exact. With the compiler as the oracle, an offset deref
    /// comes back as the idiomatic subscript `p[K]` (vs the `*(p+K)` default),
    /// and every rendering is verified — so the output is always faithful.
    #[test]
    fn render_idiomatic_prefers_the_subscript_form() {
        let opts = CompileOpts { no_reg_vars: true, ..CompileOpts::default() };
        for (src, want) in [
            ("int f(int *p) { return p[2]; }\n", "p1[2]"),
            ("void f(int *p) { p[2] = 5; }\n", "p1[2] = 5"),
            ("void f(int *p, int v) { p[2] = v; }\n", "p1[2] = p2"),
        ] {
            let code = recompile_text(src, &opts).expect("the sample compiles");
            let c = render_idiomatic(&code, &opts).expect("recovered");
            assert!(c.contains(want), "expected {want:?} in idiomatic form:\n{c}");
            assert!(
                verify(&c, &opts, &code).expect("compiles").is_match(),
                "the idiomatic form must still recompile byte-exact:\n{c}",
            );
        }
    }

    #[test]
    fn render_idiomatic_leaves_a_plain_deref_alone() {
        // `*p` (no offset) has no subscript spelling worth choosing — it stays
        // `*p` under either form.
        let opts = CompileOpts { no_reg_vars: true, ..CompileOpts::default() };
        let code = recompile_text("int f(int *p) { return *p; }\n", &opts).unwrap();
        let c = render_idiomatic(&code, &opts).unwrap();
        assert!(c.contains("*p1") && !c.contains('['), "plain deref stays *p:\n{c}");
    }

    #[test]
    fn first_diff_offset_is_localized() {
        let Outcome::Mismatch(d) = compare(vec![1, 2, 9, 4], vec![1, 2, 3, 4]) else {
            panic!("expected a mismatch");
        };
        assert_eq!(d.first_diff, 2);
    }

    #[test]
    fn prefix_divergence_points_at_the_shorter_end() {
        // Equal up to the shorter length but lengths differ → first_diff is
        // where the short run runs out.
        let Outcome::Mismatch(d) = compare(vec![1, 2], vec![1, 2, 3]) else {
            panic!("expected a mismatch");
        };
        assert_eq!(d.first_diff, 2);
    }

    #[test]
    fn recovered_c_recompiles_to_itself() {
        // The contract in miniature: compile C, then verify that *same* C against
        // the bytes it produced — must Match. This exercises the whole left half
        // (source → OBJ → _TEXT) plus the diff.
        let opts = CompileOpts::default();
        let target = recompile_text(RETURN_ZERO, &opts).expect("compiles");
        assert!(!target.is_empty(), "return 0 must emit some _TEXT");
        assert!(verify(RETURN_ZERO, &opts, &target).expect("compiles").is_match());
    }

    #[test]
    fn wrong_constant_is_a_localized_mismatch() {
        // `return 1` and `return 2` both lower to `mov ax,imm` — same instruction
        // shape, differing only in the immediate byte. The harness must localize
        // the divergence to that byte (a strict interior diff, not a length
        // runout), which is what makes the offset a repair signal. (Contrast
        // `return 0`, which BCC special-cases to `xor ax,ax` — a *different*
        // length — exactly the BccZeroAx idiom the recognizer keys on.)
        let opts = CompileOpts::default();
        let one = recompile_text("int f() { return 1; }\n", &opts).expect("compiles");
        let outcome = verify("int f() { return 2; }\n", &opts, &one).expect("compiles");
        let Outcome::Mismatch(d) = outcome else {
            panic!("return 2 must not reproduce return 1's bytes");
        };
        assert_eq!(d.recovered.len(), d.target.len(), "same instruction shape");
        assert!(d.first_diff < one.len(), "diff must land inside the code");
        // Only the immediate differs, so target and recovered agree everywhere
        // except that one offset.
        let diffs = d.recovered.iter().zip(&d.target).filter(|(a, b)| a != b).count();
        assert_eq!(diffs, 1, "exactly the immediate byte differs");
    }

    #[test]
    fn malformed_c_is_a_compile_error_not_a_mismatch() {
        let opts = CompileOpts::default();
        let err = verify("int f() { return", &opts, &[]).unwrap_err();
        let HarnessError::Compile(_) = err;
    }
}
