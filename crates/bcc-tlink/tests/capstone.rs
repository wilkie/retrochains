//! End-to-end capstone: link real BCC-compiled programs against the real
//! Borland C++ 2.0 runtime and assert the output is byte-for-byte what TLINK
//! produces, across memory models.
//!
//! The object files are small and tracked under `tests/data/`; the model
//! suffix marks the compile model:
//! - `MAIN*.OBJ`  — `int main(void){return 0;}`.
//! - `HELLO*.OBJ` — `printf("Hello, world\n")`; pulls the stdio chain.
//! - small (`-ms`, no suffix), medium (`-mm`, `_M`), compact (`-mc`, `_C`),
//!   large (`-ml`, `_L`), huge (`-mh`, `_H`).
//!
//! The startup `C0<m>.OBJ` and runtime `C<m>.LIB` are *not* tracked — they are
//! large, reproducible artifacts of the provisioned install (`oracle provision
//! bcc`), read from `.bc2/BC2/LIB/` at test time; the tests skip when absent.
//!
//! The byte-exact contract is the recorded SHA-256 of the linked `MAIN.EXE`
//! and `MAIN.MAP`, captured against TLINK.EXE 4.0, for every model.

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

/// VROOMM overlay link: a medium-model program whose `MOD.OBJ` is overlaid
/// (`tlink C0M+MAIN /o MOD, PROG, , CM+OVERLAY`). Drives the full overlay path —
/// force-pulling the disk-overlay manager, generating the INT 3F stub, the
/// `_EXEINFO_`/`__SEGTABLE__` relocation table, and the appended FBOV area — and
/// asserts the whole EXE (resident image + overlay area) byte-matches TLINK.
fn overlay_data(rel: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/overlay").join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read overlay/{rel}: {e}"))
}

/// Link a medium-model overlay program: a resident `MAIN.OBJ` (data from
/// `main_rel`) plus the provisioned C0M/CM/OVERLAY, with each `(obj_name, rel)`
/// in `overlaid` placed after `/o`. Returns the EXE, or `None` if the install
/// isn't present.
fn link_overlay_exe(main_rel: &str, overlaid: &[(&str, &str)]) -> Option<Vec<u8>> {
    let dir = lib_dir();
    let (c0, cm, ovl) = (dir.join("C0M.OBJ"), dir.join("CM.LIB"), dir.join("OVERLAY.LIB"));
    if !c0.exists() || !cm.exists() || !ovl.exists() {
        eprintln!("skipping overlay capstone: install not found at {} (run `oracle provision bcc`)", dir.display());
        return None;
    }
    let mut objects = vec![
        ("C0M.OBJ".to_string(), std::fs::read(&c0).expect("read C0M")),
        ("MAIN.OBJ".to_string(), overlay_data(main_rel)),
    ];
    let mut overlaid_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    for &(obj, rel) in overlaid {
        objects.push((obj.to_string(), overlay_data(rel)));
        overlaid_set.insert(obj.to_string());
    }
    let libraries = vec![
        ("CM.LIB".to_string(), std::fs::read(&cm).expect("read CM.LIB")),
        ("OVERLAY.LIB".to_string(), std::fs::read(&ovl).expect("read OVERLAY.LIB")),
    ];
    Some(bcc_tlink::link_overlay(&objects, &libraries, &overlaid_set, "PROG.EXE").expect("overlay link"))
}

#[test]
fn medium_overlay() {
    let Some(exe) = link_overlay_exe("MAIN.OBJ", &[("MOD.OBJ", "MOD.OBJ")]) else { return };
    assert_eq!(
        hex_sha256(&exe),
        "eee0f5e246f7df19be44b9db632c39cef3cba5431157ddc4d859b89ca9211bc4",
        "overlay PROG.EXE diverged from TLINK",
    );
}

/// Overlaid code with one inter-segment reference (`square` far-calls the
/// resident `helper`): exercises the `__SEGTABLE__`-index rewrite, the single
/// load-time relocation offset, and the stub's reloc-count field.
#[test]
fn medium_overlay_intseg_one_ref() {
    let Some(exe) = link_overlay_exe("intseg/MAIN.OBJ", &[("MOD.OBJ", "intseg/MOD1.OBJ")]) else {
        return;
    };
    assert_eq!(
        hex_sha256(&exe),
        "0d70eb7cca83f63894e9d127051d0d8e9a59f40b5744708288c09481189aad1e",
        "one-ref overlay PROG.EXE diverged from TLINK",
    );
}

/// Overlaid code with two inter-segment references (two far calls): exercises
/// the descending relocation-offset list and the reloc-count scaling.
#[test]
fn medium_overlay_intseg_two_refs() {
    let Some(exe) = link_overlay_exe("intseg/MAIN.OBJ", &[("MOD.OBJ", "intseg/MOD2.OBJ")]) else {
        return;
    };
    assert_eq!(
        hex_sha256(&exe),
        "3e6b2ba22e3907e1be6cde20d10838d5834917e5145afd87b37b772bf38ec1a2",
        "two-ref overlay PROG.EXE diverged from TLINK",
    );
}

/// Two overlaid modules where one overlaid function far-calls the other
/// (`square` → `helper`, both overlaid): exercises multi-overlay area-offset
/// accumulation (the second stub's descriptor points past the first slot) and
/// an overlay→overlay reference resolved through the callee's stub thunk.
#[test]
fn medium_overlay_two_modules() {
    let overlaid = [("MOD1.OBJ", "multi/MOD1.OBJ"), ("MOD2.OBJ", "multi/MOD2.OBJ")];
    let Some(exe) = link_overlay_exe("multi/MAIN.OBJ", &overlaid) else { return };
    assert_eq!(
        hex_sha256(&exe),
        "a343e581cf0029c12a49c30900f51f70cbcd920713969cb5a463d61ec76842a1",
        "two-module overlay PROG.EXE diverged from TLINK",
    );
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

#[test]
fn compact_return_zero() {
    check('C', "MAIN_C.OBJ",
        "faf2dda41bba82b594db065875ed6cab2cd0694bc145abbadc0920d5a5a7ebb8",
        Some("d75dc94fd4afbae5fc53b9aa62543eecb9f963bca1c3e70dbfd5f60ec20a8b26"));
}

#[test]
fn compact_printf_hello_world() {
    check('C', "HELLO_C.OBJ",
        "0265e45d443c9d8175a6d75bdf2931552bcdb0cde9c414ac4aea2132d58c7052",
        Some("9baf868e24ee547886df5676a9b579a627b2803be2ee4bcd30b68714c7f8097c"));
}

#[test]
fn large_return_zero() {
    check('L', "MAIN_L.OBJ",
        "8565f2a373d43da87e849b8fe73e6d6ed1e2f6afbc91ec65c8190aa929f64048",
        Some("6ff0fda62c2783114713ce7199d4e0fec6c0db5206a7926bb6487a29f81b149d"));
}

#[test]
fn large_printf_hello_world() {
    check('L', "HELLO_L.OBJ",
        "d2d5c7e03a51dffeed0da98b606c1071324488e6e942469dcf05547b75d5bab6",
        Some("e393c84c5f04402696d495bcc129f9e498ead27ae34910752a668cb32533ec59"));
}

#[test]
fn huge_return_zero() {
    check('H', "MAIN_H.OBJ",
        "6a730e10ccfed647b1df40e88b128a9721219f0c561d00ffd543f49dd4593d8a",
        Some("134cd20321d661e3a192d1368229edd67e39ac31c67fba740d1dd4487d8d3fdc"));
}

#[test]
fn huge_printf_hello_world() {
    check('H', "HELLO_H.OBJ",
        "99aea3945a03d3b7e0abf5ddd48c0382e795328a0b10e138dadf34c48fc1fa95",
        Some("0ceeea5a8c4f71ec0e8c61bc9ae229ccb89c1b572a340af90acf79804c6eeca5"));
}
