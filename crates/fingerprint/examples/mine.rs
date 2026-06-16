//! Idiom gap-miner: compile the BCC fixture corpus with the byte-exact compiler,
//! run the idiom recognizer over each `_TEXT`, and rank the *unrecognized* byte
//! runs by frequency. The top entries are the highest-value idioms to add next,
//! and the coverage figure tracks the catalog's progress.
//!
//! Run from the workspace root: `cargo run -p fingerprint --example mine`.

// A one-off analysis tool: the coverage ratio and the `.c` extension check don't
// warrant the pedantic ceremony.
#![allow(clippy::cast_precision_loss, clippy::case_sensitive_file_extension_comparisons)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bcc::{build_obj, MemoryModel};
use fingerprint::idioms::{code_of_obj, recognize};

fn main() {
    let fixtures = workspace_root().join("fixtures/c");
    let mut dirs = Vec::new();
    collect_fixture_dirs(&fixtures, &mut dirs);
    dirs.sort();

    let mut total_code = 0usize;
    let mut total_matched = 0usize;
    let (mut compiled, mut skipped) = (0usize, 0usize);
    // Leading 1- and 2-byte opcodes of each unrecognized run.
    let mut opcode: HashMap<u8, usize> = HashMap::new();
    let mut prefix: HashMap<[u8; 2], usize> = HashMap::new();

    for dir in &dirs {
        let Some((args, source_path)) = read_invocation(dir) else { continue };
        let Ok(source) = fs::read_to_string(&source_path) else { continue };
        let model = model_of(&args);
        let unsigned = args.iter().any(|a| a == "-K");
        let target_186 = args.iter().any(|a| a == "-1");
        let lower = source_path.file_name().and_then(|s| s.to_str()).unwrap_or("a.c").to_ascii_lowercase();
        let obj = match build_obj(
            &source, &lower, SystemTime::UNIX_EPOCH, model, false, &[], unsigned, false, target_186,
            false, false,
        ) {
            Ok(b) => b,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let code = code_of_obj(&obj);
        if code.is_empty() {
            continue;
        }
        compiled += 1;

        let matches = recognize(&code);
        total_code += code.len();
        total_matched += matches.iter().map(|m| m.len).sum::<usize>();

        let mut covered = vec![false; code.len()];
        for m in &matches {
            for b in covered.iter_mut().skip(m.offset).take(m.len) {
                *b = true;
            }
        }
        let mut i = 0;
        while i < code.len() {
            if covered[i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < code.len() && !covered[i] {
                i += 1;
            }
            *opcode.entry(code[start]).or_default() += 1;
            if i - start >= 2 {
                *prefix.entry([code[start], code[start + 1]]).or_default() += 1;
            }
        }
    }

    let pct = if total_code == 0 { 0.0 } else { 100.0 * total_matched as f64 / total_code as f64 };
    println!("compiled {compiled} fixtures ({skipped} skipped), {total_code} code bytes");
    println!("idiom coverage: {pct:.1}%  ({total_matched}/{total_code} bytes recognized)\n");

    println!("top unrecognized opcodes (leading byte of a gap run):");
    for (op, n) in top(&opcode, 12) {
        println!("  {op:02x}  ×{n:<5} {}", opcode_hint(op));
    }
    println!("\ntop unrecognized 2-byte prefixes:");
    for (p, n) in top(&prefix, 12) {
        println!("  {:02x} {:02x}  ×{n}", p[0], p[1]);
    }
}

fn top<K: Copy>(counts: &HashMap<K, usize>, n: usize) -> Vec<(K, usize)> {
    let mut v: Vec<(K, usize)> = counts.iter().map(|(&k, &c)| (k, c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(n);
    v
}

/// A coarse hint for the most common BCC opcodes, to read the histogram.
fn opcode_hint(op: u8) -> &'static str {
    match op {
        0x8b => "mov r16, r/m16  (load)",
        0x89 => "mov r/m16, r16  (store)",
        0xa1 => "mov ax, [mem]",
        0xa3 => "mov [mem], ax",
        0x8a => "mov r8, r/m8",
        0x88 => "mov r/m8, r8",
        0x03 => "add r16, r/m16",
        0x2b => "sub r16, r/m16",
        0x3b => "cmp r16, r/m16",
        0x40..=0x47 => "inc r16",
        0x48..=0x4f => "dec r16",
        0x50..=0x57 => "push r16",
        0x58..=0x5f => "pop r16",
        0x72 | 0x73 | 0x74 | 0x75 | 0x7c..=0x7f => "jcc (conditional jump)",
        0xc7 => "mov r/m16, imm16",
        0xc6 => "mov r/m8, imm8",
        0xf7 => "grp3 (imul/idiv/...)",
        0xd1 | 0xd3 => "shift/rotate",
        _ => "",
    }
}

fn model_of(args: &[String]) -> MemoryModel {
    for a in args {
        match a.as_str() {
            "-mt" => return MemoryModel::Tiny,
            "-ms" => return MemoryModel::Small,
            "-mc" => return MemoryModel::Compact,
            "-mm" => return MemoryModel::Medium,
            "-ml" => return MemoryModel::Large,
            "-mh" => return MemoryModel::Huge,
            _ => {}
        }
    }
    MemoryModel::Small
}

/// Read a fixture's BCC invocation: the `args` array and the `.C` source path.
fn read_invocation(dir: &Path) -> Option<(Vec<String>, PathBuf)> {
    let text = fs::read_to_string(dir.join("invocation.bcc.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let args: Vec<String> = value
        .get("args")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let source = args.iter().find(|a| {
        let l = a.to_ascii_lowercase();
        l.ends_with(".c")
    })?;
    Some((args.clone(), dir.join(source)))
}

fn collect_fixture_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.join("invocation.bcc.toml").exists() {
                out.push(p);
            } else {
                collect_fixture_dirs(&p, out);
            }
        }
    }
}

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}
