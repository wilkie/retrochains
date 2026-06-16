//! Install-free linker + librarian coverage from *synthetic* objects.
//!
//! These tests need no oracle, no DOSBox, and no provisioned `.bc2`: they build
//! OMF objects in code with [`bcc_tlink::omf::emit`], then exercise the linker
//! and librarian on them. Correctness is anchored two ways that need no external
//! reference — a value derived from the OMF/MZ rules (the resolved call
//! displacement) and a *metamorphic* invariant (linking a member directly must
//! equal linking it pulled from a `bcc-tlib` archive).

use bcc_tlink::omf::ModuleBuilder;

/// Object A: an entry stub that near-calls the external `_helper`, then a stack.
/// `e8 .. ..` (call _helper) `b8 00 4c cd 21` (mov ax,4c00h; int 21h).
fn prog_object() -> Vec<u8> {
    let mut b = ModuleBuilder::new("PROG");
    let text = b.code_segment("_TEXT", &[0xe8, 0x00, 0x00, 0xb8, 0x00, 0x4c, 0xcd, 0x21]);
    b.near_call(text, 1, "_helper");
    b.stack_segment("STACK", 0x80);
    b.public("_start", text, 0).entry(text, 0);
    b.emit()
}

/// Object B: defines `_helper` — `b8 2a 00 c3` (mov ax,42; ret).
fn helper_object() -> Vec<u8> {
    let mut b = ModuleBuilder::new("HELPER");
    let text = b.code_segment("_TEXT", &[0xb8, 0x2a, 0x00, 0xc3]);
    b.public("_helper", text, 0);
    b.emit()
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Byte-exact against real TLINK, but install-free: the two objects are
/// generated in code by `emit`, and the linked EXE's SHA-256 is asserted
/// against a value captured **once** from TLINK.EXE 4.0 linking those exact
/// emitted bytes (`PROG.OBJ+HELPER.OBJ`). Re-bless via the oracle (drive real
/// TLINK on the emitted objects) only if the synthetic objects change.
#[test]
fn synthetic_link_is_byte_exact_vs_tlink() {
    let exe = bcc_tlink::link_objects(
        &[("PROG.OBJ".into(), prog_object()), ("HELPER.OBJ".into(), helper_object())],
        &[],
    )
    .expect("link synthetic objects");
    assert_eq!(
        hex_sha256(&exe),
        "de1f1db5126bbf6479bf116c9a1f92e29fbf96f04ab873edd747a0698c60dafe",
        "synthetic SYN.EXE diverged from TLINK",
    );
}

/// The linker resolves a cross-object near call: `_helper` lands right after the
/// 8-byte entry stub in the combined `_TEXT`, so the `e8` displacement is
/// `8 - 3 = 5` (target minus the address past the call instruction).
#[test]
fn synthetic_cross_object_call_resolves() {
    let exe = bcc_tlink::link_objects(
        &[("PROG.OBJ".into(), prog_object()), ("HELPER.OBJ".into(), helper_object())],
        &[],
    )
    .expect("link synthetic objects");

    assert_eq!(&exe[0..2], b"MZ", "MZ signature");
    // Header is one 0x200 page (no relocations); `_TEXT` (first CODE segment)
    // loads at image offset 0, so its bytes start at file offset 0x200.
    assert_eq!(exe[0x200], 0xe8, "entry stub starts with a near call");
    assert_eq!(&exe[0x201..0x203], &[0x05, 0x00], "call displacement resolves to _helper");
}

/// Metamorphic invariant, no external reference: linking `_helper` directly must
/// produce the same executable as linking it pulled from a `bcc-tlib` archive.
/// Exercises emit → librarian archive build → linker library pull + resolution.
#[test]
fn synthetic_library_pull_matches_direct_link() {
    let prog = prog_object();
    let helper = helper_object();

    let direct = bcc_tlink::link_objects(
        &[("PROG.OBJ".into(), prog.clone()), ("HELPER.OBJ".into(), helper.clone())],
        &[],
    )
    .expect("direct link");

    let lib = bcc_tlib::build_library(&[("HELPER".into(), helper)], false).expect("build library");
    let via_lib =
        bcc_tlink::link_objects(&[("PROG.OBJ".into(), prog)], &[("SYN.LIB".into(), lib)])
            .expect("link against synthetic library");

    assert_eq!(direct, via_lib, "library-pulled member must link identically to a direct object");
}
