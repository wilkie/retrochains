//! End-to-end capstone: link real BCC-compiled programs against the real
//! Borland C++ 2.0 runtime and assert the output is byte-for-byte what TLINK
//! produces, across memory models.
//!
//! The object files are small and tracked under `tests/data/`; the model
//! suffix (`_M`/`_L`) marks the compile model:
//! - `MAIN*.OBJ`  — `int main(void){return 0;}`.
//! - `HELLO*.OBJ` — `printf("Hello, world\n")`; pulls the stdio chain.
//! - small (`-ms`, no suffix), medium (`-mm`, `_M`), large (`-ml`, `_L`).
//!
//! The startup `C0<m>.OBJ` and runtime `C<m>.LIB` are *not* tracked — they are
//! large, reproducible artifacts of the provisioned install (`oracle provision
//! bcc`), read from `.bc2/BC2/LIB/` at test time; the tests skip when absent.
//!
//! The byte-exact contract is the recorded SHA-256 of the linked `MAIN.EXE`
//! (and, where it matches, `MAIN.MAP`) captured against TLINK.EXE 4.0. The
//! large-model `.MAP` is intentionally not gated: TLINK orders a few
//! same-address far/near alias pairs (`_free`/`_farfree`) by an internal
//! symbol-table order the linker doesn't model — the EXE is unaffected.

use sha2::{Digest, Sha256};

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn lib_dir() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.join(".bc2/BC2/LIB")
}

/// Link `tests/data/<obj_name>` in memory model `m` (`'S'`/`'M'`/`'L'`) against
/// the provisioned `C0<m>.OBJ` + `C<m>.LIB`, returning the linked (EXE, MAP) or
/// `None` when the install isn't present.
fn link_in_model(m: char, obj_name: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let dir = lib_dir();
    let c0_name = format!("C0{m}.OBJ");
    let lib_name = format!("C{m}.LIB");
    let (c0_path, lib_path) = (dir.join(&c0_name), dir.join(&lib_name));
    if !c0_path.exists() || !lib_path.exists() {
        eprintln!("skipping capstone: install not found at {} (run `oracle provision bcc`)", dir.display());
        return None;
    }
    let c0 = std::fs::read(&c0_path).expect("read C0");
    let lib = std::fs::read(&lib_path).expect("read lib");
    let obj_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(obj_name);
    let obj = std::fs::read(&obj_path).unwrap_or_else(|e| panic!("read {obj_name}: {e}"));

    let objects = vec![(c0_name, c0), (obj_name.to_string(), obj)];
    let libraries = vec![(lib_name, lib)];
    let image = bcc_tlink::link_image(&objects, &libraries).expect("link");
    Some((bcc_tlink::mz::write(&image), bcc_tlink::map::format(&image)))
}

/// Assert the linked EXE (and, when `map_sha` is `Some`, the MAP) match TLINK.
fn check(m: char, obj: &str, exe_sha: &str, map_sha: Option<&str>) {
    let Some((exe, map)) = link_in_model(m, obj) else { return };
    assert_eq!(hex_sha256(&exe), exe_sha, "{obj} ({m}): MAIN.EXE diverged from TLINK");
    if let Some(map_sha) = map_sha {
        assert_eq!(hex_sha256(&map), map_sha, "{obj} ({m}): MAIN.MAP diverged from TLINK");
    }
}

#[test]
fn small_return_zero() {
    check('S', "MAIN.OBJ",
        "a429690a97fd5b956f77f596c1057062310b82aa550bfa66f02f4466a68c5727",
        Some("e45b3af6fc469bd0bfc58c23a9dca8fbb0448edc1496528974b3cf38f853628c"));
}

#[test]
fn small_printf_hello_world() {
    check('S', "HELLO.OBJ",
        "4e3af60028660868f0e30283bca8a7c31fe798fcbd71f07c0ecbd034f8d26ed2",
        Some("f8cd2ac8bba4c7499c0d30b06d79220914ba269612f42f440287fd0211e48aa0"));
}

#[test]
fn medium_return_zero() {
    check('M', "MAIN_M.OBJ",
        "c209f7044039d4c9cabb0a426f435c6dd64813ffcd809907c0f99a3559c871b9",
        Some("039c48f49915c1bdfb6c00472a8e399a83390a783796335df9dc13ed5a192e58"));
}

#[test]
fn medium_printf_hello_world() {
    check('M', "HELLO_M.OBJ",
        "36e0924c7ac9fabeadec42f771bab1acab55d80ac485515fafc11c9f912c8342",
        Some("52fc4f8ce42f5c06bba1a3c440f33b4c3c3c66ac47dab6fce37d8a0c4562ca5e"));
}

// Large model: EXE byte-exact; `.MAP` not gated (see module docs — `_free`/
// `_farfree` alias ordering).
#[test]
fn large_return_zero() {
    check('L', "MAIN_L.OBJ",
        "8565f2a373d43da87e849b8fe73e6d6ed1e2f6afbc91ec65c8190aa929f64048", None);
}

#[test]
fn large_printf_hello_world() {
    check('L', "HELLO_L.OBJ",
        "d2d5c7e03a51dffeed0da98b606c1071324488e6e942469dcf05547b75d5bab6", None);
}
