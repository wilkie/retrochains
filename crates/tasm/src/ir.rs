//! In-memory representation of an assembled module. The parser builds
//! this; the emitter walks it.

/// One assembled translation unit, in the order BCC emits records.
#[derive(Debug, Default)]
pub struct Module {
    /// Module name, from the `?debug S "<name>"` directive (lowercased
    /// source basename). BCC always emits one; if absent we fall back
    /// to the empty string and the THEADR is empty.
    pub source_name: String,
    /// Raw `?debug C` payloads in source order. Each becomes its own
    /// COMENT record (class = first byte of the hex blob).
    pub debug_comments: Vec<Vec<u8>>,
    /// Segments in declaration order. The first declaration "wins"
    /// for SEGDEF emission; later `<name> segment ...` lines are
    /// just re-opens that may add data.
    pub segments: Vec<Segment>,
    /// Groups (currently only `DGROUP = _DATA, _BSS` is observed).
    pub groups: Vec<Group>,
    /// `public _name` declarations in source order.
    pub publics: Vec<String>,
    /// `extrn _name:near` declarations in source order.
    pub externs: Vec<String>,
}

#[derive(Debug)]
pub struct Segment {
    pub name: String,
    pub align: SegAlign,
    pub combine: SegCombine,
    pub class: String,
    /// Sequential items inside the segment, across all `segment ... ends`
    /// re-opens. Order matches source order.
    pub items: Vec<SegItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegAlign {
    Byte,
    Word,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegCombine {
    Public,
}

#[derive(Debug)]
pub enum SegItem {
    /// `<name>:` (jump target) or `<name> label byte|word` (anchor like
    /// `d@`, `s@`). Both define a name at the current offset; the width
    /// of an anchor doesn't matter for offset arithmetic, so we collapse
    /// them into one variant.
    Label(String),
    /// A machine instruction. Encoded into bytes during emit.
    Instr(Instr),
    /// A `db` directive carrying concrete byte values. Goes into the
    /// LEDATA for the segment.
    Db(Vec<u8>),
    /// `db N dup (?)` — reserve N bytes of uninitialized space. Grows
    /// the segment's notional size but emits nothing into LEDATA. BCC
    /// uses this in `_BSS` for globals.
    Pad(u32),
    /// A `proc near` / `endp` pair. We record only the `proc` start
    /// (with its name, kept for future debug-record emission) and
    /// treat `endp` as a noop separator — proc scoping doesn't affect
    /// OMF output beyond labels.
    Proc(#[allow(dead_code)] String),
    EndProc,
}

#[derive(Debug)]
pub struct Group {
    pub name: String,
    pub segments: Vec<String>,
}

/// One x86 instruction in the BCC subset. Each variant carries just
/// enough operand info for `encode.rs` to produce bytes. As we add
/// fixtures we widen this enum.
#[derive(Debug)]
pub enum Instr {
    /// `push bp`
    PushBp,
    /// `pop bp`
    PopBp,
    /// `mov bp,sp`
    MovBpSp,
    /// `mov sp,bp`
    MovSpBp,
    /// `xor ax,ax`
    XorAxAx,
    /// `mov ax,<imm16>`
    MovAxImm(u16),
    /// `sub sp,<imm8>` — encoded as 83 EC ii (sign-extended imm8 form).
    /// Larger immediates would require the 81 EC encoding; BCC uses
    /// 83 EC for the small frame sizes we've seen (≤ 127 bytes).
    SubSpImm(u8),
    /// `dec sp` — 1 byte, encoded as 4C.
    DecSp,
    /// `mov word ptr [bp+<offset>],<imm16>` — BCC uses signed
    /// offsets (negative for locals, positive for params).
    MovBpRelImm { offset: i16, imm: u16 },
    /// `mov ax,word ptr [bp+<offset>]`
    MovAxBpRel { offset: i16 },
    /// `add ax,word ptr [bp+<offset>]` — 03 46 dd
    AddAxBpRel { offset: i16 },
    /// `sub ax,word ptr [bp+<offset>]` — 2B 46 dd
    SubAxBpRel { offset: i16 },
    /// `and ax,word ptr [bp+<offset>]` — 23 46 dd
    AndAxBpRel { offset: i16 },
    /// `or ax,word ptr [bp+<offset>]` — 0B 46 dd
    OrAxBpRel { offset: i16 },
    /// `xor ax,word ptr [bp+<offset>]` — 33 46 dd
    XorAxBpRel { offset: i16 },
    /// `cmp ax,word ptr [bp+<offset>]` — 3B 46 dd
    CmpAxBpRel { offset: i16 },
    /// `j<cc> short <label>` — Jcc rel8 family.
    JmpCondShort { cond: JmpCond, target: String },
    /// `jmp short <label>`
    JmpShort(String),
    /// `call near ptr <label>` — E8 rel16. Intra-segment near call.
    CallNear(String),
    /// `mov ax,word ptr <group>:<symbol>` — segment-relative load
    /// against a group-anchored data symbol. Emits `A1 lo hi` plus a
    /// FIXUPP request (frame = group, target = symbol's home segment).
    MovAxGroupSym { group: String, symbol: String },
    /// `mov ax,offset <group>:<symbol>` — load AX with the address
    /// (offset within the group) of a data symbol. Emits `B8 lo hi`
    /// plus the same SegRelGroupTarget FIXUPP. Used for passing
    /// string literals and globals by-reference (fixture 108).
    MovAxOffsetGroupSym { group: String, symbol: String },
    /// `mov word ptr [bp+<offset>],offset <symbol>` — store a
    /// function or data symbol's address into a stack local. Emits
    /// `C7 46 dd lo hi` plus a SegRelTargetFrameSegment FIXUPP. Used
    /// for function-pointer init (fixture 110).
    MovBpRelOffsetSym { offset: i16, symbol: String },
    /// `call word ptr [bp+<offset>]` — indirect near call through a
    /// stack-resident function pointer. Emits `FF 56 dd`. No FIXUPP
    /// (the address is loaded from the local at runtime).
    CallIndirectBpRel { offset: i16 },
    /// `push ax`
    PushAx,
    /// `pop cx` — used to clean up cdecl-pushed arguments after a call.
    PopCx,
    /// `ret`
    Ret,
}

/// Signed-comparison conditional-jump opcodes. The byte encoding of
/// each is `0x7X` where X is the low nibble, supplied by `opcode_byte`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JmpCond {
    /// `je` / `jz` — zero flag set (equal)
    E,
    /// `jne` / `jnz` — zero flag clear
    Ne,
    /// `jl` / `jnge` — sign≠overflow (signed less than)
    L,
    /// `jle` / `jng` — ZF=1 or sign≠overflow
    Le,
    /// `jg` / `jnle` — ZF=0 and sign=overflow
    G,
    /// `jge` / `jnl` — sign=overflow
    Ge,
}

impl JmpCond {
    /// Low nibble of the Jcc rel8 opcode (`74` + this).
    pub fn opcode_byte(self) -> u8 {
        match self {
            Self::E => 0x74,
            Self::Ne => 0x75,
            Self::L => 0x7C,
            Self::Ge => 0x7D,
            Self::Le => 0x7E,
            Self::G => 0x7F,
        }
    }
}

/// A relocation request emitted by the encoder. The assembler turns
/// these into OMF FIXUPP subrecords after each LEDATA.
#[derive(Debug)]
pub struct FixupReq {
    /// Byte position within the LEDATA data payload where the fixup
    /// is to be applied. Relative to the first data byte (NOT the
    /// LEDATA record's segment/offset header).
    pub data_offset: u16,
    pub kind: FixupKind,
}

#[derive(Debug)]
pub enum FixupKind {
    /// Segment-relative 16-bit offset (M=1, location=1), frame method
    /// F1 (GRPDEF), target method T4 (SEGDEF, no displacement). Used
    /// for `mov ax,word ptr <group>:<sym>` loads (fixture 109) and
    /// `mov ax,offset <group>:<sym>` immediates (fixture 108).
    SegRelGroupTarget { group_idx: u8, segment_idx: u8 },
    /// Self-relative 16-bit offset (M=0, location=1), frame method
    /// F5 (target), target method T6 (EXTDEF, no displacement). Used
    /// for near calls to extern functions (fixture 108's `_printf`).
    SelfRelExtern { extdef_idx: u8 },
    /// Segment-relative 16-bit offset (M=1, location=1), frame method
    /// F5 (target), target method T4 (SEGDEF, no displacement). Used
    /// when storing a code symbol's address into memory — frame is
    /// the target's segment because `_TEXT` is not in any group
    /// (fixture 110's `mov [bp-2],offset _f`).
    SegRelTargetFrameSegment { segment_idx: u8 },
}

/// A position-bound parse error. The line number is 1-based and refers
/// to the input `.ASM` text.
#[derive(Debug, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct AsmError {
    pub line: usize,
    pub message: String,
}

pub type AsmResult<T> = Result<T, AsmError>;

impl AsmError {
    pub fn new(line: usize, message: impl Into<String>) -> Self {
        Self { line, message: message.into() }
    }
}

