//! Codegen idiom recognition for BCC 2.0 **and** MSC — the first step toward
//! reading a compiled binary back as the constructs its compiler emits, and
//! toward telling the two compilers apart from their code alone.
//!
//! [`recognize`] linearly decomposes a code byte slice (a `_TEXT` segment) into
//! the instruction idioms the backends produce; [`classify`] weighs the
//! compiler-distinctive ones into a verdict. This is the *decoder* counterpart
//! to the byte-exact compilers: every idiom here is a sequence one of them emits
//! for a known C construct (see `specs/bcc/ASM_OUTPUT.md`, `specs/msc/`, and
//! `specs/FINGERPRINTS.md`), grounded in real BCC/MSC output.
//!
//! It is a prototype: a curated, high-confidence idiom set, not a full
//! disassembler. Unrecognized bytes are reported as gaps, so coverage measures
//! how much of the code the catalog explains. For a *robust* compiler verdict,
//! combine this code-level evidence with the symbol/structure fingerprints the
//! rest of the crate already extracts (the `__chkstk`/`__acrtused` externs and
//! `SLIBCE` default-library COMENT that mark MSC, the `TC86` translator COMENT
//! that marks BCC).

use std::fmt::Write as _;

/// A toolchain whose codegen idioms we recognize. Each idiom is tagged with the
/// *set* of toolchains that emit it (see [`Idiom::toolchains`]); a "shared"
/// idiom is simply one whose set has more than one member, so there is no
/// dedicated `Shared` variant. (Compiler *versions* are a future refinement —
/// model a toolchain as a family plus an optional version, tag idioms at the
/// coarsest scope they're characteristic of, and resolve family before version.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compiler {
    Bcc,
    Msc,
}

/// How distinctive an idiom is of its compiler (mirrors `specs/FINGERPRINTS.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strength {
    /// A near-conclusive marker for the compiler on its own.
    Strong,
    /// Typical but shared with the era; useful only in aggregate.
    Weak,
}

/// A recognized codegen idiom — an instruction sequence and its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idiom {
    /// `55 8b ec` — `push bp; mov bp,sp` (function entry). Shared.
    Prologue,
    /// `55 8b ec 83 ec N` — BCC entry plus `sub sp,N` reserving locals.
    PrologueLocals,
    /// `55 8b ec b8 .. .. e8 .. ..` — MSC's framed prologue: entry, load the
    /// locals size, then `call __chkstk`. MSC calls chkstk in every prologue.
    MscChkstkPrologue,
    /// `33 c0 e8 .. ..` — MSC's frameless prologue: `xor ax,ax` (chkstk size 0)
    /// then `call __chkstk`, with no `bp` frame.
    MscChkstkFrameless,
    /// `8b e5 5d c3` — `mov sp,bp; pop bp; ret` (restore-sp near epilogue).
    EpilogueRestoreSp,
    /// `5d c3` — `pop bp; ret` (near epilogue).
    EpilogueNear,
    /// `5d cb` — `pop bp; retf` (far epilogue, compact/large/huge models).
    EpilogueFar,
    /// `eb 00` — `jmp $+2`: BCC emits a jump to the exit even when the epilogue
    /// is the next instruction. Distinctive of BCC; MSC doesn't.
    BccExitJump,
    /// `33 c0` — `xor ax,ax`: BCC's return-0 / zero.
    BccZeroAx,
    /// `2b c0` — `sub ax,ax`: MSC's return-0. BCC always uses `xor` (`33 c0`),
    /// so this discriminates the two.
    MscZeroAx,
    /// `b8 lo hi` — `mov ax,imm16` (load a literal or relocated address).
    LoadImmAx,
    /// `50` — `push ax` (push a cdecl argument).
    PushAx,
    /// `e8 lo hi` — `call near` (relative).
    NearCall,
    /// `9a o o s s` — `call far`.
    FarCall,
    /// `59` — `pop cx`: BCC's one-argument cdecl cleanup after a call.
    CdeclPop1,
    /// `83 c4 N` — `add sp,N`: discard N bytes of cdecl arguments (MSC's style).
    CdeclPopN,
    /// `56` / `57` — `push si` / `push di`: save a callee-saved register variable.
    SaveRegVar,
    /// `5e` / `5f` — `pop si` / `pop di`: restore a register variable.
    RestoreRegVar,
    /// `4c 4c` — `dec sp; dec sp`: BCC reserves 2 stack bytes this way (one byte
    /// shorter than `sub sp,2`).
    StackReserve2,
    /// `8b /r [bp±N]` — `mov r16, [bp±disp]`: load a local or parameter.
    LoadLocal,
    /// `89 /r [bp±N]` — `mov [bp±disp], r16`: store a local.
    StoreLocal,
    /// `8a /r [bp±N]` — `mov r8, [bp±disp]`: load a `char` local/param.
    LoadLocalByte,
    /// `88 /r [bp±N]` — `mov [bp±disp], r8`: store a `char` local.
    StoreLocalByte,
    /// `c7 46 N ii ii` — `mov [bp±disp], imm16`: store a literal to a local.
    StoreImmLocal,
    /// `c7 06 aa aa ii ii` — `mov [mem], imm16`: store a literal to a global.
    StoreImmGlobal,
    /// `a1 aa aa` — `mov ax, [mem]`: load a global into ax.
    LoadGlobal,
    /// `a3 aa aa` — `mov [mem], ax`: store ax to a global.
    StoreGlobal,
    /// `eb rr` — `jmp rel8`: a short jump (control flow; `eb 00` is the exit jump).
    ShortJump,
    /// `8b /r` or `89 /r` with mod=11 — `mov r16, r16` (register copy).
    MovReg,
    /// `b9+r ii ii` — `mov r16, imm16` for a register other than ax (often a
    /// register variable initialized to a constant; ax is [`LoadImmAx`]).
    LoadImmReg,
    /// `03/2b/0b/23/33/3b /r` with mod=11 — `add/sub/or/and/xor/cmp r16, r16`.
    AluReg,
    /// the same opcodes with `[bp±disp]` — an ALU op against a local/param.
    AluLocal,
    /// `83 /r ii` — an ALU op with a sign-extended `imm8` (`add/cmp/...`).
    AluImm,
    /// `8d /r [bp±N]` — `lea r16, [bp±disp]`: the address of a local (array /
    /// struct / `&local`).
    LeaLocal,
    /// `c6 46 N ii` — `mov [bp±disp], imm8`: store a `char` literal to a local.
    StoreImmLocalByte,
    /// `c6 06 aa aa ii` — `mov byte ptr [mem], imm8`: store a `char` literal to
    /// a global.
    StoreImmGlobalByte,
    /// `98` — `cbw`: sign-extend al→ax (the `char`→`int` promotion).
    Cbw,
    /// `f7 /r` with mod=11 — group 3 (`imul/idiv/mul/div/neg/not`).
    Grp3,
    /// `d1 /r` with mod=11 — shift/rotate a register by 1.
    Shift1,
    /// `ff /r [bp±N]` — group 5 on a local (`inc/dec/push`).
    Grp5Local,
    /// `7x rr` — a conditional jump (`jz/jnz/jl/jle/...`): an `if`/loop branch.
    Jcc,
    /// `4x` — `inc r16` / `dec r16`.
    IncDecReg,
    /// `99` — `cwd`: sign-extend ax→dx:ax (the setup for `idiv` / 32-bit).
    Cwd,
    /// `58+r` — `pop r16` (a register other than the specific cases above).
    PopReg,
    /// `8b/8a /r [si|di]` — load through a near pointer (`*p`).
    PointerLoad,
    /// `89/88 /r [si|di]` — store through a near pointer (`*p = …`).
    PointerStore,
    /// `a0 aa aa` — `mov al, [mem]`: load a `char` global.
    LoadGlobalByte,
    /// `a2 aa aa` — `mov [mem], al`: store a `char` global.
    StoreGlobalByte,
    /// `03/2b/0b/23/33/3b /r [mem]` — an ALU op against a global.
    AluGlobal,
    /// `80 /r ... ii` — a group-1 ALU op at **byte** width with an `imm8`
    /// (`cmp/add/… byte ptr [mem|reg], imm`). The byte counterpart of
    /// [`AluImm`], emitted for `char` operands.
    AluImmByte,
    /// `b0+r ii` — `mov r8, imm8`: load a byte literal into a byte register
    /// (often a `char` register variable like `dl`).
    LoadImmByteReg,
    /// `8a /r` or `88 /r` with mod=11 — `mov r8, r8` (a byte register copy,
    /// e.g. `mov al, dl` reading a `char` register variable into the accumulator).
    MovByteReg,
    /// `02/0a/22/2a/32/3a /r` with mod=11 — a byte ALU reg-reg op
    /// (`add/or/and/sub/xor/cmp r8, r8`); `or dl,dl` is the `char` truthiness test.
    AluByteReg,
    /// `05/0d/15/1d/25/2d/35/3d ii ii` — an ALU op on `ax` with an `imm16`
    /// (`add/or/adc/sbb/and/sub/xor/cmp ax, imm`). The accumulator-specific
    /// short encoding BCC/TASM prefer over `81 /r` for `ax`.
    AluAxImm,
}

impl Idiom {
    /// The toolchains that emit this idiom. A single-element set is a
    /// discriminator; a multi-element set is structural (shared) and tells the
    /// toolchains apart from nothing.
    #[must_use]
    pub fn toolchains(self) -> &'static [Compiler] {
        use Compiler::{Bcc, Msc};
        match self {
            Idiom::PrologueLocals
            | Idiom::BccExitJump
            | Idiom::BccZeroAx
            | Idiom::CdeclPop1
            | Idiom::StackReserve2 => &[Bcc],
            Idiom::MscChkstkPrologue | Idiom::MscChkstkFrameless | Idiom::MscZeroAx => &[Msc],
            _ => &[Bcc, Msc],
        }
    }

    /// The single toolchain this idiom is exclusive to (its set is a singleton),
    /// if any — i.e. the toolchain it discriminates in favor of.
    #[must_use]
    pub fn exclusive_to(self) -> Option<Compiler> {
        match self.toolchains() {
            [only] => Some(*only),
            _ => None,
        }
    }

    /// How strongly this idiom points at its compiler.
    #[must_use]
    pub fn strength(self) -> Strength {
        match self {
            // Near-conclusive: the redundant exit jump (BCC) and the chkstk
            // prologue / sub-based zero (MSC) don't appear in the other.
            Idiom::BccExitJump
            | Idiom::MscChkstkPrologue
            | Idiom::MscChkstkFrameless
            | Idiom::MscZeroAx => Strength::Strong,
            _ => Strength::Weak,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Idiom::Prologue => "prologue (push bp; mov bp,sp)",
            Idiom::PrologueLocals => "BCC prologue + sub sp (reserve locals)",
            Idiom::MscChkstkPrologue => "MSC prologue + call __chkstk",
            Idiom::MscChkstkFrameless => "MSC frameless prologue + call __chkstk",
            Idiom::EpilogueRestoreSp => "epilogue (mov sp,bp; pop bp; ret)",
            Idiom::EpilogueNear => "epilogue (pop bp; ret)",
            Idiom::EpilogueFar => "epilogue (pop bp; retf)",
            Idiom::BccExitJump => "BCC exit jump (jmp $+2)",
            Idiom::BccZeroAx => "BCC return 0 (xor ax,ax)",
            Idiom::MscZeroAx => "MSC return 0 (sub ax,ax)",
            Idiom::LoadImmAx => "load ax,imm16",
            Idiom::PushAx => "push ax (arg)",
            Idiom::NearCall => "call near",
            Idiom::FarCall => "call far",
            Idiom::CdeclPop1 => "cdecl cleanup (pop cx)",
            Idiom::CdeclPopN => "cdecl cleanup (add sp,N)",
            Idiom::SaveRegVar => "save register var (push si/di)",
            Idiom::RestoreRegVar => "restore register var (pop si/di)",
            Idiom::StackReserve2 => "reserve 2 stack bytes (dec sp; dec sp)",
            Idiom::LoadLocal => "load local/param (mov r16, [bp±N])",
            Idiom::StoreLocal => "store local (mov [bp±N], r16)",
            Idiom::LoadLocalByte => "load char local (mov r8, [bp±N])",
            Idiom::StoreLocalByte => "store char local (mov [bp±N], r8)",
            Idiom::StoreImmLocal => "store imm to local (mov [bp±N], imm16)",
            Idiom::StoreImmGlobal => "store imm to global (mov [mem], imm16)",
            Idiom::LoadGlobal => "load global (mov ax, [mem])",
            Idiom::StoreGlobal => "store global (mov [mem], ax)",
            Idiom::ShortJump => "short jump (jmp rel8)",
            Idiom::MovReg => "mov reg, reg",
            Idiom::LoadImmReg => "load reg, imm16",
            Idiom::AluReg => "alu reg, reg (add/sub/or/and/xor/cmp)",
            Idiom::AluLocal => "alu reg, local",
            Idiom::AluImm => "alu r/m, imm8",
            Idiom::LeaLocal => "lea (address of local)",
            Idiom::StoreImmLocalByte => "store char imm to local (mov [bp±N], imm8)",
            Idiom::StoreImmGlobalByte => "store char imm to global (mov [mem], imm8)",
            Idiom::Cbw => "cbw (sign-extend al→ax)",
            Idiom::Grp3 => "grp3 (imul/idiv/neg/not)",
            Idiom::Shift1 => "shift/rotate by 1",
            Idiom::Grp5Local => "grp5 on local (inc/dec/push)",
            Idiom::Jcc => "conditional jump (jcc rel8)",
            Idiom::IncDecReg => "inc/dec reg",
            Idiom::Cwd => "cwd (sign-extend ax→dx:ax)",
            Idiom::PopReg => "pop reg",
            Idiom::PointerLoad => "load via pointer (mov r16, [si/di])",
            Idiom::PointerStore => "store via pointer (mov [si/di], r16)",
            Idiom::LoadGlobalByte => "load global byte (mov al, [mem])",
            Idiom::StoreGlobalByte => "store global byte (mov [mem], al)",
            Idiom::AluGlobal => "alu reg, global",
            Idiom::AluImmByte => "alu byte [mem|reg], imm8",
            Idiom::LoadImmByteReg => "load imm into byte reg (mov r8, imm8)",
            Idiom::MovByteReg => "byte reg copy (mov r8, r8)",
            Idiom::AluByteReg => "alu byte reg, reg",
            Idiom::AluAxImm => "alu ax, imm16",
        }
    }
}

/// One byte of an idiom pattern: a fixed value, an operand wildcard, or a
/// masked match (`byte & mask == value`) — e.g. a ModR/M byte whose `reg` field
/// is free but whose mode/`rm` select `[bp±disp]`.
#[derive(Clone, Copy)]
enum Bm {
    Lit(u8),
    Any,
    Mask(u8, u8),
}

struct Def {
    idiom: Idiom,
    pat: &'static [Bm],
}

use Bm::{Any as A, Lit as L, Mask as M};

/// ModR/M byte selecting `[bp+disp8]` with any `reg` field (mod=01, rm=110).
const BP_DISP8: Bm = M(0xc7, 0x46);
/// ModR/M byte selecting a register operand (mod=11), any `reg`/`rm`.
const REG: Bm = M(0xc0, 0xc0);
/// ModR/M byte selecting a direct `[disp16]` global (mod=00, rm=110).
const DISP16: Bm = M(0xc7, 0x06);
/// ModR/M byte selecting `[si]` or `[di]` (mod=00, rm=10x) — a near pointer.
const PTR: Bm = M(0xc6, 0x04);

/// The idiom catalog, ordered most-specific-first so a longer idiom wins over a
/// prefix of it at the same offset (e.g. `MscChkstkPrologue` and
/// `PrologueLocals` before `Prologue`; `MscChkstkFrameless` before `BccZeroAx`;
/// `BccExitJump` (`eb 00`) before the general `ShortJump`). MSC's chkstk
/// prologue is its `b8.. e8..` — the call follows the size load *immediately*,
/// where BCC would push an argument (`50`) in between.
const IDIOMS: &[Def] = &[
    Def { idiom: Idiom::MscChkstkPrologue, pat: &[L(0x55), L(0x8b), L(0xec), L(0xb8), A, A, L(0xe8), A, A] },
    Def { idiom: Idiom::PrologueLocals, pat: &[L(0x55), L(0x8b), L(0xec), L(0x83), L(0xec), A] },
    Def { idiom: Idiom::Prologue, pat: &[L(0x55), L(0x8b), L(0xec)] },
    Def { idiom: Idiom::EpilogueRestoreSp, pat: &[L(0x8b), L(0xe5), L(0x5d), L(0xc3)] },
    Def { idiom: Idiom::EpilogueNear, pat: &[L(0x5d), L(0xc3)] },
    Def { idiom: Idiom::EpilogueFar, pat: &[L(0x5d), L(0xcb)] },
    Def { idiom: Idiom::BccExitJump, pat: &[L(0xeb), L(0x00)] },
    Def { idiom: Idiom::MscChkstkFrameless, pat: &[L(0x33), L(0xc0), L(0xe8), A, A] },
    Def { idiom: Idiom::BccZeroAx, pat: &[L(0x33), L(0xc0)] },
    Def { idiom: Idiom::MscZeroAx, pat: &[L(0x2b), L(0xc0)] },
    Def { idiom: Idiom::StackReserve2, pat: &[L(0x4c), L(0x4c)] }, // before IncDecReg
    Def { idiom: Idiom::Cbw, pat: &[L(0x98)] },
    Def { idiom: Idiom::Cwd, pat: &[L(0x99)] },
    Def { idiom: Idiom::FarCall, pat: &[L(0x9a), A, A, A, A] },
    // store a literal (word / byte) to a local or global.
    Def { idiom: Idiom::StoreImmLocal, pat: &[L(0xc7), L(0x46), A, A, A] },
    Def { idiom: Idiom::StoreImmGlobal, pat: &[L(0xc7), L(0x06), A, A, A, A] },
    Def { idiom: Idiom::StoreImmLocalByte, pat: &[L(0xc6), L(0x46), A, A] },
    Def { idiom: Idiom::StoreImmGlobalByte, pat: &[L(0xc6), L(0x06), A, A, A] },
    // bp-relative loads/stores (word and byte), and lea of a local.
    Def { idiom: Idiom::LoadLocal, pat: &[L(0x8b), BP_DISP8, A] },
    Def { idiom: Idiom::StoreLocal, pat: &[L(0x89), BP_DISP8, A] },
    Def { idiom: Idiom::LoadLocalByte, pat: &[L(0x8a), BP_DISP8, A] },
    Def { idiom: Idiom::StoreLocalByte, pat: &[L(0x88), BP_DISP8, A] },
    Def { idiom: Idiom::LeaLocal, pat: &[L(0x8d), BP_DISP8, A] },
    Def { idiom: Idiom::Grp5Local, pat: &[L(0xff), BP_DISP8, A] },
    // pointer load/store (mov r/r8, [si|di]); before MovReg's mod=11.
    Def { idiom: Idiom::PointerLoad, pat: &[L(0x8b), PTR] },
    Def { idiom: Idiom::PointerLoad, pat: &[L(0x8a), PTR] },
    Def { idiom: Idiom::PointerStore, pat: &[L(0x89), PTR] },
    Def { idiom: Idiom::PointerStore, pat: &[L(0x88), PTR] },
    // global loads/stores: accumulator-direct (a0-a3) and reg via [disp16].
    Def { idiom: Idiom::LoadGlobal, pat: &[L(0xa1), A, A] },
    Def { idiom: Idiom::StoreGlobal, pat: &[L(0xa3), A, A] },
    Def { idiom: Idiom::LoadGlobalByte, pat: &[L(0xa0), A, A] },
    Def { idiom: Idiom::StoreGlobalByte, pat: &[L(0xa2), A, A] },
    Def { idiom: Idiom::LoadGlobal, pat: &[L(0x8b), DISP16, A, A] },
    Def { idiom: Idiom::StoreGlobal, pat: &[L(0x89), DISP16, A, A] },
    // byte reg-reg mov (mov r8,r8, mod=11) — before nothing else uses 8a/88 REG.
    Def { idiom: Idiom::MovByteReg, pat: &[L(0x8a), REG] },
    Def { idiom: Idiom::MovByteReg, pat: &[L(0x88), REG] },
    // byte ALU reg-reg (the r8,rm8 forms): add/or/and/sub/xor/cmp.
    Def { idiom: Idiom::AluByteReg, pat: &[L(0x02), REG] },
    Def { idiom: Idiom::AluByteReg, pat: &[L(0x0a), REG] },
    Def { idiom: Idiom::AluByteReg, pat: &[L(0x22), REG] },
    Def { idiom: Idiom::AluByteReg, pat: &[L(0x2a), REG] },
    Def { idiom: Idiom::AluByteReg, pat: &[L(0x32), REG] },
    Def { idiom: Idiom::AluByteReg, pat: &[L(0x3a), REG] },
    // reg-to-reg mov, and ALU reg,reg / reg,local (add/sub/or/and/xor/cmp).
    Def { idiom: Idiom::MovReg, pat: &[L(0x8b), REG] },
    Def { idiom: Idiom::MovReg, pat: &[L(0x89), REG] },
    Def { idiom: Idiom::AluReg, pat: &[L(0x03), REG] },
    Def { idiom: Idiom::AluReg, pat: &[L(0x2b), REG] },
    Def { idiom: Idiom::AluReg, pat: &[L(0x0b), REG] },
    Def { idiom: Idiom::AluReg, pat: &[L(0x23), REG] },
    Def { idiom: Idiom::AluReg, pat: &[L(0x33), REG] },
    Def { idiom: Idiom::AluReg, pat: &[L(0x3b), REG] },
    Def { idiom: Idiom::AluLocal, pat: &[L(0x03), BP_DISP8, A] },
    Def { idiom: Idiom::AluLocal, pat: &[L(0x2b), BP_DISP8, A] },
    Def { idiom: Idiom::AluLocal, pat: &[L(0x0b), BP_DISP8, A] },
    Def { idiom: Idiom::AluLocal, pat: &[L(0x23), BP_DISP8, A] },
    Def { idiom: Idiom::AluLocal, pat: &[L(0x33), BP_DISP8, A] },
    Def { idiom: Idiom::AluLocal, pat: &[L(0x3b), BP_DISP8, A] },
    Def { idiom: Idiom::AluGlobal, pat: &[L(0x03), DISP16, A, A] },
    Def { idiom: Idiom::AluGlobal, pat: &[L(0x2b), DISP16, A, A] },
    Def { idiom: Idiom::AluGlobal, pat: &[L(0x0b), DISP16, A, A] },
    Def { idiom: Idiom::AluGlobal, pat: &[L(0x23), DISP16, A, A] },
    Def { idiom: Idiom::AluGlobal, pat: &[L(0x33), DISP16, A, A] },
    Def { idiom: Idiom::AluGlobal, pat: &[L(0x3b), DISP16, A, A] },
    // group-1 ALU with imm8 (reg or local); CdeclPopN (`83 c4`) wins first.
    Def { idiom: Idiom::CdeclPopN, pat: &[L(0x83), L(0xc4), A] },
    Def { idiom: Idiom::AluImm, pat: &[L(0x83), BP_DISP8, A, A] },
    Def { idiom: Idiom::AluImm, pat: &[L(0x83), DISP16, A, A, A] }, // alu [disp16], imm8 (global)
    Def { idiom: Idiom::AluImm, pat: &[L(0x83), REG, A] },
    // byte group-1 with imm8 (local / global / register) — `char` operands.
    Def { idiom: Idiom::AluImmByte, pat: &[L(0x80), BP_DISP8, A, A] },
    Def { idiom: Idiom::AluImmByte, pat: &[L(0x80), DISP16, A, A, A] },
    Def { idiom: Idiom::AluImmByte, pat: &[L(0x80), REG, A] },
    // alu ax, imm16 — the accumulator short forms (05/0d/15/1d/25/2d/35/3d), all
    // `00xxx101`, distinguished by the `reg`-like bits from the 81/83 groups.
    Def { idiom: Idiom::AluAxImm, pat: &[M(0xc7, 0x05), A, A] },
    // unary group 3 (imul/idiv/...) and shift-by-1.
    Def { idiom: Idiom::Grp3, pat: &[L(0xf7), REG] },
    Def { idiom: Idiom::Shift1, pat: &[L(0xd1), REG] },
    Def { idiom: Idiom::NearCall, pat: &[L(0xe8), A, A] },
    Def { idiom: Idiom::LoadImmAx, pat: &[L(0xb8), A, A] }, // ax-specific; before LoadImmReg
    Def { idiom: Idiom::LoadImmReg, pat: &[M(0xf8, 0xb8), A, A] },
    Def { idiom: Idiom::LoadImmByteReg, pat: &[M(0xf8, 0xb0), A] }, // mov r8, imm8 (b0-b7)
    Def { idiom: Idiom::ShortJump, pat: &[L(0xeb), A] },
    Def { idiom: Idiom::Jcc, pat: &[M(0xf0, 0x70), A] }, // 70-7f conditional jumps
    Def { idiom: Idiom::IncDecReg, pat: &[M(0xf0, 0x40)] }, // 40-4f; after StackReserve2
    Def { idiom: Idiom::SaveRegVar, pat: &[M(0xfe, 0x56)] }, // push si / push di
    Def { idiom: Idiom::RestoreRegVar, pat: &[M(0xfe, 0x5e)] }, // pop si / pop di
    Def { idiom: Idiom::PushAx, pat: &[L(0x50)] },
    Def { idiom: Idiom::CdeclPop1, pat: &[L(0x59)] },
    Def { idiom: Idiom::PopReg, pat: &[M(0xf8, 0x58)] }, // 58-5f; after the specific pops
];

fn matches_at(code: &[u8], at: usize, pat: &[Bm]) -> bool {
    code.len() - at >= pat.len()
        && pat.iter().enumerate().all(|(k, m)| match m {
            Bm::Lit(b) => code[at + k] == *b,
            Bm::Any => true,
            Bm::Mask(mask, value) => code[at + k] & mask == *value,
        })
}

/// One recognized idiom at a byte offset within the scanned code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdiomMatch {
    pub offset: usize,
    pub len: usize,
    pub idiom: Idiom,
}

/// Linearly decompose `code` (a `_TEXT` segment) into recognized idioms. Scans
/// left to right, consuming the longest matching idiom at each position and
/// skipping one byte where nothing matches (reported as coverage gaps).
#[must_use]
pub fn recognize(code: &[u8]) -> Vec<IdiomMatch> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < code.len() {
        if let Some(def) = IDIOMS.iter().find(|d| matches_at(code, i, d.pat)) {
            out.push(IdiomMatch { offset: i, len: def.pat.len(), idiom: def.idiom });
            i += def.pat.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Which compiler the code idioms point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Bcc,
    Msc,
    /// Distinctive idioms for both compilers appeared — inconclusive on code
    /// alone (e.g. mixed objects, or a false positive); consult symbol evidence.
    Ambiguous,
    /// No distinctive idiom for either compiler was found.
    Unknown,
}

/// The result of [`classify`]: a verdict plus the distinctive-idiom tallies it
/// rests on and the full idiom decomposition.
#[derive(Debug, Clone)]
pub struct Classification {
    pub verdict: Verdict,
    /// Count of BCC-distinctive (`Strong`) idiom hits.
    pub bcc_evidence: usize,
    /// Count of MSC-distinctive (`Strong`) idiom hits.
    pub msc_evidence: usize,
    pub matches: Vec<IdiomMatch>,
}

/// Decide, from code idioms alone, whether `code` was produced by BCC or MSC.
#[must_use]
pub fn classify(code: &[u8]) -> Classification {
    let matches = recognize(code);
    // Evidence for a toolchain = strong idioms it alone emits (exclusive set).
    let exclusive_strong = |t: Compiler| {
        matches
            .iter()
            .filter(|m| m.idiom.strength() == Strength::Strong && m.idiom.exclusive_to() == Some(t))
            .count()
    };
    let bcc_evidence = exclusive_strong(Compiler::Bcc);
    let msc_evidence = exclusive_strong(Compiler::Msc);
    Classification { verdict: decide_verdict(bcc_evidence, msc_evidence), bcc_evidence, msc_evidence, matches }
}

/// How many times one toolchain's evidence must exceed the other's to win
/// outright. A real program accrues a few coincidental matches for the *wrong*
/// compiler (data bytes that happen to look like an idiom), so a verdict needs
/// clear dominance, not just a nonzero lead — e.g. the JETPACK game scores 61
/// BCC idioms to 2 MSC, which is BCC, not "ambiguous".
const DOMINANCE: usize = 3;

fn decide_verdict(bcc: usize, msc: usize) -> Verdict {
    if bcc == 0 && msc == 0 {
        Verdict::Unknown
    } else if bcc > msc && bcc >= msc.saturating_mul(DOMINANCE) {
        Verdict::Bcc
    } else if msc > bcc && msc >= bcc.saturating_mul(DOMINANCE) {
        Verdict::Msc
    } else {
        Verdict::Ambiguous
    }
}

/// Fraction of `code` bytes the idiom catalog explained (0.0–1.0).
#[must_use]
#[allow(clippy::cast_precision_loss)] // byte counts are exact in f32
pub fn coverage(code: &[u8]) -> f32 {
    if code.is_empty() {
        return 0.0;
    }
    let matched: usize = recognize(code).iter().map(|m| m.len).sum();
    matched as f32 / code.len() as f32
}

/// A human-readable lift of `code`: one line per recognized idiom (with operand
/// values where they're part of the idiom), and `??` lines for gap bytes.
#[must_use]
pub fn summarize(code: &[u8]) -> String {
    let mut out = String::new();
    let mut covered = 0usize;
    for m in recognize(code) {
        while covered < m.offset {
            let _ = writeln!(out, "  {covered:#06x}: ?? {:02x}", code[covered]);
            covered += 1;
        }
        let bytes = &code[m.offset..m.offset + m.len];
        let detail = operand_detail(m.idiom, bytes);
        let _ = writeln!(out, "  {:#06x}: {:<14} {}{detail}", m.offset, hex(bytes), m.idiom.label());
        covered = m.offset + m.len;
    }
    while covered < code.len() {
        let _ = writeln!(out, "  {covered:#06x}: ?? {:02x}", code[covered]);
        covered += 1;
    }
    out
}

fn operand_detail(idiom: Idiom, bytes: &[u8]) -> String {
    match idiom {
        Idiom::LoadImmAx if bytes.len() == 3 => {
            format!("  → ax = {:#06x}", u16::from(bytes[1]) | (u16::from(bytes[2]) << 8))
        }
        Idiom::MscChkstkPrologue if bytes.len() == 9 => {
            format!("  → {} bytes of locals", u16::from(bytes[4]) | (u16::from(bytes[5]) << 8))
        }
        Idiom::CdeclPopN if bytes.len() == 3 => format!("  → sp += {}", bytes[2]),
        Idiom::PrologueLocals if bytes.len() == 6 => format!("  → {} bytes of locals", bytes[5]),
        _ => String::new(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Read an OMF index (1 byte, or 2 when the high bit of the first is set).
fn take_index(p: &[u8], i: &mut usize) -> u16 {
    let v = u16::from(p.get(*i).copied().unwrap_or(0));
    if v & 0x80 != 0 {
        let lo = u16::from(p.get(*i + 1).copied().unwrap_or(0));
        *i += 2;
        ((v & 0x7f) << 8) | lo
    } else {
        *i += 1;
        v
    }
}

/// Extract the first CODE-class segment's bytes (the `_TEXT`) from an OMF object
/// module, for idiom analysis — unlike the first LEDATA, this skips the CONST /
/// `_DATA` records MSC emits before `_TEXT`. Empty if there is no CODE segment.
#[must_use]
pub fn code_of_obj(obj: &[u8]) -> Vec<u8> {
    let mut reader = obj::ObjReader::new(obj);
    let mut lnames: Vec<String> = vec![String::new()];
    let mut seg_count: u8 = 0;
    let mut first_code_seg: Option<u8> = None;
    let mut code: Vec<u8> = Vec::new();
    while let Ok(Some(rec)) = reader.next() {
        match rec.ty {
            obj::LNAMES => {
                let p = rec.payload;
                let mut i = 0;
                while i < p.len() {
                    let len = usize::from(p[i]);
                    let end = (i + 1 + len).min(p.len());
                    lnames.push(String::from_utf8_lossy(&p[i + 1..end]).into_owned());
                    i = end;
                }
            }
            obj::SEGDEF_16 => {
                let p = rec.payload;
                let mut i = 1; // skip ACBP
                if p.first().is_some_and(|a| (a >> 5) == 0) {
                    i += 3; // absolute: frame + offset
                }
                i += 2; // length
                let _name = take_index(p, &mut i);
                let class = take_index(p, &mut i);
                seg_count += 1;
                let is_code = lnames.get(usize::from(class)).is_some_and(|c| c == "CODE");
                if is_code && first_code_seg.is_none() {
                    first_code_seg = Some(seg_count);
                }
            }
            obj::LEDATA_16 => {
                let p = rec.payload;
                if p.len() >= 3 && Some(p[0]) == first_code_seg {
                    let off = usize::from(p[1]) | (usize::from(p[2]) << 8);
                    let data = &p[3..];
                    if off + data.len() > code.len() {
                        code.resize(off + data.len(), 0);
                    }
                    code[off..off + data.len()].copy_from_slice(data);
                }
            }
            _ => {}
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::Idiom::*;
    use super::*;

    fn kinds(code: &[u8]) -> Vec<Idiom> {
        recognize(code).into_iter().map(|m| m.idiom).collect()
    }

    // --- BCC samples (real bytes from the tracked BCC objects) ---

    /// `int main(){return 0;}` (small model, MAIN.OBJ): entry, BCC return 0,
    /// the redundant exit jump, near epilogue — and classified as BCC.
    #[test]
    fn bcc_return_zero() {
        let code = [0x55, 0x8b, 0xec, 0x33, 0xc0, 0xeb, 0x00, 0x5d, 0xc3];
        assert_eq!(kinds(&code), [Prologue, BccZeroAx, BccExitJump, EpilogueNear]);
        let c = classify(&code);
        assert_eq!(c.verdict, Verdict::Bcc);
        assert!(coverage(&code) >= 1.0, "fully recognized");
    }

    /// `printf("...")` (HELLO.OBJ): the cdecl call shape — load, **push**, call,
    /// pop-cx cleanup. The push between load and call keeps it from looking like
    /// MSC's chkstk prologue; the exit jump classifies it BCC.
    #[test]
    fn bcc_printf_call() {
        let code = [
            0x55, 0x8b, 0xec, 0xb8, 0x00, 0x00, 0x50, 0xe8, 0x00, 0x00, 0x59, 0x33, 0xc0, 0xeb, 0x00,
            0x5d, 0xc3,
        ];
        assert_eq!(
            kinds(&code),
            [Prologue, LoadImmAx, PushAx, NearCall, CdeclPop1, BccZeroAx, BccExitJump, EpilogueNear],
        );
        assert_eq!(classify(&code).verdict, Verdict::Bcc);
    }

    /// Large model (MAIN_L.OBJ): same shape, far epilogue (`retf`).
    #[test]
    fn bcc_far_model() {
        let code = [0x55, 0x8b, 0xec, 0x33, 0xc0, 0xeb, 0x00, 0x5d, 0xcb];
        assert_eq!(kinds(&code), [Prologue, BccZeroAx, BccExitJump, EpilogueFar]);
        assert_eq!(classify(&code).verdict, Verdict::Bcc);
    }

    // --- MSC samples (real bytes from our byte-exact MSC compiler) ---

    /// The verdict needs clear dominance, not just a nonzero lead — so a real
    /// program's coincidental wrong-compiler matches don't force "ambiguous".
    #[test]
    fn verdict_margin() {
        use super::Verdict::{Ambiguous, Bcc, Msc, Unknown};
        assert_eq!(decide_verdict(0, 0), Unknown);
        assert_eq!(decide_verdict(1, 0), Bcc);
        assert_eq!(decide_verdict(61, 2), Bcc); // the JETPACK game: dominant despite noise
        assert_eq!(decide_verdict(0, 2), Msc);
        assert_eq!(decide_verdict(2, 2), Ambiguous);
        assert_eq!(decide_verdict(5, 2), Ambiguous); // a lead, but not 3x dominance
    }

    /// `int z(void){return 0;}` (MZ.OBJ): the frameless chkstk prologue then
    /// MSC's `sub ax,ax` zero — both MSC-distinctive.
    #[test]
    fn msc_return_zero() {
        let code = [0x33, 0xc0, 0xe8, 0x00, 0x00, 0x2b, 0xc0, 0xc3];
        assert_eq!(kinds(&code), [MscChkstkFrameless, MscZeroAx]); // trailing c3 is a gap
        let c = classify(&code);
        assert_eq!(c.verdict, Verdict::Msc);
        assert_eq!(c.msc_evidence, 2);
    }

    /// `int l(int a){int b; b=a+1; return b;}` (ML.OBJ): the framed chkstk
    /// prologue (`b8 size; call __chkstk`) classifies it MSC.
    #[test]
    fn msc_framed_chkstk() {
        let code = [
            0x55, 0x8b, 0xec, 0xb8, 0x02, 0x00, 0xe8, 0x00, 0x00, 0x8b, 0x46, 0x04, 0x40, 0x89, 0x46,
            0xfe, 0x8b, 0xe5, 0x5d, 0xc3,
        ];
        assert_eq!(kinds(&code)[0], MscChkstkPrologue);
        assert_eq!(kinds(&code).last(), Some(&EpilogueRestoreSp));
        assert_eq!(classify(&code).verdict, Verdict::Msc);
    }

    /// `int c(void){return g(7);}` (MC.OBJ): frameless chkstk, then a cdecl call
    /// cleaned with `add sp,2` (MSC's style, vs BCC's `pop cx`).
    #[test]
    fn msc_cdecl_call() {
        let code = [
            0x33, 0xc0, 0xe8, 0x00, 0x00, 0xb8, 0x07, 0x00, 0x50, 0xe8, 0x00, 0x00, 0x83, 0xc4, 0x02,
            0xc3,
        ];
        assert_eq!(kinds(&code), [MscChkstkFrameless, LoadImmAx, PushAx, NearCall, CdeclPopN]);
        assert_eq!(classify(&code).verdict, Verdict::Msc);
    }
}
