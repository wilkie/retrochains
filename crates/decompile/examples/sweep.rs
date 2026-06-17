//! Corpus sweep: run the decompiler over every BCC fixture's `_TEXT` and bucket
//! the round-trip outcome. The decompiler has been built against hand-written
//! probes; this measures where it actually stands on the real corpus and
//! clusters the failures into a frontier.
//!
//! For each fixture under `fixtures/c` that has a `HELLO.C` and an
//! `invocation.bcc.toml`, we:
//!   1. parse the invocation's BCC flags into the matching [`CompileOpts`],
//!   2. compile the source to `_TEXT` (the target bytes),
//!   3. decompile those bytes back to candidate C, and
//!   4. verify the candidate recompiles to the same bytes (same opts).
//!
//! Each fixture lands in one bucket: `match`, `mismatch`, `incomplete`,
//! `cerr` (the recovered C didn't compile), `notext` (no code emitted), or
//! `panic` (the recover/verify crashed). The mismatch/incomplete buckets are
//! the frontier; we print a sample of each.
//!
//! Run from the repo root:
//!   cargo run -p decompile --example sweep -- [fixtures/c] [sample-per-bucket]

use std::path::{Path, PathBuf};

use bcc::{parse_args, CompileMode};
use decompile::{decompile, verify, CompileOpts, Outcome};

#[derive(Default)]
struct Tally {
    matched: Vec<String>,
    mismatch: Vec<String>,
    incomplete: Vec<String>,
    cerr: Vec<String>,
    notext: Vec<String>,
    panic: Vec<String>,
    skipped: Vec<String>,
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| "fixtures/c".to_string());
    let sample: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);

    let mut fixtures = Vec::new();
    collect(Path::new(&root), &mut fixtures);
    fixtures.sort();

    let mut t = Tally::default();
    for dir in &fixtures {
        let label = label_of(dir, &root);
        match run_one(dir) {
            Bucket::Match => t.matched.push(label),
            Bucket::Mismatch => t.mismatch.push(label),
            Bucket::Incomplete => t.incomplete.push(label),
            Bucket::Cerr => t.cerr.push(label),
            Bucket::NoText => t.notext.push(label),
            Bucket::Panic => t.panic.push(label),
            Bucket::Skip => t.skipped.push(label),
        }
    }

    let considered = fixtures.len() - t.skipped.len();
    println!("\n=== decompiler corpus sweep ({considered} BCC fixtures) ===");
    report("MATCH       ", &t.matched, considered, sample, false);
    report("incomplete  ", &t.incomplete, considered, sample, true);
    report("MISMATCH    ", &t.mismatch, considered, sample, true);
    report("cerr        ", &t.cerr, considered, sample, true);
    report("notext      ", &t.notext, considered, sample, true);
    report("PANIC       ", &t.panic, considered, sample, true);
    if !t.skipped.is_empty() {
        println!("(skipped {} fixtures with unparseable invocations)", t.skipped.len());
    }
}

enum Bucket {
    Match,
    Mismatch,
    Incomplete,
    Cerr,
    NoText,
    Panic,
    Skip,
}

fn run_one(dir: &Path) -> Bucket {
    let Ok(src) = std::fs::read_to_string(dir.join("HELLO.C")) else {
        return Bucket::Skip;
    };
    let Some(opts) = opts_of(&dir.join("invocation.bcc.toml")) else {
        return Bucket::Skip;
    };
    // The whole round trip is wrapped so a recover/verify panic is just a
    // bucket, not an aborted sweep.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let target = decompile::recompile_text(&src, &opts).ok()?;
        if target.is_empty() {
            return Some(Bucket::NoText);
        }
        let Some(candidate) = decompile(&target) else {
            return Some(Bucket::Incomplete);
        };
        Some(match verify(&candidate, &opts, &target) {
            Ok(Outcome::Match) => Bucket::Match,
            Ok(Outcome::Mismatch(_)) => Bucket::Mismatch,
            Err(_) => Bucket::Cerr,
        })
    }));
    match outcome {
        Ok(Some(b)) => b,
        Ok(None) => Bucket::Skip, // source didn't compile under our bcc — not a decompiler signal
        Err(_) => Bucket::Panic,
    }
}

/// Map a fixture's `invocation.bcc.toml` flags to the decompiler's
/// [`CompileOpts`], reusing the real `bcc` argument parser so the sweep
/// compiles each fixture exactly as the byte-exact harness does.
fn opts_of(toml: &Path) -> Option<CompileOpts> {
    let text = std::fs::read_to_string(toml).ok()?;
    let argv = toml_args(&text)?;
    let parsed = parse_args(&argv).ok()?;
    // Only `-S`/`-c` (compile-only) fixtures have a `_TEXT` to recover; a link
    // invocation isn't a single-function target.
    if !matches!(parsed.mode, CompileMode::Assembly | CompileMode::Object) {
        return None;
    }
    Some(CompileOpts {
        model: parsed.memory_model,
        merge_strings: parsed.merge_strings,
        unsigned_chars: parsed.unsigned_chars,
        optimize: parsed.optimize,
        target_186: parsed.target_186,
        stack_check: parsed.stack_check,
        // The recovery emits plain `int` locals; BCC's deterministic register
        // allocation re-derives reg vars, so we leave them enabled (the corpus
        // default) rather than forcing `-r-`.
        no_reg_vars: parsed.no_reg_vars,
        defines: parsed.defines,
    })
}

/// Pull the `args = [...]` array out of an `invocation.*.toml` without a full
/// TOML parser: split the bracketed body on quotes and keep the odd tokens.
fn toml_args(text: &str) -> Option<Vec<String>> {
    let line = text.lines().find(|l| l.trim_start().starts_with("args"))?;
    let open = line.find('[')?;
    let close = line.rfind(']')?;
    let body = &line[open + 1..close];
    Some(body.split('"').skip(1).step_by(2).map(str::to_string).collect())
}

#[allow(clippy::cast_precision_loss)] // bucket counts are tiny; f64 is exact here
fn report(name: &str, items: &[String], total: usize, sample: usize, show: bool) {
    let pct = if total == 0 { 0.0 } else { 100.0 * items.len() as f64 / total as f64 };
    println!("{name} {:>5}  {pct:5.1}%", items.len());
    if show && !items.is_empty() {
        for s in items.iter().take(sample) {
            println!("    {s}");
        }
        if items.len() > sample {
            println!("    … and {} more", items.len() - sample);
        }
    }
}

fn label_of(dir: &Path, root: &str) -> String {
    dir.strip_prefix(root).unwrap_or(dir).display().to_string()
}

/// Recursively collect every directory that holds a BCC fixture.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut is_fixture = false;
    let mut children = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            children.push(p);
        } else if p.file_name().is_some_and(|n| n == "invocation.bcc.toml") {
            is_fixture = true;
        }
    }
    if is_fixture {
        out.push(dir.to_path_buf());
    }
    for c in children {
        collect(&c, out);
    }
}
