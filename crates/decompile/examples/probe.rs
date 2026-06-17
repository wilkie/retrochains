//! One-off: show what the decompiler does with a single fixture's `_TEXT`.
//! `cargo run -p decompile --example probe -- fixtures/c/<area>/<sub>/<dir>`
use decompile::{decompile, recompile_text, verify, CompileOpts, Outcome};
use std::path::Path;
fn main() {
    let dir = std::env::args().nth(1).expect("fixture dir");
    let dir = Path::new(&dir);
    let src = std::fs::read_to_string(dir.join("HELLO.C")).unwrap();
    let opts = CompileOpts { no_reg_vars: false, ..CompileOpts::default() };
    let target = recompile_text(&src, &opts).unwrap();
    println!("--- source ---\n{src}");
    println!("target _TEXT: {} bytes", target.len());
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
