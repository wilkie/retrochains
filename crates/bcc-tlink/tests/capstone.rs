//! End-to-end capstone: link a real BCC-compiled program against the real
//! Borland C++ 2.0 runtime and assert the output is byte-for-byte what TLINK
//! produces.
//!
//! `MAIN.OBJ` (211 bytes, `int main(void){return 0;}` compiled with
//! `bcc -c -ms`) is tracked here. The startup `C0S.OBJ` and the runtime
//! library `CS.LIB` are *not* tracked — they are large, reproducible artifacts
//! of the provisioned install (`oracle provision bcc`), so we read them from
//! `.bc2/BC2/LIB/` at test time and skip cleanly when the install is absent
//! (e.g. a checkout that hasn't been provisioned).
//!
//! The byte-exact contract is the recorded SHA-256 of the linked `MAIN.EXE`
//! and `MAIN.MAP`, captured against TLINK.EXE 4.0. Linking pulls 16 members
//! transitively from CS.LIB; matching these hashes exercises the whole
//! pipeline: OMF parsing (incl. absolute PUBDEFs and LIDATA), DOSSEG segment
//! ordering, own-alignment packing, library-order member placement,
//! group-relative public framing, MZ relocation emission, and the `.MAP`
//! listing (uppercasing, `Abs` tags, by-value absolute grouping).

use sha2::{Digest, Sha256};

/// SHA-256 of TLINK's `MAIN.EXE` for `C0S.OBJ + MAIN.OBJ` against `CS.LIB`.
const EXE_SHA256: &str = "a429690a97fd5b956f77f596c1057062310b82aa550bfa66f02f4466a68c5727";
/// SHA-256 of TLINK's `MAIN.MAP` for the same link.
const MAP_SHA256: &str = "e45b3af6fc469bd0bfc58c23a9dca8fbb0448edc1496528974b3cf38f853628c";

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn capstone_links_real_bcc_program_byte_exact() {
    let root = {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    };
    let lib_dir = root.join(".bc2/BC2/LIB");
    let c0s_path = lib_dir.join("C0S.OBJ");
    let cslib_path = lib_dir.join("CS.LIB");
    if !c0s_path.exists() || !cslib_path.exists() {
        eprintln!(
            "skipping capstone: provisioned install not found at {} \
             (run `oracle provision bcc`)",
            lib_dir.display()
        );
        return;
    }

    let c0s = std::fs::read(&c0s_path).expect("read C0S.OBJ");
    let cslib = std::fs::read(&cslib_path).expect("read CS.LIB");
    let main_obj =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/MAIN.OBJ")).expect("MAIN.OBJ");

    // TLINK command line: `C0S.OBJ+MAIN.OBJ, MAIN.EXE, MAIN.MAP, CS.LIB`.
    let objects = vec![("C0S.OBJ".to_string(), c0s), ("MAIN.OBJ".to_string(), main_obj)];
    let libraries = vec![("CS.LIB".to_string(), cslib)];

    let image = bcc_tlink::link_image(&objects, &libraries).expect("link");
    let exe = bcc_tlink::mz::write(&image);
    let map = bcc_tlink::map::format(&image);

    assert_eq!(hex_sha256(&exe), EXE_SHA256, "MAIN.EXE bytes diverged from TLINK");
    assert_eq!(hex_sha256(&map), MAP_SHA256, "MAIN.MAP bytes diverged from TLINK");
}
