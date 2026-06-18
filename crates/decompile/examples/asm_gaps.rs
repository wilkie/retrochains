//! Sub-triage the `Asm(unlifted)` cluster from `triage.rs`: for every BCC fixture
//! the decompiler declines *and* whose `_TEXT` lift left an opaque [`LoOp::Asm`]
//! run, histogram the leading opcode of each unlifted run. Turns "38% bail on an
//! unlifted instruction" into "opcode 0xNN (mnemonic) is the gap, in N fixtures".
//!
//!   cargo run -p decompile --example asm_gaps -- [fixtures/c] [samples]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bcc::{parse_args, CompileMode};
use decompile::{decompile, lift, CompileOpts, LoOp};

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| "fixtures/c".to_string());
    let samples: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    let mut fixtures = Vec::new();
    collect(Path::new(&root), &mut fixtures);
    fixtures.sort();

    // opcode signature -> (fixtures, total run count)
    let mut clusters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut fixtures_with_gap = 0usize;

    for dir in &fixtures {
        let Some(gaps) = unlifted_opcodes(dir) else { continue };
        if gaps.is_empty() {
            continue;
        }
        fixtures_with_gap += 1;
        let label = label_of(dir, &root);
        // Attribute the fixture to the *first* distinct unlifted opcode it hits
        // (the proximate gap), but list every distinct opcode it contains so a
        // sweep of one opcode shows which fixtures it would unblock.
        for sig in &gaps {
            clusters.entry(sig.clone()).or_default().push(label.clone());
        }
    }

    let mut ranked: Vec<(&String, &Vec<String>)> = clusters.iter().collect();
    ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

    println!("\n=== unlifted-opcode sub-triage ({fixtures_with_gap} fixtures with an Asm gap) ===");
    println!("(a fixture appears under every distinct opcode it contains)\n");
    for (sig, hits) in &ranked {
        println!("{:>5}  {sig}", hits.len());
        for s in hits.iter().take(samples) {
            println!("         {s}");
        }
        if hits.len() > samples {
            println!("         … and {} more", hits.len() - samples);
        }
    }
}

/// The distinct leading-opcode signatures of the opaque `Asm` runs in a fixture's
/// `_TEXT`, but only when the decompiler actually declines it. `None` to skip.
fn unlifted_opcodes(dir: &Path) -> Option<Vec<String>> {
    let src = std::fs::read_to_string(dir.join("HELLO.C")).ok()?;
    let opts = opts_of(&dir.join("invocation.bcc.toml"))?;
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let target = decompile::recompile_text(&src, &opts).ok()?;
        if target.is_empty() || decompile(&target).is_some() {
            return None;
        }
        let mut sigs: Vec<String> = lift(&target)
            .iter()
            .filter_map(|n| match &n.op {
                LoOp::Asm { bytes } => Some(opcode_sig(bytes)),
                _ => None,
            })
            .collect();
        sigs.sort();
        sigs.dedup();
        Some(sigs)
    }));
    res.ok().flatten()
}

/// A short signature for an unlifted run: its first opcode byte plus a coarse
/// mnemonic guess (enough to cluster — not a disassembler). Reports a `0f` two-
/// byte opcode and ModRM /reg sub-opcodes where they disambiguate the mnemonic.
fn opcode_sig(bytes: &[u8]) -> String {
    let b0 = bytes.first().copied().unwrap_or(0);
    let mnem = match b0 {
        0xf6 | 0xf7 => match bytes.get(1).map(|m| (m >> 3) & 7) {
            Some(0) => "test r/m,imm",
            Some(2) => "not",
            Some(3) => "neg",
            Some(4) => "mul",
            Some(5) => "imul",
            Some(6) => "div",
            Some(7) => "idiv",
            _ => "grp3",
        },
        0xd0..=0xd3 => "shift/rotate (grp2)",
        0xc0 | 0xc1 => "shift/rotate imm8 (grp2)",
        0x80 | 0x81 | 0x83 => "alu r/m,imm (grp1)",
        0x23 | 0x21 | 0x25 => "and",
        0x0b | 0x09 | 0x0d => "or",
        0x33 | 0x31 | 0x35 => "xor",
        0x2b | 0x29 | 0x2d => "sub",
        0x03 | 0x01 | 0x05 => "add",
        0x3b | 0x39 | 0x3d => "cmp",
        0x86 | 0x87 => "xchg",
        0x88..=0x8b => "mov r/m",
        0xc6 | 0xc7 => "mov r/m,imm",
        0x8d => "lea",
        0xff => match bytes.get(1).map(|m| (m >> 3) & 7) {
            Some(0) => "inc r/m",
            Some(1) => "dec r/m",
            Some(2) => "call r/m",
            Some(4) => "jmp r/m",
            Some(6) => "push r/m",
            _ => "grp5",
        },
        0xfe => "inc/dec r/m8 (grp4)",
        0x98 => "cbw",
        0x99 => "cwd",
        0x9a => "call far",
        0xe8 => "call",
        0xea => "jmp far",
        0xcb => "retf",
        0x0f => "0f two-byte",
        0x26 | 0x2e | 0x36 | 0x3e => "seg prefix",
        0xf2 | 0xf3 => "rep/repne",
        0xa4 | 0xa5 | 0xaa | 0xab | 0xac | 0xad => "string op",
        0x90 => "nop",
        _ => "other",
    };
    format!("{b0:02x}  {mnem}")
}

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
