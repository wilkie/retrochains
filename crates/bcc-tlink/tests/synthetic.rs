//! Install-free linker + librarian coverage from *synthetic* objects.
//!
//! These tests need no oracle, no DOSBox, and no provisioned `.bc2`: they build
//! OMF objects in code with [`bcc_tlink::omf::emit`], then exercise the linker
//! and librarian on them. Correctness is anchored two ways that need no external
//! reference — a value derived from the OMF/MZ rules (the resolved call
//! displacement) and a *metamorphic* invariant (linking a member directly must
//! equal linking it pulled from a `bcc-tlib` archive).

use bcc_tlink::omf::{Frame, ModuleBuilder};

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

/// A self-contained DGROUP program: set `DS = DGROUP`, then load an
/// initialized `_DATA` global and store it into an uninitialized `_BSS` global,
/// both addressed DGROUP-relative. Exercises a group selector (T5, location 2)
/// and near data references framed against the group (T4, F1).
///
/// ```text
///   b8 .. ..   mov ax, DGROUP     ; group selector (relocated)
///   8e d8      mov ds, ax
///   a1 .. ..   mov ax, [_seed]    ; DGROUP-relative load   (_DATA + 0)
///   a3 .. ..   mov [_result], ax  ; DGROUP-relative store  (_BSS  + 0)
///   b8 00 4c   mov ax, 4c00h
///   cd 21      int 21h
/// ```
fn dgroup_object() -> Vec<u8> {
    let mut b = ModuleBuilder::new("DGDATA");
    let text = b.code_segment(
        "_TEXT",
        &[0xb8, 0, 0, 0x8e, 0xd8, 0xa1, 0, 0, 0xa3, 0, 0, 0xb8, 0x00, 0x4c, 0xcd, 0x21],
    );
    let data = b.data_segment("_DATA", &[0x2a, 0x00]); // _seed = 42
    let bss = b.bss_segment("_BSS", 2); // _result
    let dgroup = b.group("DGROUP", &[data, bss]);
    b.group_ref(text, 1, 2, true, Frame::Group(dgroup), dgroup); // mov ax, DGROUP
    b.segment_ref(text, 6, 1, true, Frame::Group(dgroup), data); // mov ax, [_seed]
    b.segment_ref(text, 9, 1, true, Frame::Group(dgroup), bss); // mov [_result], ax
    b.public("_seed", data, 0)
        .public("_result", bss, 0)
        .public("_start", text, 0)
        .entry(text, 0);
    b.emit()
}

/// A self-contained far-data program: build a far pointer (`ES:BX`) to a global
/// in its own paragraph-aligned `FAR_DATA` segment, then read through it.
/// Unlike DGROUP data, the segment word relocates to that segment's *own*
/// paragraph (not the data group) and the offset is segment-relative.
///
/// ```text
///   b8 .. ..      mov ax, SEG _fdata    ; far segment selector (relocated)
///   8e c0         mov es, ax
///   bb .. ..      mov bx, OFFSET _fdata ; offset within FAR_DATA
///   26 8b 07      mov ax, [es:bx]
///   b8 00 4c      mov ax, 4c00h
///   cd 21         int 21h
/// ```
fn far_data_object() -> Vec<u8> {
    let mut b = ModuleBuilder::new("FARDAT");
    let text = b.code_segment(
        "_TEXT",
        &[0xb8, 0, 0, 0x8e, 0xc0, 0xbb, 0, 0, 0x26, 0x8b, 0x07, 0xb8, 0x00, 0x4c, 0xcd, 0x21],
    );
    // Paragraph-aligned far segment (align 3) so its base is a clean frame.
    let far = b.segment("FAR_DATA", "FAR_DATA", 3, 2, &[0x34, 0x12]); // _fdata = 0x1234
    b.segment_ref(text, 1, 2, true, Frame::Segment(far), far); // SEG _fdata (selector)
    b.segment_ref(text, 6, 1, true, Frame::Segment(far), far); // OFFSET _fdata
    b.public("_fdata", far, 0).public("_start", text, 0).entry(text, 0);
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

/// Far-data addressing, byte-exact against TLINK and install-free. The segment
/// selector relocates to `FAR_DATA`'s own paragraph (it sits right after the
/// 0x10-byte `_TEXT`, at image paragraph 1, and is fixed up by one MZ
/// relocation), and the offset is segment-relative (`_fdata` at FAR_DATA+0).
#[test]
fn synthetic_far_data_pointer() {
    let exe = bcc_tlink::link_objects(&[("FARDAT.OBJ".into(), far_data_object())], &[])
        .expect("link far-data object");
    assert_eq!(
        hex_sha256(&exe),
        "459cd89800d566f2022fecccffd385b9faefc89c927b7896cc2a80fc566db50c",
        "far-data object diverged from TLINK",
    );
    assert_eq!(&exe[0x201..0x203], &[0x01, 0x00], "SEG _fdata = FAR_DATA's own paragraph");
    assert_eq!(&exe[0x206..0x208], &[0x00, 0x00], "OFFSET _fdata = FAR_DATA+0");
    assert_eq!(u16::from_le_bytes([exe[6], exe[7]]), 1, "the far segment word is relocated once");
}

/// DGROUP-relative data addressing, byte-exact against TLINK and install-free.
/// The two globals resolve to group-relative offsets — `_DATA` at the DGROUP
/// base (0), `_BSS` right after the 2-byte `_DATA` (2) — and the whole EXE
/// matches a SHA captured once from TLINK.EXE 4.0 linking the emitted object.
#[test]
fn synthetic_dgroup_relative_data() {
    let exe = bcc_tlink::link_objects(&[("DGDATA.OBJ".into(), dgroup_object())], &[])
        .expect("link DGROUP object");
    assert_eq!(
        hex_sha256(&exe),
        "b7ba34fdfab49183c608cdc3545fb589dc95ade54b125fdef50a3ee1260931a7",
        "DGROUP object diverged from TLINK",
    );
    // `_TEXT` (first CODE segment) loads at image offset 0 → file offset 0x200.
    assert_eq!(&exe[0x206..0x208], &[0x00, 0x00], "_seed addressed as _DATA+0 in DGROUP");
    assert_eq!(&exe[0x209..0x20b], &[0x02, 0x00], "_result addressed as _BSS = DGROUP+2");
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
