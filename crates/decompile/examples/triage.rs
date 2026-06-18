//! Triage the decompiler's `incomplete` bucket: for every BCC fixture the
//! decompiler declines (`decompile` -> `None`), record *why* recovery bailed and
//! cluster the corpus by that reason. The reason is [`Function::bail_reason`] —
//! the signature of the op that defeated the straight-line fold (`Bin:Mul`,
//! `Load:deref`, `Asm(unlifted)`, …) or a structural tag (`structure:…`,
//! `long-high-slot-double-count`, `dangling-array`). A whole-program decline that
//! isn't any one function's fault (file-scope globals, no recovered function) is
//! tagged `program:*`.
//!
//! This turns "40% incomplete" into a ranked work-list. Run from the repo root:
//!   cargo run -p decompile --example triage -- [fixtures/c] [samples-per-cluster]

// A diagnostic example: percentages are display-only, and the
// skip/recovered/incomplete trichotomy is naturally an `Option<Option<_>>`.
#![allow(clippy::cast_precision_loss, clippy::option_option)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bcc::{parse_args, CompileMode};
use decompile::{decompile, recover_program, CompileOpts, Var};

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| "fixtures/c".to_string());
    let samples: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(6);

    let mut fixtures = Vec::new();
    collect(Path::new(&root), &mut fixtures);
    fixtures.sort();

    // reason -> fixtures that bail for it
    let mut clusters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut incomplete = 0usize;
    let mut considered = 0usize;

    for dir in &fixtures {
        let Some(reason) = triage_one(dir) else { continue };
        considered += 1;
        let Some(reason) = reason else { continue }; // recovered fine — not incomplete
        incomplete += 1;
        clusters.entry(reason).or_default().push(label_of(dir, &root));
    }

    // Rank clusters by size.
    let mut ranked: Vec<(&String, &Vec<String>)> = clusters.iter().collect();
    ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

    println!("\n=== incomplete-bucket triage ({incomplete} incomplete / {considered} fixtures) ===\n");
    let pct = |n: usize| if incomplete == 0 { 0.0 } else { 100.0 * n as f64 / incomplete as f64 };
    for (reason, hits) in &ranked {
        println!("{:>5}  {:5.1}%  {reason}", hits.len(), pct(hits.len()));
        for s in hits.iter().take(samples) {
            println!("           {s}");
        }
        if hits.len() > samples {
            println!("           … and {} more", hits.len() - samples);
        }
    }
}

/// `None` if the fixture isn't a recoverable BCC target (skip); `Some(None)` if it
/// decompiles fully; `Some(Some(reason))` if it's incomplete, with the reason.
fn triage_one(dir: &Path) -> Option<Option<String>> {
    let src = std::fs::read_to_string(dir.join("HELLO.C")).ok()?;
    let opts = opts_of(&dir.join("invocation.bcc.toml"))?;
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let target = decompile::recompile_text(&src, &opts).ok()?;
        if target.is_empty() {
            return None; // notext — not an incompleteness signal
        }
        if decompile(&target).is_some() {
            return Some(None); // recovered
        }
        // Incomplete: blame the first function recovery declined, else the program.
        let funcs = recover_program(&target);
        let reason = funcs.iter().find(|f| !f.complete).map_or_else(
            || {
                // Every function recovered, yet `decompile` declined: a program
                // shape `emit_program` can't frame.
                if funcs.is_empty() {
                    "program:no-function".to_string()
                } else if funcs.iter().any(|f| f.vars.iter().any(|v| matches!(v, Var::Global(_)))) {
                    "program:globals".to_string()
                } else {
                    "program:other".to_string()
                }
            },
            |f| f.bail_reason.clone().unwrap_or_else(|| "unknown".to_string()),
        );
        Some(Some(reason))
    }));
    match res {
        Ok(inner) => inner,
        Err(_) => Some(Some("panic".to_string())),
    }
}

/// See `sweep.rs` — map a fixture's invocation to the decompiler's `CompileOpts`.
fn opts_of(toml: &Path) -> Option<CompileOpts> {
    let text = std::fs::read_to_string(toml).ok()?;
    let argv = toml_args(&text)?;
    let parsed = parse_args(&argv).ok()?;
    if !matches!(parsed.mode, CompileMode::Assembly | CompileMode::Object) {
        return None;
    }
    Some(CompileOpts {
        model: parsed.memory_model.into(),
        merge_strings: parsed.merge_strings,
        unsigned_chars: parsed.unsigned_chars,
        optimize: parsed.optimize,
        target_186: parsed.target_186,
        stack_check: parsed.stack_check,
        no_reg_vars: parsed.no_reg_vars,
        defines: parsed.defines,
    })
}

fn toml_args(text: &str) -> Option<Vec<String>> {
    let line = text.lines().find(|l| l.trim_start().starts_with("args"))?;
    let open = line.find('[')?;
    let close = line.rfind(']')?;
    let body = &line[open + 1..close];
    Some(body.split('"').skip(1).step_by(2).map(str::to_string).collect())
}

fn label_of(dir: &Path, root: &str) -> String {
    dir.strip_prefix(root).unwrap_or(dir).display().to_string()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut is_fixture = false;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.file_name().is_some_and(|n| n == "HELLO.C") {
            is_fixture = true;
        }
    }
    if is_fixture {
        out.push(dir.to_path_buf());
    }
    for sub in subdirs {
        collect(&sub, out);
    }
}
