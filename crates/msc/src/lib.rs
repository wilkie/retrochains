//! Microsoft C 5.0 compiler reimplementation. Phase 1 covers
//! `int main(void) { return K; }` under `cl /c /AS` for any 16-bit
//! integer literal K. See `specs/plans/MSC_PHASE_1.md` for the
//! sliced roadmap; this file's Slice 1+2 emit the OBJ directly.
//!
//! The reimplementation produces OBJ bytes directly via `crates/obj`
//! rather than going through an ASM-text round-trip (which is BCC's
//! choice because it has a `-S` text output to match). MSC has no
//! equivalent textual intermediate, so the byte-exact target IS the
//! OBJ.

use std::path::Path;

use obj::ObjBuilder;

/// Compile `source_path` (a C source file) to `<NAME>.OBJ` in the
/// current working directory. Mirrors `cl /c /AS HELLO.C`'s file
/// naming: the output basename is the source's basename uppercased
/// with the `.OBJ` extension.
///
/// # Errors
/// Returns [`EmitError`] on I/O failures or unsupported source shapes.
pub fn emit_dash_c(source_path: &Path) -> Result<std::path::PathBuf, EmitError> {
    let source_filename = source_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| EmitError::BadSourcePath(source_path.display().to_string()))?;
    let source = std::fs::read_to_string(source_path).map_err(EmitError::Io)?;
    let return_value = parse_return_int(&source)?;
    let bytes = build_obj(source_filename, return_value);
    let basename = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("OUT");
    let out_name = format!("{}.OBJ", basename.to_ascii_uppercase());
    std::fs::write(&out_name, bytes).map_err(EmitError::Io)?;
    Ok(std::path::PathBuf::from(out_name))
}

/// Extract the `return K;` literal from an `int main(void) { return K; }`
/// source. Phase 1's source-shape envelope is intentionally tiny —
/// once Slice 3 lands we'll have a real parser. Returns the literal
/// truncated to a 16-bit value (matching MSC's `int` width under /AS).
fn parse_return_int(source: &str) -> Result<i32, EmitError> {
    let after_return = source
        .find("return")
        .map(|i| &source[i + "return".len()..])
        .ok_or_else(|| EmitError::Unsupported("no `return` keyword in source".to_owned()))?;
    let trimmed = after_return.trim_start();
    let end = trimmed
        .find(';')
        .ok_or_else(|| EmitError::Unsupported("no `;` after `return`".to_owned()))?;
    let lit = trimmed[..end].trim();
    lit.parse::<i32>()
        .map_err(|_| EmitError::Unsupported(format!("`return {lit};` literal not parseable")))
}

/// Produce the OBJ bytes for `cl /c /AS <source>` compiling
/// `int main(void) { return <return_value>; }`. `source_filename`
/// goes into THEADR uppercased the same way CL does it on the
/// command line.
#[must_use]
pub fn build_obj(source_filename: &str, return_value: i32) -> Vec<u8> {
    let mut b = ObjBuilder::new();

    // THEADR — module header. Source filename uppercased.
    b.write_theadr(source_filename);

    // COMENT class 0x00 — translator identification.
    // Payload (after flags+class): "MS C". Flags 0x00 = no-purge,
    // no-list (LINK keeps the COMENT in the output).
    b.write_coment(&[0x00, 0x00, b'M', b'S', b' ', b'C']);

    // COMENT class 0x9F — default library. The linker should pull
    // SLIBCE (small-model, math-emulator C runtime) when resolving
    // unresolved externs at link time. /AS without an explicit
    // /F* flag selects SLIBCE.
    b.write_coment(&[0x00, 0x9F, b'S', b'L', b'I', b'B', b'C', b'E']);

    // COMENT class 0x9D — memory-model marker. MSC's internal tag
    // for the model + a few flag bits. Bytes ASCII "0sO" — three
    // single-byte fields that MSC's LINK reads to verify model
    // consistency across OBJs. We carry the exact bytes /AS emits;
    // characterizing each byte's meaning is Phase 2 work.
    b.write_coment(&[0x00, 0x9D, b'0', b's', b'O']);

    // COMENT class 0xA1 — extension marker. Payload `0x01 "CV"` —
    // probably "CodeView 1" capability hint. Empty-main has no
    // debug info but MSC emits the hint unconditionally under /AS.
    b.write_coment(&[0x00, 0xA1, 0x01, b'C', b'V']);

    // LNAMES — name table. Empty name at index 1 is the standard
    // placeholder; MSC then orders the remaining names with DGROUP
    // first (BCC puts DGROUP last). Indices used by the SEGDEFs
    // and GRPDEF below.
    //   1: ""        (placeholder)
    //   2: DGROUP
    //   3: _TEXT     4: CODE
    //   5: _DATA     6: DATA
    //   7: CONST     (its own class)
    //   8: _BSS      9: BSS
    b.write_lnames(&[
        "",
        "DGROUP",
        "_TEXT", "CODE",
        "_DATA", "DATA",
        "CONST",
        "_BSS", "BSS",
    ]);

    // Build the `_main` body up front so we can stamp its length
    // into the _TEXT SEGDEF. MSC pads odd-length function bodies
    // with a trailing NOP so every function ends on a word boundary
    // — the `return K != 0` shape lands at 9 bytes pre-pad and 10
    // after; `return 0` is already 8 and gets no pad. Fixtures 4075
    // (return 0), 4076 (return 1), 4077 (return 42), 4078 (return -1).
    let main_body = main_body_for_return(return_value);
    let text_len = u16::try_from(main_body.len()).expect("_TEXT body fits in u16");

    // SEGDEF table. MSC uses acbp=0x48 (word-aligned, public, big=0,
    // proc=0) for every segment in the small model — distinct from
    // BCC which uses 0x28 (byte-aligned) for _TEXT. The 0x48 value
    // forces TLINK/LINK to pad to a word boundary before each
    // segment, which matters when multiple OBJs combine.
    //
    // SEGDEF #1: _TEXT  — code, sized to `_main`'s padded length
    b.write_segdef16(0x48, text_len, 3, 4, 1);
    // SEGDEF #2: _DATA  — initialized data, 0 bytes (no globals)
    b.write_segdef16(0x48, 0, 5, 6, 1);
    // SEGDEF #3: CONST  — read-only literals, 0 bytes
    b.write_segdef16(0x48, 0, 7, 7, 1);
    // SEGDEF #4: _BSS   — uninitialized data, 0 bytes
    b.write_segdef16(0x48, 0, 8, 9, 1);

    // GRPDEF — DGROUP contains CONST, _BSS, _DATA in *that* order.
    // The order matches MSC's typical link layout: read-only first,
    // then BSS (which links can collapse), then writable. BCC puts
    // _DATA / _BSS in source-declaration order; MSC reorders.
    b.write_grpdef(2, &[3, 4, 2]);

    // FIXUPP — pre-emitted THREAD subrecords. MSC's CL emits these
    // even when only some are referenced; they let later FIXUPs use
    // a 1-byte thread reference instead of the full frame/target
    // datum pair.
    //
    //   Target thread 0 → SEGDEF #3 (CONST)
    //   Target thread 1 → SEGDEF #2 (_DATA)
    //   Target thread 2 → SEGDEF #1 (_TEXT)
    //   Target thread 3 → SEGDEF #4 (_BSS)
    //   Frame  thread 0 → SEGDEF #1 (_TEXT)
    //   Frame  thread 1 → GRPDEF #1 (DGROUP)
    //
    // Each subrecord is (header_byte, index_byte). The header byte
    // encodes D (FIXUP vs THREAD), F (frame vs target), method, and
    // thread number — see specs/formats/OMF.md §FIXUPP THREAD.
    b.write_fixupp(&[
        // Target threads (D=0, F=0, method T0=SEGDEF):
        0x00, 0x03,   // T0: SEGDEF #3 (CONST)
        0x01, 0x02,   // T1: SEGDEF #2 (_DATA)
        0x02, 0x01,   // T2: SEGDEF #1 (_TEXT)
        0x03, 0x04,   // T3: SEGDEF #4 (_BSS)
        // Frame threads (D=0, F=1):
        0x40, 0x01,   // F0: SEGDEF #1 (_TEXT) — method F0=SEGDEF
        0x45, 0x01,   // F1: GRPDEF #1 (DGROUP) — method F1=GRPDEF
    ]);

    // EXTDEF — external symbols. MSC emits three even for an
    // empty-main:
    //   __acrtused  — sentinel that forces LINK to pull in the C
    //                 runtime startup. No FIXUP references it; its
    //                 presence in the EXTDEF table alone does the
    //                 job. Type-idx 0x01 (vs 0x00) marks it as
    //                 special — see Phase 2 for what 0x01 means
    //                 exactly.
    //   __chkstk    — stack-overflow checker; called from every
    //                 function's prologue under /AS.
    //   _main       — declared here as well as defined via PUBDEF
    //                 below. MSC emits the dual declaration for
    //                 the COMDAT / module-level lookup path.
    //
    // EXTDEF entry shape: <name-length><name-bytes><type-idx>.
    // `obj::ObjBuilder::write_extdef` hardcodes type-idx 0, which
    // doesn't fit MSC's `__acrtused: 1` pattern — build the payload
    // manually.
    {
        let mut payload = Vec::new();
        for (name, ty) in [("__acrtused", 0x01u8), ("__chkstk", 0x00), ("_main", 0x00)] {
            payload.push(u8::try_from(name.len()).expect("EXTDEF name fits"));
            payload.extend_from_slice(name.as_bytes());
            payload.push(ty);
        }
        b.write_record(obj::EXTDEF, &payload);
    }

    // PUBDEF — _main at _TEXT offset 0. base_group_idx=0 means the
    // public is relative to its base segment (SEGDEF #1, _TEXT)
    // directly, no group adjustment.
    b.write_pubdef16(0, 1, "_main", 0, 0);

    // COMENT class 0xA2 — link-pass marker. MSC sandwiches the
    // LEDATA records between EXTDEF/PUBDEF setup and the data
    // itself. The payload byte 0x01 is the "start of data" marker;
    // the matching 0xA2 with 0x00 doesn't appear in this OBJ
    // because there's only one LEDATA pass.
    b.write_coment(&[0x00, 0xA2, 0x01]);

    // LEDATA #1 — _TEXT segment, offset 0, `_main` body bytes.
    // See `main_body_for_return` for the shape.
    b.write_ledata16(1, 0, &main_body);

    // FIXUPP — patch the placeholder bytes of the `call __chkstk`.
    //   Locat byte 1 (0x84): bit7=1 (FIXUP), M=0 (self-relative),
    //                        location=0001 (16-bit offset), hi-off=00
    //   Locat byte 2 (0x03): low 8 bits of data-record offset = 3
    //                        (i.e. bytes 3-4 of the LEDATA data,
    //                        which is the `00 00` displacement of
    //                        the `e8 00 00`)
    //   Fix Data  (0x56):    F=0 (frame explicit), frame-method=F5
    //                        (target's segment), T=0 (target explicit),
    //                        P=1 (no displacement), target-method=T2
    //                        (EXTDEF)
    //   Target datum (0x02): EXTDEF index 2 (__chkstk)
    b.write_fixupp(&[0x84, 0x03, 0x56, 0x02]);

    // MODEND — end of module. No-entry form (the executable's entry
    // point comes from the PUBDEF of `_main` resolved at link time,
    // not from MODEND's start-address field).
    b.write_modend16_no_entry();

    b.into_bytes()
}

/// MSC's `_main` body for `int main(void) { return K; }`. Two
/// shapes, picked by whether the literal is zero:
///
/// **`return 0;` (fixture 4075):**
/// ```text
/// 33 c0          xor ax, ax       ; chkstk arg = frame size (0)
/// e8 00 00       call __chkstk   ; FIXUP'd to EXTDEF #2
/// 2b c0          sub ax, ax       ; return value = 0
/// c3             ret
/// ```
/// Total 8 bytes — already word-aligned, no NOP needed.
///
/// **`return K != 0;` (fixtures 4076 / 4077 / 4078):**
/// ```text
/// 33 c0          xor ax, ax       ; chkstk arg = 0
/// e8 00 00       call __chkstk   ; FIXUP'd
/// b8 <lo> <hi>   mov ax, K        ; return value
/// c3             ret
/// 90             nop              ; pad to even byte count
/// ```
/// Total 10 bytes after the pad.
///
/// The shared prefix `33 c0 e8 00 00` is the per-function prologue
/// MSC always emits under /AS (zero AX as the chkstk arg, call
/// chkstk, the call's displacement gets FIXUP'd at byte offset 3).
/// After the call returns, codegen emits the return-value setup —
/// `sub ax, ax` for zero (special-case, even byte count) or `mov ax,
/// imm16` for everything else (with a trailing NOP to round up).
fn main_body_for_return(return_value: i32) -> Vec<u8> {
    let mut body = Vec::with_capacity(10);
    // Per-function prologue: AX = chkstk arg (0 for empty frame),
    // call __chkstk with the displacement bytes left as 00 00 for
    // the FIXUP to patch.
    body.extend_from_slice(&[0x33, 0xC0, 0xE8, 0x00, 0x00]);
    if return_value == 0 {
        // `sub ax, ax` — 2-byte form picked for the return-0 path
        // even though `xor ax, ax` (2-byte) would be equivalent.
        // MSC's codegen always uses `sub ax, ax` for `return 0;`
        // — empirically pinned by fixture 4075.
        body.extend_from_slice(&[0x2B, 0xC0, 0xC3]);
    } else {
        // `mov ax, imm16` — 3-byte form B8 lo hi. Negative literals
        // wrap to 16-bit unsigned (`-1` → 0xFFFF), matching the
        // standard 8086 convention BCC also follows. Then `ret`
        // and a `nop` pad to even byte count.
        let imm = (return_value as u32 & 0xFFFF) as u16;
        body.push(0xB8);
        body.extend_from_slice(&imm.to_le_bytes());
        body.push(0xC3);
        body.push(0x90);
    }
    body
}

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("could not read source filename from path {0:?}")]
    BadSourcePath(String),
    #[error("unsupported source shape: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
