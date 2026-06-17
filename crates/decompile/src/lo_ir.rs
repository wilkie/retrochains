//! Lo-IR: the mechanical lift from recognized idioms to micro-operations.
//!
//! This is §4 of `specs/decompiler/IR.md` — the *table-driven* half of the
//! decompiler. [`fingerprint::recognize`] decomposes a `_TEXT` slice into
//! [`Idiom`](fingerprint::Idiom)s; each idiom is a known byte template, so
//! lifting it is decode-not-analyze: read the operands the recognizer masked
//! out (displacements, immediates, register fields) and emit a fixed micro-op
//! sequence. No control- or data-flow reasoning happens here — that's Hi-IR.
//!
//! Two properties make this the right first lift to build:
//!
//! - **It degrades gracefully.** Bytes the recognizer didn't cover (the long
//!   tail) become an opaque [`LoOp::Asm`] spanning the gap, so a function lifts
//!   to *mostly* structured ops with islands of raw bytes rather than failing.
//!   A function still holding `Asm` is a precise "not yet decompilable" signal.
//! - **Every op carries provenance.** Each [`LoInsn`] records the byte [`Span`]
//!   it came from (§8) — the thread the recompile-verify harness pulls when a
//!   mismatch needs to be mapped back to the op that produced the wrong bytes.

use fingerprint::{recognize, Idiom};

/// A byte range within the scanned `_TEXT` — the provenance of one [`LoInsn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Offset of the first byte within the scanned code.
    pub start: usize,
    /// Number of bytes.
    pub len: usize,
}

/// A 16-bit register, in x86 encoding order (`ax cx dx bx sp bp si di`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    Ax,
    Cx,
    Dx,
    Bx,
    Sp,
    Bp,
    Si,
    Di,
}

/// An 8-bit register, in x86 encoding order (`al cl dl bl ah ch dh bh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteReg {
    Al,
    Cl,
    Dl,
    Bl,
    Ah,
    Ch,
    Dh,
    Bh,
}

impl Reg {
    /// Decode a 3-bit ModR/M register field.
    fn from3(n: u8) -> Reg {
        [Reg::Ax, Reg::Cx, Reg::Dx, Reg::Bx, Reg::Sp, Reg::Bp, Reg::Si, Reg::Di][usize::from(n & 7)]
    }
}

impl ByteReg {
    /// Decode a 3-bit ModR/M register field as a byte register.
    fn from3(n: u8) -> ByteReg {
        [
            ByteReg::Al,
            ByteReg::Cl,
            ByteReg::Dl,
            ByteReg::Bl,
            ByteReg::Ah,
            ByteReg::Ch,
            ByteReg::Dh,
            ByteReg::Bh,
        ][usize::from(n & 7)]
    }
}

/// A storage location or operand — the spec's "place". Mapping a place to a C
/// identifier (`[bp−4]` → some local `x`) is Hi-IR's job; Lo-IR just names slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// A word register.
    Reg(Reg),
    /// A byte register (`char`-width operands).
    Byte(ByteReg),
    /// `[bp + disp]`. `disp ≥ 4` is a parameter, `disp < 0` a local (§3).
    Local(i16),
    /// `[disp16]` — a near global within DGROUP.
    Global(u16),
    /// `[si]` / `[di]` — a near-pointer dereference (`*p`).
    Deref(Reg),
    /// An immediate constant.
    Imm(i32),
    /// The flags register — the result side of a `cmp`/`test`.
    Flags,
    /// The `dx:ax` long accumulator — the result of `mul`/`div`/`cwd`.
    DxAx,
}

/// A binary operator at the ISA level (the data-flow operand count, not the
/// encoding). Folding these into C operators is Hi-IR's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Adc,
    Sbb,
    Or,
    And,
    Xor,
    Cmp,
    Test,
    Mul,
    Imul,
    Div,
    Idiv,
    Shl,
    Shr,
    Sar,
    Rol,
    Ror,
    Rcl,
    Rcr,
}

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Inc,
    Dec,
    Neg,
    Not,
}

/// A width-extending promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promote {
    /// `cbw`: sign-extend `al`→`ax` (the `char`→`int` promotion).
    Cbw,
    /// `cwd`: sign-extend `ax`→`dx:ax` (the setup for `idiv` / 32-bit).
    Cwd,
}

/// A condition code on a [`LoOp::Branch`] (the low nibble of a `7x` opcode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cond(pub u8);

/// One micro-operation. The set mirrors the §4 table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoOp {
    /// Function entry; `frame` is the locals byte-reserve (0 for a bare frame).
    Enter { frame: u16 },
    /// Epilogue stack/`bp` restore (`pop bp`, optionally `mov sp,bp` first).
    Leave,
    /// Return. `far` distinguishes `retf` (compact/large/huge) from `ret`.
    Ret { far: bool },
    /// `dst ← *src` (register/global/local/deref/imm source).
    Load { dst: Place, src: Place },
    /// `*dst ← src`.
    Store { dst: Place, src: Place },
    /// `*dst ← imm` at **byte** width (`mov byte ptr [dst], imm`). A separate op
    /// because a memory immediate store carries no register operand to reveal its
    /// width, and byte width is what marks the destination a `char`.
    StoreImmByte { dst: Place, imm: i32 },
    /// `dst ← lhs op rhs`. For `Cmp`/`Test`, `dst` is [`Place::Flags`].
    Bin { dst: Place, op: BinOp, lhs: Place, rhs: Place },
    /// `dst ← op operand` (`inc`/`dec`/`neg`/`not`).
    Un { dst: Place, op: UnOp, operand: Place },
    /// A width promotion (`cbw`/`cwd`).
    Promote { kind: Promote },
    /// `dst ← &src` (`lea`).
    Lea { dst: Place, src: Place },
    /// Push a cdecl argument (§7).
    Arg { src: Place },
    /// A `call`. `far` distinguishes `9a` from `e8`. The callee identity comes
    /// from relocations, not the encoded displacement, so it isn't resolved here.
    Call { far: bool },
    /// cdecl argument cleanup (`pop cx` = 2 bytes, or `add sp,N`).
    Cleanup { bytes: u16 },
    /// A conditional branch to an in-slice byte offset.
    Branch { cond: Cond, target: usize },
    /// An unconditional short jump to an in-slice byte offset.
    Jump { target: usize },
    /// Save a callee-saved register variable (`push si`/`push di`).
    SaveReg { reg: Reg },
    /// Restore a register variable (`pop si`/`pop di`).
    RestoreReg { reg: Reg },
    /// A `pop` that isn't a register-variable restore.
    Pop { dst: Place },
    /// Bytes the recognizer didn't cover — the long tail, lifted opaquely.
    Asm { bytes: Vec<u8> },
}

/// One micro-op plus the byte range it lifted from (its provenance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoInsn {
    pub span: Span,
    pub op: LoOp,
}

/// Lift a `_TEXT` slice to Lo-IR: recognize idioms, decode each into micro-ops,
/// and coalesce unrecognized byte runs into [`LoOp::Asm`].
#[must_use]
pub fn lift(code: &[u8]) -> Vec<LoInsn> {
    let matches = recognize(code);
    let mut out = Vec::new();
    let mut cursor = 0;
    for m in &matches {
        if m.offset > cursor {
            push_gap(&mut out, code, cursor, m.offset);
        }
        let span = Span { start: m.offset, len: m.len };
        let bytes = &code[m.offset..m.offset + m.len];
        for op in decode(m.idiom, bytes, m.offset) {
            out.push(LoInsn { span, op });
        }
        cursor = m.offset + m.len;
    }
    if cursor < code.len() {
        push_gap(&mut out, code, cursor, code.len());
    }
    out
}

/// Emit one opaque `Asm` op covering `code[from..to]`.
fn push_gap(out: &mut Vec<LoInsn>, code: &[u8], from: usize, to: usize) {
    out.push(LoInsn {
        span: Span { start: from, len: to - from },
        op: LoOp::Asm { bytes: code[from..to].to_vec() },
    });
}

/// Read a little-endian `u16` at byte `i` of an idiom's bytes.
fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from(b[i]) | (u16::from(b[i + 1]) << 8)
}

/// Read a signed `disp8` at byte `i`, widened to `i16` (a `[bp±disp]` offset).
fn disp8_at(b: &[u8], i: usize) -> i16 {
    i16::from(b[i].cast_signed())
}

/// The ALU `BinOp` for the `r16, r/m16` opcodes `03/2b/0b/23/33/3b`.
fn alu_op(opcode: u8) -> BinOp {
    match opcode {
        0x03 => BinOp::Add,
        0x2b => BinOp::Sub,
        0x0b => BinOp::Or,
        0x23 => BinOp::And,
        0x33 => BinOp::Xor,
        _ => BinOp::Cmp, // 0x3b
    }
}

/// The base register of a 16-bit `mod=00` memory operand — the `rm` field here
/// selects an addressing mode, *not* a register by the usual encoding
/// (`100`→`[si]`, `101`→`[di]`, `111`→`[bx]`).
fn deref_base(modrm: u8) -> Reg {
    match modrm & 7 {
        4 => Reg::Si,
        5 => Reg::Di,
        _ => Reg::Bx, // 7 = [bx] (the matchers only admit 4/5/7)
    }
}

/// The `BinOp` for the byte `r8, r/m8` ALU opcodes `02/2a/...`.
fn byte_alu_op(opcode: u8) -> BinOp {
    match opcode {
        0x02 => BinOp::Add,
        0x0a => BinOp::Or,
        0x22 => BinOp::And,
        0x2a => BinOp::Sub,
        0x32 => BinOp::Xor,
        _ => BinOp::Cmp, // 0x3a
    }
}

/// The group-1 `BinOp` from a ModR/M `reg` field (opcode `0x83`).
fn group1_op(reg: u8) -> BinOp {
    [
        BinOp::Add,
        BinOp::Or,
        BinOp::Adc,
        BinOp::Sbb,
        BinOp::And,
        BinOp::Sub,
        BinOp::Xor,
        BinOp::Cmp,
    ][usize::from(reg & 7)]
}

/// Lift one recognized idiom to its micro-op sequence. `bytes` is the idiom's
/// own bytes; `off` its offset in the scanned code (for branch-target math).
#[allow(clippy::too_many_lines)] // a flat decode table reads better unsplit
fn decode(idiom: Idiom, bytes: &[u8], off: usize) -> Vec<LoOp> {
    use Place::{Byte, Deref, Global, Imm, Local, Reg as R};

    // The reg/rm fields of a ModR/M byte at index 1 (the common case).
    let modrm = |i: usize| bytes[i];
    let reg_of = |i: usize| Reg::from3(modrm(i) >> 3);
    let rm_of = |i: usize| Reg::from3(modrm(i));
    let byte_reg_of = |i: usize| ByteReg::from3(modrm(i) >> 3);
    let byte_rm_of = |i: usize| ByteReg::from3(modrm(i));
    // The in-slice target of a relative jump: the byte after the instruction,
    // plus the signed displacement.
    let rel8_target = |disp: i8| (off + bytes.len()).wrapping_add_signed(isize::from(disp));

    match idiom {
        // ---- prologue / epilogue -------------------------------------------
        // A bare frame and MSC's frameless chkstk prologue both reserve nothing.
        Idiom::Prologue | Idiom::MscChkstkFrameless => vec![LoOp::Enter { frame: 0 }],
        Idiom::PrologueLocals => vec![LoOp::Enter { frame: u16::from(bytes[5]) }],
        Idiom::MscChkstkPrologue => vec![LoOp::Enter { frame: u16_at(bytes, 4) }],
        Idiom::StackReserve2 => vec![LoOp::Enter { frame: 2 }],
        // Near epilogues differ only in whether `sp` is restored first.
        Idiom::EpilogueRestoreSp | Idiom::EpilogueNear => vec![LoOp::Leave, LoOp::Ret { far: false }],
        Idiom::EpilogueFar => vec![LoOp::Leave, LoOp::Ret { far: true }],

        // ---- the zero idioms (`ax ← 0`) ------------------------------------
        Idiom::BccZeroAx | Idiom::MscZeroAx => {
            vec![LoOp::Load { dst: R(Reg::Ax), src: Imm(0) }]
        }

        // ---- immediates into a register ------------------------------------
        Idiom::LoadImmAx => vec![LoOp::Load { dst: R(Reg::Ax), src: Imm(i32::from(u16_at(bytes, 1))) }],
        Idiom::LoadImmReg => {
            vec![LoOp::Load { dst: R(Reg::from3(bytes[0])), src: Imm(i32::from(u16_at(bytes, 1))) }]
        }

        // ---- bp-relative locals/params -------------------------------------
        Idiom::LoadLocal => vec![LoOp::Load { dst: R(reg_of(1)), src: Local(disp8_at(bytes, 2)) }],
        Idiom::StoreLocal => vec![LoOp::Store { dst: Local(disp8_at(bytes, 2)), src: R(reg_of(1)) }],
        Idiom::LoadLocalByte => {
            vec![LoOp::Load { dst: Byte(byte_reg_of(1)), src: Local(disp8_at(bytes, 2)) }]
        }
        Idiom::StoreLocalByte => {
            vec![LoOp::Store { dst: Local(disp8_at(bytes, 2)), src: Byte(byte_reg_of(1)) }]
        }
        Idiom::StoreImmLocal => {
            vec![LoOp::Store { dst: Local(disp8_at(bytes, 2)), src: Imm(i32::from(u16_at(bytes, 3))) }]
        }
        Idiom::StoreImmLocalByte => {
            vec![LoOp::StoreImmByte { dst: Local(disp8_at(bytes, 2)), imm: i32::from(bytes[3]) }]
        }
        Idiom::StoreImmGlobalByte => {
            vec![LoOp::StoreImmByte { dst: Global(u16_at(bytes, 2)), imm: i32::from(bytes[4]) }]
        }
        Idiom::LeaLocal => vec![LoOp::Lea { dst: R(reg_of(1)), src: Local(disp8_at(bytes, 2)) }],

        // ---- globals -------------------------------------------------------
        Idiom::LoadGlobal if bytes[0] == 0xa1 => {
            vec![LoOp::Load { dst: R(Reg::Ax), src: Global(u16_at(bytes, 1)) }]
        }
        Idiom::StoreGlobal if bytes[0] == 0xa3 => {
            vec![LoOp::Store { dst: Global(u16_at(bytes, 1)), src: R(Reg::Ax) }]
        }
        Idiom::LoadGlobalByte => vec![LoOp::Load { dst: Byte(ByteReg::Al), src: Global(u16_at(bytes, 1)) }],
        Idiom::StoreGlobalByte => vec![LoOp::Store { dst: Global(u16_at(bytes, 1)), src: Byte(ByteReg::Al) }],
        // the `8b/89 [disp16]` reg forms.
        Idiom::LoadGlobal => vec![LoOp::Load { dst: R(reg_of(1)), src: Global(u16_at(bytes, 2)) }],
        Idiom::StoreGlobal => vec![LoOp::Store { dst: Global(u16_at(bytes, 2)), src: R(reg_of(1)) }],
        Idiom::StoreImmGlobal => {
            vec![LoOp::Store { dst: Global(u16_at(bytes, 2)), src: Imm(i32::from(u16_at(bytes, 4))) }]
        }

        // ---- register moves and ALU ----------------------------------------
        Idiom::MovReg if bytes[0] == 0x8b => vec![LoOp::Load { dst: R(reg_of(1)), src: R(rm_of(1)) }],
        Idiom::MovReg => vec![LoOp::Load { dst: R(rm_of(1)), src: R(reg_of(1)) }], // 0x89
        Idiom::AluReg => {
            let op = alu_op(bytes[0]);
            let dst = if op == BinOp::Cmp { Place::Flags } else { R(reg_of(1)) };
            vec![LoOp::Bin { dst, op, lhs: R(reg_of(1)), rhs: R(rm_of(1)) }]
        }
        Idiom::AluLocal => {
            let op = alu_op(bytes[0]);
            let dst = if op == BinOp::Cmp { Place::Flags } else { R(reg_of(1)) };
            vec![LoOp::Bin { dst, op, lhs: R(reg_of(1)), rhs: Local(disp8_at(bytes, 2)) }]
        }
        Idiom::AluGlobal => {
            let op = alu_op(bytes[0]);
            let dst = if op == BinOp::Cmp { Place::Flags } else { R(reg_of(1)) };
            vec![LoOp::Bin { dst, op, lhs: R(reg_of(1)), rhs: Global(u16_at(bytes, 2)) }]
        }
        Idiom::AluImmByte => {
            // Byte group-1 with imm8, same operand shapes as `AluImm` — but the
            // register form is a *byte* register (a `char`).
            let m = modrm(1);
            let op = group1_op(m >> 3);
            let (lhs, imm) = match m & 0xc7 {
                0x46 => (Local(disp8_at(bytes, 2)), i32::from(bytes[3].cast_signed())),
                0x06 => (Global(u16_at(bytes, 2)), i32::from(bytes[4].cast_signed())),
                _ => (Byte(byte_rm_of(1)), i32::from(bytes[2].cast_signed())),
            };
            let dst = if op == BinOp::Cmp { Place::Flags } else { lhs };
            vec![LoOp::Bin { dst, op, lhs, rhs: Imm(imm) }]
        }
        Idiom::LoadImmByteReg => {
            vec![LoOp::Load { dst: Byte(ByteReg::from3(bytes[0])), src: Imm(i32::from(bytes[1])) }]
        }
        // `8a /r` = `mov r8, r/m8` (dst is reg); `88 /r` = `mov r/m8, r8` (dst is rm).
        Idiom::MovByteReg if bytes[0] == 0x8a => {
            vec![LoOp::Load { dst: Byte(byte_reg_of(1)), src: Byte(byte_rm_of(1)) }]
        }
        Idiom::MovByteReg => vec![LoOp::Load { dst: Byte(byte_rm_of(1)), src: Byte(byte_reg_of(1)) }],
        Idiom::AluByteReg => {
            let op = byte_alu_op(bytes[0]);
            let dst = if op == BinOp::Cmp { Place::Flags } else { Byte(byte_reg_of(1)) };
            vec![LoOp::Bin { dst, op, lhs: Byte(byte_reg_of(1)), rhs: Byte(byte_rm_of(1)) }]
        }
        Idiom::IncDecByteReg => {
            let reg = Byte(byte_rm_of(1));
            // group-4 reg field: /0 = inc, /1 = dec.
            let op = if (modrm(1) >> 3) & 7 == 1 { UnOp::Dec } else { UnOp::Inc };
            vec![LoOp::Un { dst: reg, op, operand: reg }]
        }
        Idiom::AluAxImm => {
            // The op lives in the same bit positions as the group-1 `reg` field.
            let op = group1_op(bytes[0] >> 3);
            let imm = i32::from(u16_at(bytes, 1).cast_signed());
            let dst = if op == BinOp::Cmp { Place::Flags } else { R(Reg::Ax) };
            vec![LoOp::Bin { dst, op, lhs: R(Reg::Ax), rhs: Imm(imm) }]
        }
        Idiom::AluImm => {
            // group 1 with a sign-extended imm8, against a local (`83 46 disp
            // imm`), a global (`83 06 disp16 imm`), or a register (`83 modrm imm`).
            let m = modrm(1);
            let op = group1_op(m >> 3);
            let (lhs, imm) = match m & 0xc7 {
                0x46 => (Local(disp8_at(bytes, 2)), i32::from(bytes[3].cast_signed())),
                0x06 => (Global(u16_at(bytes, 2)), i32::from(bytes[4].cast_signed())),
                _ => (R(rm_of(1)), i32::from(bytes[2].cast_signed())),
            };
            let dst = if op == BinOp::Cmp { Place::Flags } else { lhs };
            vec![LoOp::Bin { dst, op, lhs, rhs: Imm(imm) }]
        }

        // ---- group 3 (f7) and shift-by-1 (d1), both mod=11 -----------------
        Idiom::Grp3 => {
            let rm = R(rm_of(1));
            match modrm(1) >> 3 & 7 {
                0 | 1 => vec![LoOp::Bin { dst: Place::Flags, op: BinOp::Test, lhs: rm, rhs: rm }],
                2 => vec![LoOp::Un { dst: rm, op: UnOp::Not, operand: rm }],
                3 => vec![LoOp::Un { dst: rm, op: UnOp::Neg, operand: rm }],
                4 => vec![LoOp::Bin { dst: Place::DxAx, op: BinOp::Mul, lhs: R(Reg::Ax), rhs: rm }],
                5 => vec![LoOp::Bin { dst: Place::DxAx, op: BinOp::Imul, lhs: R(Reg::Ax), rhs: rm }],
                6 => vec![LoOp::Bin { dst: Place::DxAx, op: BinOp::Div, lhs: Place::DxAx, rhs: rm }],
                _ => vec![LoOp::Bin { dst: Place::DxAx, op: BinOp::Idiv, lhs: Place::DxAx, rhs: rm }],
            }
        }
        Idiom::Shift1 => {
            let rm = R(rm_of(1));
            let op = match modrm(1) >> 3 & 7 {
                0 => BinOp::Rol,
                1 => BinOp::Ror,
                2 => BinOp::Rcl,
                3 => BinOp::Rcr,
                4 => BinOp::Shl,
                5 => BinOp::Shr,
                _ => BinOp::Sar, // 7 (6 is undefined)
            };
            vec![LoOp::Bin { dst: rm, op, lhs: rm, rhs: Imm(1) }]
        }

        // ---- group 5 on a local (inc/dec/push) -----------------------------
        Idiom::Grp5Local => {
            let local = Local(disp8_at(bytes, 2));
            match modrm(1) >> 3 & 7 {
                0 => vec![LoOp::Un { dst: local, op: UnOp::Inc, operand: local }],
                1 => vec![LoOp::Un { dst: local, op: UnOp::Dec, operand: local }],
                _ => vec![LoOp::Arg { src: local }], // 6 = push
            }
        }

        // ---- promotions ----------------------------------------------------
        Idiom::Cbw => vec![LoOp::Promote { kind: Promote::Cbw }],
        Idiom::Cwd => vec![LoOp::Promote { kind: Promote::Cwd }],

        // ---- pointers (near, [si]/[di]) ------------------------------------
        Idiom::PointerLoad if bytes[0] == 0x8b => {
            vec![LoOp::Load { dst: R(reg_of(1)), src: Deref(deref_base(modrm(1))) }]
        }
        // 0x8a (byte deref)
        Idiom::PointerLoad => {
            vec![LoOp::Load { dst: Byte(byte_reg_of(1)), src: Deref(deref_base(modrm(1))) }]
        }
        Idiom::PointerStore if bytes[0] == 0x89 => {
            vec![LoOp::Store { dst: Deref(deref_base(modrm(1))), src: R(reg_of(1)) }]
        }
        // 0x88 (byte deref)
        Idiom::PointerStore => {
            vec![LoOp::Store { dst: Deref(deref_base(modrm(1))), src: Byte(byte_reg_of(1)) }]
        }

        // ---- inc/dec of a register -----------------------------------------
        Idiom::IncDecReg => {
            let reg = R(Reg::from3(bytes[0]));
            let op = if bytes[0] < 0x48 { UnOp::Inc } else { UnOp::Dec };
            vec![LoOp::Un { dst: reg, op, operand: reg }]
        }

        // ---- calls and cdecl argument handling -----------------------------
        Idiom::NearCall => vec![LoOp::Call { far: false }],
        Idiom::FarCall => vec![LoOp::Call { far: true }],
        Idiom::PushAx => vec![LoOp::Arg { src: R(Reg::Ax) }],
        Idiom::CdeclPop1 => vec![LoOp::Cleanup { bytes: 2 }],
        Idiom::CdeclPopN => vec![LoOp::Cleanup { bytes: u16::from(bytes[2]) }],
        Idiom::PopReg => vec![LoOp::Pop { dst: R(Reg::from3(bytes[0])) }],

        // ---- register variables (si/di) ------------------------------------
        Idiom::SaveRegVar => {
            vec![LoOp::SaveReg { reg: if bytes[0] == 0x56 { Reg::Si } else { Reg::Di } }]
        }
        Idiom::RestoreRegVar => {
            vec![LoOp::RestoreReg { reg: if bytes[0] == 0x5e { Reg::Si } else { Reg::Di } }]
        }

        // ---- control flow --------------------------------------------------
        Idiom::Jcc => {
            vec![LoOp::Branch { cond: Cond(bytes[0] & 0x0f), target: rel8_target(bytes[1].cast_signed()) }]
        }
        // BCC's `eb 00` exit jump folds into the general short jump: its target
        // is the immediately-following instruction (the epilogue).
        Idiom::ShortJump | Idiom::BccExitJump => {
            vec![LoOp::Jump { target: rel8_target(bytes[1].cast_signed()) }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{recompile_text, CompileOpts};

    /// Lift the `_TEXT` our own compiler produces for a snippet — the lift has to
    /// work on real BCC bytes, not hand-built ones.
    fn lift_c(src: &str) -> Vec<LoInsn> {
        let code = recompile_text(src, &CompileOpts::default()).expect("compiles");
        lift(&code)
    }

    fn ops(insns: &[LoInsn]) -> Vec<&LoOp> {
        insns.iter().map(|i| &i.op).collect()
    }

    #[test]
    fn return_zero_lifts_to_enter_load_leave_ret() {
        // `int f(){return 0;}` → push bp;mov bp,sp / xor ax,ax / [exit jmp] /
        // pop bp;ret. The xor is the zero idiom → `ax ← 0`.
        let insns = lift_c("int f() { return 0; }\n");
        let ops = ops(&insns);
        assert!(matches!(ops.first(), Some(LoOp::Enter { frame: 0 })), "starts with Enter");
        assert!(
            ops.iter().any(|o| matches!(o, LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Imm(0) })),
            "the xor ax,ax zero idiom lifts to ax ← 0",
        );
        assert!(matches!(ops.last(), Some(LoOp::Ret { far: false })), "ends with a near Ret");
    }

    #[test]
    fn lift_covers_every_byte_with_provenance() {
        // Provenance must tile the code: spans are contiguous and cover it all,
        // so a recompile mismatch at any offset maps to exactly one LoInsn.
        let code = recompile_text("int f() { return 0; }\n", &CompileOpts::default()).expect("compiles");
        let insns = lift(&code);
        // Consecutive ops may share a span (an epilogue lifts to Leave+Ret over
        // the same bytes), so the invariant is over *distinct* spans tiling.
        let mut at = 0;
        let mut last: Option<Span> = None;
        for insn in &insns {
            if last == Some(insn.span) {
                continue;
            }
            assert_eq!(insn.span.start, at, "spans must be contiguous");
            at += insn.span.len;
            last = Some(insn.span);
        }
        assert_eq!(at, code.len(), "spans must cover the whole slice");
    }

    #[test]
    fn local_store_decodes_disp_and_immediate() {
        // `x = 7;` on a local → `mov [bp-N], 7` (c7 46 N 07 00) → Store to a
        // negative local of the immediate 7.
        let insns = lift_c("int f() { int x; x = 7; return x; }\n");
        let store = insns
            .iter()
            .find_map(|i| match i.op {
                LoOp::Store { dst: Place::Local(d), src: Place::Imm(v) } => Some((d, v)),
                _ => None,
            })
            .expect("a store-immediate-to-local");
        assert!(store.0 < 0, "a local sits below bp, got disp {}", store.0);
        assert_eq!(store.1, 7, "the stored immediate");
    }

    #[test]
    fn unrecognized_bytes_become_one_asm_run() {
        // A lone byte the recognizer can't place must coalesce into a single Asm
        // op spanning it — not one Asm per byte.
        let insns = lift(&[0x90, 0x90, 0x90]); // three nops, unrecognized
        assert_eq!(insns.len(), 1, "a gap run coalesces");
        assert!(matches!(&insns[0].op, LoOp::Asm { bytes } if bytes.len() == 3));
    }

    #[test]
    fn cdecl_call_lifts_arg_call_cleanup() {
        // A call with one int arg: push the arg, call, then clean up. We just
        // require the Arg…Call…Cleanup window the §7 recovery keys on appears.
        let insns = lift_c("void g(int); void f() { g(3); }\n");
        let ops = ops(&insns);
        let call = ops.iter().position(|o| matches!(o, LoOp::Call { .. })).expect("a call");
        assert!(
            ops[..call].iter().any(|o| matches!(o, LoOp::Arg { .. })),
            "an Arg precedes the Call",
        );
        assert!(
            ops[call + 1..].iter().any(|o| matches!(o, LoOp::Cleanup { .. })),
            "a Cleanup follows the Call",
        );
    }
}
