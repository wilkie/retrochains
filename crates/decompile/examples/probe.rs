//! One-off: show what the decompiler does with a fixture's `_TEXT`, or with an
//! inline source. Dumps the lifted ops, the recovered C, and the verdict.
//!   cargo run -p decompile --example probe -- fixtures/c/<area>/<sub>/<dir>
//!   cargo run -p decompile --example probe -- -e 'int f(int x){ x++; return x; }'
use decompile::{decompile, lift, recompile_text, verify, CompileOpts, LoOp, Outcome};
use std::path::Path;
fn main() {
    let mut a = std::env::args().skip(1);
    let first = a.next().expect("a fixture dir or -e <source>");
    let src = if first == "-e" {
        a.next().expect("source after -e")
    } else {
        std::fs::read_to_string(Path::new(&first).join("HELLO.C")).unwrap()
    };
    let opts = CompileOpts { no_reg_vars: false, ..CompileOpts::default() };
    let target = recompile_text(&src, &opts).unwrap();
    println!("--- source ---\n{src}");
    println!("target _TEXT: {} bytes", target.len());
    println!("--- lift ---");
    for i in lift(&target) {
        let off = i.span.start;
        match &i.op {
            LoOp::Asm { bytes } => println!("@{off:<3} Asm {bytes:02x?}"),
            LoOp::IndirectJump { disp } => println!("@{off:<3} IndJmp {disp}"),
            op => println!("@{off:<3} {op:?}"),
        }
    }
    match decompile(&target) {
        None => println!("recover: INCOMPLETE"),
        Some(c) => {
            println!("--- recovered candidate ---\n{c}");
            match verify(&c, &opts, &target) {
                Ok(Outcome::Match) => println!("verdict: MATCH"),
                Ok(Outcome::Mismatch(d)) => println!(
                    "verdict: MISMATCH at byte {} (recovered {} vs target {} bytes)",
                    d.first_diff,
                    d.recovered.len(),
                    d.target.len()
                ),
                Err(e) => println!("verdict: CERR {e}"),
            }
        }
    }
}
