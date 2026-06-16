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

/// Two CODE segments that byte-pack: `SUB_TEXT` is laid down immediately after
/// the 0x0A-byte `_TEXT` (at image offset 0x0A, *not* paragraph-aligned to
/// 0x10), the way per-module `<MODULE>_TEXT` code segments do in the far memory
/// models. A far call from `_TEXT` to `_sub` therefore carries the byte-packed
/// offset 0x0A in paragraph 0.
///
/// ```text
///   _TEXT:    9a .. .. .. ..   call far _sub
///             b8 00 4c cd 21   mov ax,4c00h; int 21h
///   SUB_TEXT: b8 07 00 cb      mov ax,7; retf
/// ```
fn packed_code_object() -> Vec<u8> {
    let mut b = ModuleBuilder::new("PACKED");
    let text = b.code_segment("_TEXT", &[0x9a, 0, 0, 0, 0, 0xb8, 0x00, 0x4c, 0xcd, 0x21]);
    let sub = b.code_segment("SUB_TEXT", &[0xb8, 0x07, 0x00, 0xcb]);
    b.segment_ref(text, 1, 3, true, Frame::Segment(sub), sub); // call far _sub (location 3)
    b.public("_sub", sub, 0).public("_start", text, 0).entry(text, 0);
    b.emit()
}

/// A self-contained program with a near communal `_g` (a tentative `int g;`)
/// that no PUBDEF defines, referenced DGROUP-relative. The linker must allocate
/// storage for the communal.
fn comdef_object() -> Vec<u8> {
    comdef_object_framed(false)
}

/// The COMDEF program, with the `_g` reference framed by either DGROUP (F1) or
/// the target's own frame (F5 — MSC's `c4 off 56 idx` extern idiom). For a
/// grouped communal both resolve to the same DGROUP-relative offset.
fn comdef_object_framed(g_via_f5: bool) -> Vec<u8> {
    let mut b = ModuleBuilder::new("COMG");
    let text = b.code_segment(
        "_TEXT",
        &[0xb8, 0, 0, 0x8e, 0xd8, 0xa1, 0, 0, 0xb8, 0x00, 0x4c, 0xcd, 0x21],
    );
    let data = b.data_segment("_DATA", &[0x01, 0x00]);
    let bss = b.bss_segment("_BSS", 2);
    let dgroup = b.group("DGROUP", &[data, bss]);
    b.comdef("_g", 2);
    b.group_ref(text, 1, 2, true, Frame::Group(dgroup), dgroup); // mov ax, DGROUP
    let g_frame = if g_via_f5 { Frame::Target } else { Frame::Group(dgroup) };
    b.extern_ref(text, 6, 1, true, g_frame, "_g"); // mov ax, [_g]
    b.public("_start", text, 0).entry(text, 0);
    b.emit()
}

/// Two near communals (`_zz` 1 byte, `_aa` 2 bytes) referenced DGROUP-relative,
/// no `@DATA` selector. Probes communal ordering and packing in `_COMDEF_`.
fn comdef_two_object() -> Vec<u8> {
    let mut b = ModuleBuilder::new("COMG2");
    let text = b.code_segment("_TEXT", &[0xa1, 0, 0, 0xa1, 0, 0, 0xb8, 0x00, 0x4c, 0xcd, 0x21]);
    let data = b.data_segment("_DATA", &[0x01, 0x00]);
    let bss = b.bss_segment("_BSS", 2);
    let dgroup = b.group("DGROUP", &[data, bss]);
    b.comdef("_zz", 1);
    b.comdef("_aa", 2);
    b.extern_ref(text, 1, 1, true, Frame::Group(dgroup), "_zz");
    b.extern_ref(text, 4, 1, true, Frame::Group(dgroup), "_aa");
    b.public("_start", text, 0).entry(text, 0);
    b.emit()
}

/// COMDEF allocation, byte-exact against TLINK and install-free. An undefined
/// near communal `_g` is allocated in a synthesized `_COMDEF_` segment (class
/// BSS, in DGROUP) after `_BSS`; the DGROUP-relative reference resolves to it.
#[test]
fn synthetic_comdef_allocation() {
    let exe = bcc_tlink::link_objects(&[("COMG.OBJ".into(), comdef_object())], &[])
        .expect("link COMDEF object");
    assert_eq!(
        hex_sha256(&exe),
        "67e3c8acbac86920ea3b011338cdd525e3a4000fbb69e8511338810248965bda",
        "COMDEF object diverged from TLINK",
    );
    // `_g` lands in _COMDEF_ after _DATA(2) + _BSS(2); DGROUP base is paragraph 0
    // (no paragraph-aligned member), so _g is at DGROUP offset 0x12 (file 0x206
    // is the `mov ax, [_g]` operand).
    assert_eq!(&exe[0x206..0x208], &[0x12, 0x00], "_g resolved to its _COMDEF_ slot");
}

/// MSC's frame idiom for an extern reference is **F5** (the target's own frame),
/// emitted as `c4 off 56 idx` — not F2 (MSC never emits F2). For a grouped
/// communal, F5 resolves to the same DGROUP-relative offset as F1, so the F5
/// program links byte-identically to the F1 one and byte-exact to TLINK. This
/// exercises `apply_fixup`'s F5 path (`target.frame_para` = the group base).
#[test]
fn synthetic_msc_f5_frame() {
    let via_f5 = bcc_tlink::link_objects(&[("F5G.OBJ".into(), comdef_object_framed(true))], &[])
        .expect("link F5-framed object");
    let via_f1 = bcc_tlink::link_objects(&[("COMG.OBJ".into(), comdef_object_framed(false))], &[])
        .expect("link F1-framed object");
    assert_eq!(via_f5, via_f1, "F5 (target frame) links identically to F1 for a grouped target");
    assert_eq!(
        hex_sha256(&via_f5),
        "67e3c8acbac86920ea3b011338cdd525e3a4000fbb69e8511338810248965bda",
        "F5-framed COMDEF object diverged from TLINK",
    );
}

/// Multiple communals are laid out byte-packed in record order (not name-sorted,
/// no alignment): `_zz` (1 byte) at _COMDEF_+0, `_aa` (2 bytes) right after.
#[test]
fn synthetic_comdef_byte_packed_order() {
    let exe = bcc_tlink::link_objects(&[("COMG2.OBJ".into(), comdef_two_object())], &[])
        .expect("link two-communal object");
    assert_eq!(
        hex_sha256(&exe),
        "57950d4c2f1c988e345d0051418b1067f8dc3ac4bdd8f0d0247edfb5be2e3eea",
        "two-communal object diverged from TLINK",
    );
    assert_eq!(&exe[0x201..0x203], &[0x10, 0x00], "_zz at _COMDEF_+0 (DGROUP 0x10)");
    assert_eq!(&exe[0x204..0x206], &[0x11, 0x00], "_aa byte-packed at _COMDEF_+1 (DGROUP 0x11)");
}

/// An app object that references `_rt` but defines it nowhere, and names a
/// default library `MYRT` (a class-0x9F COMENT) to satisfy it — the way MSC
/// objects name `SLIBCE`.
fn app_needs_default_lib() -> Vec<u8> {
    let mut b = ModuleBuilder::new("APP");
    let text = b.code_segment("_TEXT", &[0xe8, 0, 0, 0xb8, 0x00, 0x4c, 0xcd, 0x21]);
    b.near_call(text, 1, "_rt");
    b.default_lib("MYRT");
    b.public("_start", text, 0).entry(text, 0);
    b.emit()
}

/// A `MYRT.LIB` archive whose one member defines `_rt`.
fn rt_library() -> Vec<u8> {
    let mut b = ModuleBuilder::new("RT");
    let text = b.code_segment("_TEXT", &[0xb8, 0x07, 0x00, 0xc3]); // mov ax,7; ret
    b.public("_rt", text, 0);
    bcc_tlib::build_library(&[("RT".to_string(), b.emit())], false).expect("build MYRT.LIB")
}

/// A class-0x9F default-library directive is honored: the linker pulls the named
/// library (loaded on demand) to satisfy an otherwise-unresolved symbol, and the
/// result is identical to naming that library on the command line.
#[test]
fn synthetic_default_library_directive_pulls() {
    let app = app_needs_default_lib();
    let lib = rt_library();

    // Without honoring the directive (and no command-line library), `_rt` is
    // unresolved and the link fails.
    assert!(bcc_tlink::link_objects(&[("APP.OBJ".into(), app.clone())], &[]).is_err());

    // The directive pulls MYRT (provided by the loader) and resolves `_rt`,
    // which lands right after the 8-byte `_TEXT`: the near call resolves to 5.
    let via_directive = bcc_tlink::link_objects_with_default_libs(
        &[("APP.OBJ".into(), app.clone())],
        &[],
        &|name| name.eq_ignore_ascii_case("MYRT").then(|| lib.clone()),
    )
    .expect("default-library directive resolves _rt");
    assert_eq!(&via_directive[0..2], b"MZ");
    assert_eq!(&via_directive[0x201..0x203], &[0x05, 0x00], "_rt resolved via default library");

    // Pulling via the directive is byte-identical to naming MYRT explicitly.
    let via_cmdline =
        bcc_tlink::link_objects(&[("APP.OBJ".into(), app)], &[("MYRT.LIB".into(), lib)])
            .expect("explicit library resolves _rt");
    assert_eq!(via_directive, via_cmdline, "directive pull equals command-line pull");
}

/// A synthetic stand-in for MSC's `SLIBCE.LIB`: one `CRT0` member defining the
/// runtime symbols a real MSC object references (`__chkstk`, `__acrtused`) and a
/// bit of startup code. No MODEND entry — a library member doesn't set the
/// program entry (TLINK defaults it to 0000:0000).
fn slibce_library() -> Vec<u8> {
    let mut b = ModuleBuilder::new("CRT0");
    let text = b.code_segment("_TEXT", &[0xc3, 0xe8, 0, 0, 0xb8, 0x00, 0x4c, 0xcd, 0x21]);
    b.near_call(text, 2, "_main");
    b.public("__chkstk", text, 0);
    b.public("_start", text, 1);
    b.public("__acrtused", text, 1);
    bcc_tlib::build_library(&[("CRT0".to_string(), b.emit())], false).expect("build SLIBCE.LIB")
}

/// End-to-end: a real MSC object (`int g; int main(){return g;}`) — with its
/// FIXUPP threads, COMDEF `_g`, F5 extern references, and `SLIBCE` default-
/// library directive — links against a synthetic `SLIBCE.LIB` standing in for
/// the Microsoft runtime, byte-exact vs TLINK and install-free. Proves the
/// Borland linker reimplementation consumes Microsoft object files end-to-end.
#[test]
fn msc_object_links_against_synthetic_library() {
    let comm = include_bytes!("data/COMM_MSC.OBJ").to_vec();
    let lib = slibce_library();
    let exe = bcc_tlink::link_objects_with_default_libs(
        &[("COMM.OBJ".into(), comm)],
        &[],
        &|name| name.eq_ignore_ascii_case("SLIBCE").then(|| lib.clone()),
    )
    .expect("MSC object links against synthetic SLIBCE.LIB");
    assert_eq!(
        hex_sha256(&exe),
        "ec49de71844001c6d71adeaaa77d22d669326761e293950791518462b7e04f2f",
        "MSC-object link diverged from TLINK",
    );
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

/// Byte-packed multi-segment code, byte-exact against TLINK and install-free.
/// `SUB_TEXT` packs right after the 0x0A-byte `_TEXT` (image 0x0A, not the next
/// paragraph), and the far call to `_sub` carries that byte-packed offset.
#[test]
fn synthetic_byte_packed_code() {
    let exe = bcc_tlink::link_objects(&[("PACKED.OBJ".into(), packed_code_object())], &[])
        .expect("link packed-code object");
    assert_eq!(
        hex_sha256(&exe),
        "2e0f642620e33bbf03520f77c5dc0fe97e74f29876dd50c60443341378c607ce",
        "packed-code object diverged from TLINK",
    );
    // SUB_TEXT byte-packs at image 0x0A (file 0x20A), not the next paragraph.
    assert_eq!(&exe[0x20a..0x20e], &[0xb8, 0x07, 0x00, 0xcb], "SUB_TEXT byte-packed at image 0x0A");
    // The far call resolves to that byte-packed location: 0000:000A.
    assert_eq!(&exe[0x201..0x205], &[0x0a, 0x00, 0x00, 0x00], "far call _sub = 0000:000A");
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
