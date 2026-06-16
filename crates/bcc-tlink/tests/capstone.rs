//! End-to-end capstone: link real BCC-compiled programs against the real
//! Borland C++ 2.0 runtime and assert the output is byte-for-byte what TLINK
//! produces.
//!
//! The object files are small and tracked under `tests/data/`:
//! - `MAIN.OBJ` — `int main(void){return 0;}` (`bcc -c -ms`), 211 bytes.
//! - `HELLO.OBJ` — `printf("Hello, world\n")` (`bcc -c -ms -IC:\INCLUDE`),
//!   298 bytes; pulls the formatted-output / stdio chain out of CS.LIB.
//!
//! The startup `C0S.OBJ` and the runtime library `CS.LIB` are *not* tracked —
//! they are large, reproducible artifacts of the provisioned install
//! (`oracle provision bcc`), so we read them from `.bc2/BC2/LIB/` at test time
//! and skip cleanly when the install is absent.
//!
//! The byte-exact contract is the recorded SHA-256 of the linked `MAIN.EXE` and
//! `MAIN.MAP`, captured against TLINK.EXE 4.0. Matching these exercises the
//! whole pipeline: OMF parsing (incl. absolute PUBDEFs and LIDATA), DOSSEG
//! segment ordering, own-alignment packing, library-order member placement,
//! group-relative framing (of both publics and fixups), MZ relocation
//! emission, and the `.MAP` listing.

use sha2::{Digest, Sha256};

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Link `tests/data/<obj_name>` against the provisioned C0S.OBJ + CS.LIB and
/// assert the linked EXE/MAP match the recorded TLINK hashes. Skips (returns)
/// when the install isn't present.
fn check_link(obj_name: &str, exe_sha: &str, map_sha: &str) {
    let root = {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    };
    let lib_dir = root.join(".bc2/BC2/LIB");
    let (c0s_path, cslib_path) = (lib_dir.join("C0S.OBJ"), lib_dir.join("CS.LIB"));
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
    let obj_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(obj_name);
    let obj = std::fs::read(&obj_path).unwrap_or_else(|e| panic!("read {obj_name}: {e}"));

    // TLINK command line: `C0S.OBJ+<obj>, MAIN.EXE, MAIN.MAP, CS.LIB`.
    let objects = vec![("C0S.OBJ".to_string(), c0s), (obj_name.to_string(), obj)];
    let libraries = vec![("CS.LIB".to_string(), cslib)];

    let image = bcc_tlink::link_image(&objects, &libraries).expect("link");
    let exe = bcc_tlink::mz::write(&image);
    let map = bcc_tlink::map::format(&image);

    assert_eq!(hex_sha256(&exe), exe_sha, "{obj_name}: MAIN.EXE diverged from TLINK");
    assert_eq!(hex_sha256(&map), map_sha, "{obj_name}: MAIN.MAP diverged from TLINK");
}

#[test]
fn capstone_return_zero_byte_exact() {
    check_link(
        "MAIN.OBJ",
        "a429690a97fd5b956f77f596c1057062310b82aa550bfa66f02f4466a68c5727",
        "e45b3af6fc469bd0bfc58c23a9dca8fbb0448edc1496528974b3cf38f853628c",
    );
}

#[test]
fn capstone_printf_hello_world_byte_exact() {
    check_link(
        "HELLO.OBJ",
        "4e3af60028660868f0e30283bca8a7c31fe798fcbd71f07c0ecbd034f8d26ed2",
        "f8cd2ac8bba4c7499c0d30b06d79220914ba269612f42f440287fd0211e48aa0",
    );
}
