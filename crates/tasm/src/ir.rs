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
    /// A `dw <symbol>` directive — emits 2 bytes containing the
    /// segment-relative offset of `<symbol>`, with a FIXUPP that
    /// patches the bytes at link time. Used by BCC for jump-table
    /// entries (fixture 158).
    DwSym(String),
    /// `dw <group>:<symbol>[+N]` — same 2-byte slot as `DwSym` but
    /// the FIXUPP carries a `<group>` frame instead of the symbol's
    /// own segment. Used for file-scope `char *p = "lit"` (fixture
    /// 192) where the slot must resolve via DGROUP.
    DwGroupSym {
        group: String,
        symbol: String,
        extra_offset: i16,
    },
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
    /// `push <reg16>` — 50+rc (1-byte form). Covers push of any of
    /// AX/CX/DX/BX/SP/BP/SI/DI.
    PushReg16 { reg: Reg16 },
    /// `pop <reg16>` — 58+rc.
    PopReg16 { reg: Reg16 },
    /// `mov <dst>,<src>` between 16-bit registers — 8B xx with
    /// ModR/M mod=11 reg=dst-code r/m=src-code. Covers `mov bp,sp`,
    /// `mov sp,bp`, `mov ax,dx`, `mov ax,si`, etc.
    MovReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `xor <dst>,<src>` between 16-bit registers — 33 xx. The
    /// canonical "zero the register" form is `xor reg,reg`.
    XorReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `mov <reg16>,<imm16>` — B8+rc lo hi. Generic 16-bit register
    /// immediate load (formerly MovAxImm).
    MovReg16Imm { reg: Reg16, imm: u16 },
    /// `sub sp,<imm8>` — encoded as 83 EC ii (sign-extended imm8 form).
    /// Larger immediates would require the 81 EC encoding; BCC uses
    /// 83 EC for the small frame sizes we've seen (≤ 127 bytes).
    SubSpImm(u8),
    /// `add sp,<imm8>` — encoded as 83 C4 ii. BCC uses this to clean
    /// up the stack after a multi-arg call (fixture 138: `add sp,6`).
    AddSpImm(u8),
    /// `mov word ptr [bp+<offset>],<imm16>` — BCC uses signed
    /// offsets (negative for locals, positive for params).
    MovBpRelImm { offset: i16, imm: u16 },
    /// `mov <reg16>,word ptr [bp+<offset>]` — generic 16-bit load
    /// from a stack local into any 16-bit register. Encoding:
    /// `8B xx dd` where ModR/M xx = mod=01 reg=<dst> r/m=110.
    MovReg16BpRel { reg: Reg16, offset: i16 },
    /// `add ax,word ptr [bp+<offset>]` — 03 46 dd
    AddAxBpRel { offset: i16 },
    /// `add <dst>,<src>` between 16-bit registers — 03 xx with
    /// ModR/M mod=11 reg=dst r/m=src. Used to fold a register-resident
    /// operand into AX (fixture 127: `add ax,si`).
    AddReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `or <dst>,<src>` between 16-bit registers — 0B xx. BCC uses
    /// `or ax,ax` as a compare-against-zero idiom in switch dispatch.
    OrReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `inc <reg16>` — 40+rc (1-byte form). Used heavily in loop
    /// bodies and for register-resident pre/post-increment.
    IncReg16 { reg: Reg16 },
    /// `dec <reg16>` — 48+rc (1-byte form).
    DecReg16 { reg: Reg16 },
    /// `cmp <reg16>,<imm8>` — 83 (F8+rc) ii (Grp1 r/m16,imm8
    /// sign-extended). Used when the immediate fits in a signed byte
    /// and the register isn't AX (AX gets a different encoding).
    CmpReg16Imm8 { reg: Reg16, imm: i8 },
    /// `cmp <lhs:reg16>,<rhs:reg16>` — 3B (mod=11 lhs<<3 rhs). The
    /// `r16,r/m16` family with the LHS in the reg field. BCC emits
    /// this for `cmp si,di` when both compared values are in
    /// registers (fixture 167).
    CmpReg16Reg16 { lhs: Reg16, rhs: Reg16 },
    /// `cmp ax,<imm16>` — 3D lo hi (special AX-accumulator opcode).
    /// BCC prefers this over `83 F8 ii` for `cmp ax,K` because the
    /// AX form has a dedicated opcode and is always 3 bytes
    /// regardless of K's width.
    CmpAxImm { imm: u16 },
    /// `add ax,<imm16>` — 05 lo hi (AX-accumulator special form).
    /// Sibling of `CmpAxImm`. Used by BCC for `r = r + K` patterns.
    AddAxImm { imm: u16 },
    /// `cmp word ptr [bp+<offset>],<imm8>` — 83 7E dd ii. Compare a
    /// stack local directly against a small sign-extended immediate.
    /// BCC uses this for short-circuit logical lowering of patterns
    /// like `if (x < K) ...` (fixture 149).
    CmpBpRelImm8 { offset: i16, imm: i8 },
    /// `sub ax,word ptr [bp+<offset>]` — 2B 46 dd
    SubAxBpRel { offset: i16 },
    /// `sub ax,word ptr [si]` — 2B 04. ModR/M 04 = mod=00 reg=AX
    /// rm=100([si]). Used when the right operand of a non-commutative
    /// op is a deref of a register-resident pointer (fixture 201).
    SubAxFromSiPtr,
    /// `add ax,word ptr [si]` — 03 04. Same ModR/M as the `sub`
    /// sibling; opcode 03 vs 2B. Fixture 202 uses this when the
    /// commutative-swap rule can't fire because LHS isn't a constant.
    AddAxFromSiPtr,
    /// `and ax,word ptr [bp+<offset>]` — 23 46 dd
    AndAxBpRel { offset: i16 },
    /// `or ax,word ptr [bp+<offset>]` — 0B 46 dd
    OrAxBpRel { offset: i16 },
    /// `xor ax,word ptr [bp+<offset>]` — 33 46 dd
    XorAxBpRel { offset: i16 },
    /// `cmp ax,word ptr [bp+<offset>]` — 3B 46 dd
    CmpAxBpRel { offset: i16 },
    /// `imul word ptr [bp+<offset>]` — F7 6E dd. Single-operand signed
    /// multiply: AX = AX * src; high half goes to DX (discarded for
    /// `int * int` returning `int`).
    ImulBpRel { offset: i16 },
    /// `idiv word ptr [bp+<offset>]` — F7 7E dd. Single-operand
    /// signed divide of DX:AX by src; quotient in AX, remainder in
    /// DX. Always preceded by `cwd` to sign-extend AX into DX.
    IdivBpRel { offset: i16 },
    /// `cwd` — 99. Sign-extend AX into DX:AX.
    Cwd,
    /// `mov <reg8>,byte ptr [bp+<offset>]` — 8A xx dd. Generic 8-bit
    /// load from a stack local. Used for shift counts (CL) and char
    /// locals/params (DL/BL/CL).
    MovReg8BpRel { reg: Reg8, offset: i16 },
    /// `mov byte ptr [bp+<offset>],<reg8>` — 88 xx dd. Store an 8-bit
    /// register into a stack local.
    MovBpRelReg8 { offset: i16, reg: Reg8 },
    /// `mov <reg8>,<imm8>` — B0+rc ii. Set an 8-bit register to a
    /// constant.
    MovReg8Imm8 { reg: Reg8, imm: u8 },
    /// `mov <dst>,<src>` — 8A xx. Copy between two 8-bit registers.
    MovReg8Reg8 { dst: Reg8, src: Reg8 },
    /// `mov byte ptr [bp+<offset>],<imm8>` — C6 46 dd ii. Store a
    /// constant byte into a stack local (fixture 011's `char c=1`).
    MovBpRelImm8 { offset: i16, imm: u8 },
    /// `inc <reg8>` — FE C0+rc. Increment an 8-bit register.
    IncReg8 { reg: Reg8 },
    /// `dec <reg8>` — FE C8+rc. Decrement an 8-bit register.
    DecReg8 { reg: Reg8 },
    /// `cmp <reg8>,<imm8>` — 80 F8+rc ii. Compare an 8-bit register
    /// to a constant.
    CmpReg8Imm8 { reg: Reg8, imm: u8 },
    /// `shl ax,cl` — D3 E0. Variable-count logical left shift of AX.
    ShlAxCl,
    /// `sar ax,cl` — D3 F8. Variable-count arithmetic (signed) right
    /// shift of AX. BCC uses SAR for signed `int >> ...`.
    SarAxCl,
    /// `shr ax,cl` — D3 E8. Variable-count logical (unsigned) right
    /// shift of AX. BCC uses SHR for `unsigned >> ...` (fixture 176).
    ShrAxCl,
    /// `j<cc> short <label>` — Jcc rel8 family.
    JmpCondShort { cond: JmpCond, target: String },
    /// `jmp word ptr cs:<table>[bx]` — indirect dispatch through a
    /// jump table located in _TEXT, with BX scaled to a word index.
    /// Encoded as `2E FF A7 lo hi` (cs override + FF /4 with mod=10
    /// /4 r/m=BX). FIXUPP applies to the disp16 (target = the
    /// table label's home segment).
    JmpIndirectCsTableBx { table: String },
    /// `jmp short <label>`
    JmpShort(String),
    /// `call near ptr <label>` — E8 rel16. Intra-segment near call.
    CallNear(String),
    /// `mov ax,word ptr <group>:<symbol>[+<offset>]` — segment-
    /// relative load against a group-anchored data symbol, optionally
    /// at a constant byte offset (e.g. `_a+2` for `a[1]`). Emits
    /// `A1 lo hi` plus a FIXUPP (frame = group, target = symbol's
    /// home segment); the `lo hi` carry `sym.offset + offset`.
    MovAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `mov word ptr <group>:<symbol>[+<offset>], imm16` — store an
    /// immediate to a data-segment global. Emits `C7 06 [addr with
    /// FIXUPP] [imm16]`; ModR/M `06` is `mod=00 reg=000(/0=MOV)
    /// rm=110(disp16)`. Used by `g = K` for both 16- and 32-bit
    /// globals (fixture 205 — long-global assignment writes the
    /// high half via `_g+2`).
    MovGroupSymImm16 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u16,
    },
    /// `mov word ptr <group>:<symbol>[+<offset>], ax` — store AX to
    /// a data-segment global. Emits `A3 lo hi` (mov moffs16, AX) —
    /// AX-specific short form vs the generic `MOV r/m16, r16`.
    /// Fixture 207 uses this for the high half of long-arithmetic
    /// writeback.
    MovGroupSymAx { group: String, symbol: String, offset: i16 },
    /// `mov word ptr <group>:<symbol>[+<offset>], <reg16>` for
    /// non-AX source. Emits `89 (mod=00 reg=<r> rm=110) lo hi`.
    /// Fixture 207's low-word writeback uses DX → encodes as
    /// `89 16 ...`.
    MovGroupSymReg16 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg16,
    },
    /// `add <reg16>, imm8 (sign-extended)` — Grp1 r/m16,imm8sx.
    /// Encoding `83 C(rm) ii` where ModR/M is `mod=11 /0(ADD)
    /// rm=<reg>`. Fixture 207 (`add dx,10`).
    AddReg16Imm8Sx { reg: Reg16, imm: i8 },
    /// `adc ax, imm16` — `15 lo hi`. AX-specific add-with-carry
    /// short form. Fixture 207's high-half carry propagation.
    AdcAxImm16 { imm: u16 },
    /// `sbb ax, imm16` — `1D lo hi`. AX-specific subtract-with-borrow
    /// short form. Used by long unary minus (fixture 226) to combine
    /// the carry from the low-half `neg` into the high half.
    SbbAxImm16 { imm: u16 },
    /// `mov <reg16>,word ptr <group>:<symbol>[+<offset>]` for non-AX
    /// destinations. Uses MOV r16,r/m16 with disp16-only addressing
    /// (`8B (mod=00 reg=<r> rm=110) lo hi`). Same FIXUPP shape as
    /// `MovAxGroupSym`. Fixture 192 uses this for `mov bx,word ptr
    /// DGROUP:_p` when dereferencing a global pointer.
    MovReg16WordGroupSym {
        reg: Reg16,
        group: String,
        symbol: String,
        offset: i16,
    },
    /// `mov al,byte ptr <group>:<symbol>[+<offset>]` — 8-bit moffs8
    /// load (A0 lo hi). Same FIXUPP shape as MovAxGroupSym.
    MovAlGroupSym { group: String, symbol: String, offset: i16 },
    /// `mov al,byte ptr [si]` — 8A 04. 8-bit load through SI pointer.
    MovAlFromSiPtr,
    /// `mov al,byte ptr [bx]` — 8A 07. 8-bit load through BX pointer.
    /// Fixture 192 dereferences a global char pointer via BX.
    MovAlFromBxPtr,
    /// `imul <reg16>` — F7 (mod=11 /5 r/m=reg). Single-operand signed
    /// multiply with a register operand. Used when the operand is
    /// register-resident, e.g. `x *= 3` after BCC enregisters x.
    ImulReg16 { reg: Reg16 },
    /// `add ax,word ptr <group>:<symbol>[+<offset>]` — ADD r16,r/m16
    /// with disp16-only addressing (`03 06 lo hi`). Same FIXUPP
    /// shape; offset added to the symbol's location.
    AddAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `or ax,word ptr <group>:<symbol>[+<offset>]` — OR r16,r/m16
    /// with disp16-only addressing (`0B 06 lo hi`). Used by long
    /// comparison against zero (fixture 215): `mov ax,low / or
    /// ax,high` sets ZF iff both halves are zero.
    OrAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `add dx,word ptr <group>:<symbol>[+<offset>]` — ADD r16,r/m16
    /// with DX as destination (`03 16 lo hi`). Used by long-to-long
    /// addition for the low-half add (fixture 219).
    AddDxGroupSym { group: String, symbol: String, offset: i16 },
    /// `adc ax,word ptr <group>:<symbol>[+<offset>]` — ADC r16,r/m16
    /// with AX as destination (`13 06 lo hi`). Companion to
    /// `AddDxGroupSym` for the high-half carry-in (fixture 219).
    AdcAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `sub dx,word ptr <group>:<symbol>[+<offset>]` — SUB r16,r/m16
    /// with DX dst (`2B 16 lo hi`). Long-to-long subtraction's low-
    /// half subtract (fixture 220).
    SubDxGroupSym { group: String, symbol: String, offset: i16 },
    /// `sbb ax,word ptr <group>:<symbol>[+<offset>]` — SBB r16,r/m16
    /// with AX dst (`1B 06 lo hi`). High-half subtract-with-borrow
    /// (fixture 220).
    SbbAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `and dx,word ptr <group>:<symbol>[+<offset>]` — AND r16,r/m16
    /// with DX dst (`23 16 lo hi`). Used by long bitwise AND for
    /// the low half (fixture 221).
    AndDxGroupSym { group: String, symbol: String, offset: i16 },
    /// `and ax,word ptr <group>:<symbol>[+<offset>]` — AND r16,r/m16
    /// with AX dst (`23 06 lo hi`). Companion for the high half.
    AndAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `or dx,word ptr <group>:<symbol>[+<offset>]` — OR r16,r/m16
    /// with DX dst (`0B 16 lo hi`). Long bitwise OR low half
    /// (fixture 222). Companion to existing `OrAxGroupSym` for
    /// the high half.
    OrDxGroupSym { group: String, symbol: String, offset: i16 },
    /// `xor dx,word ptr <group>:<symbol>[+<offset>]` — XOR r16,r/m16
    /// with DX dst (`33 16 lo hi`). Long bitwise XOR low half
    /// (fixture 224).
    XorDxGroupSym { group: String, symbol: String, offset: i16 },
    /// `xor ax,word ptr <group>:<symbol>[+<offset>]` — XOR r16,r/m16
    /// with AX dst (`33 06 lo hi`). Companion for the high half.
    XorAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `cmp word ptr <group>:<symbol>[+<offset>], imm8 (sx)` — Grp1
    /// r/m16,imm8sx with /7=CMP and disp16-only addressing
    /// (`83 3E lo hi ii`, 5 bytes). Used by long const-compare
    /// (fixture 223): two of these chained with `jne` for `g == K`.
    CmpGroupSymImm8Sx {
        group: String,
        symbol: String,
        offset: i16,
        imm: i8,
    },
    /// `cbw` — 98. Sign-extend AL to AX. Used after loading a `char`
    /// global to widen it to int for arithmetic (fixture 130).
    Cbw,
    /// `lea <reg16>,word ptr [bp+<offset>]` — 8D xx dd. Load
    /// effective address into a 16-bit register. Used by BCC to
    /// compute the address of a stack local (e.g. for `p = &a;`
    /// in fixture 136).
    LeaReg16BpRel { dst: Reg16, offset: i16 },
    /// `mov word ptr [si],<imm16>` — C7 04 lo hi. Store an
    /// immediate through a pointer in SI (fixture 136's `p->x = 7`).
    MovSiPtrImm { imm: u16 },
    /// `add word ptr [si],<imm8 sign-extended>` — 83 04 ii. Read-
    /// modify-write through SI used by compound member assignment
    /// `p->x += K` when SI holds `p` (fixture 182).
    AddSiPtrImm8 { imm: i8 },
    /// `add word ptr [bx],<imm8 sign-extended>` — 83 07 ii. Same
    /// shape as the SI variant; used by global / chained compound
    /// pointer assignment `*p += K` after loading `p` into BX
    /// (fixture 197).
    AddBxPtrImm8 { imm: i8 },
    /// `add word ptr [bp+disp8],<imm8 sign-extended>` — 83 46 dd ii.
    /// Read-modify-write on a stack local; used by compound array
    /// assignment `a[K] += V` when the index is constant (fixture
    /// 184).
    AddBpRelImm8 { offset: i16, imm: i8 },
    /// `mov ax,word ptr [si]` — 8B 04. Load through SI pointer.
    MovAxFromSiPtr,
    /// `mov word ptr [bx],<imm16>` — C7 07 lo hi. Store through BX
    /// (used by indexed array writes after `lea bx,base + scale*i`).
    MovBxPtrImm { imm: u16 },
    /// `mov ax,word ptr [bx]` — 8B 07. Load through BX.
    MovAxFromBxPtr,
    /// `mov bx,word ptr [bx]` — 8B 1F. Chain step in `**p` style
    /// double-indirect loads (fixture 195): keeps the running
    /// pointer in BX while peeling one level of indirection.
    MovBxFromBxPtr,
    /// `shl <reg16>,1` — D1 (mod=11 /4 r/m=reg). The 1-bit shift form
    /// (BCC uses this to compute `i*2` for word-array indexing).
    ShlReg16One { reg: Reg16 },
    /// `rcl <reg16>,1` — D1 (mod=11 /2 r/m=reg). Rotate-left through
    /// carry; used as the high-half partner to `shl` for long left
    /// shift by 1 (fixture 227).
    RclReg16One { reg: Reg16 },
    /// `neg <reg16>` — F7 (mod=11 /3 r/m=reg). Two's-complement negate.
    NegReg16 { reg: Reg16 },
    /// `not <reg16>` — F7 (mod=11 /2 r/m=reg). One's-complement.
    NotReg16 { reg: Reg16 },
    /// `mov <reg16>,offset <group>:<symbol>[+<offset>]` — load a
    /// 16-bit register with the segment-relative address of a data
    /// symbol (possibly at a constant offset). Emits `B8+rc lo hi`
    /// plus a SegRelGroupTarget FIXUPP. Covers fixture 108
    /// (`mov ax,...`) and fixture 157 (`mov si,...`).
    MovReg16OffsetGroupSym { reg: Reg16, group: String, symbol: String, offset: i16 },
    /// `mov <reg16>,offset <symbol>` — symbol with no group prefix,
    /// i.e. an intra-segment code label. Emits `B8+rc lo hi` plus a
    /// SegRelTargetFrameSegment FIXUPP (frame = target's segment).
    /// Used by BCC's linear-search switch to load the address of a
    /// value table in _TEXT (fixture 160).
    MovReg16OffsetSym { reg: Reg16, symbol: String },
    /// `mov word ptr [bp+<offset>],ax` — store AX to a stack local.
    /// Encoding: 89 46 dd (mod=01 reg=AX r/m=110([bp+disp8])).
    MovBpRelAx { offset: i16 },
    /// `mov ax,word ptr cs:[bx]` — load AX through CS:BX (no
    /// displacement). Encoding: 2E 8B 07. Used by linear-search
    /// dispatch to read consecutive case values from a _TEXT table.
    MovAxFromCsBx,
    /// `jmp word ptr cs:[bx+<imm8>]` — indirect jump through
    /// CS:BX+disp8. Encoding: 2E FF 67 dd. Used by linear-search
    /// dispatch to dispatch to the matching label table entry
    /// (the value table and label table are adjacent in memory).
    JmpIndirectCsBxDisp { disp: u8 },
    /// `loop short <label>` — E2 rel8. Decrement CX; jump if CX≠0.
    LoopShort { target: String },
    /// `mov word ptr [bp+<offset>],offset <symbol>` — store a
    /// function or data symbol's address into a stack local. Emits
    /// `C7 46 dd lo hi` plus a SegRelTargetFrameSegment FIXUPP. Used
    /// for function-pointer init (fixture 110).
    MovBpRelOffsetSym { offset: i16, symbol: String },
    /// `call word ptr [bp+<offset>]` — indirect near call through a
    /// stack-resident function pointer. Emits `FF 56 dd`. No FIXUPP
    /// (the address is loaded from the local at runtime).
    CallIndirectBpRel { offset: i16 },
    /// `ret`
    Ret,
}

/// 8086 16-bit general-purpose registers. The byte encoding is the
/// standard x86 "reg" field (0..7): AX=0, CX=1, DX=2, BX=3, SP=4,
/// BP=5, SI=6, DI=7. Used in ModR/M's reg field, as the low 3 bits
/// of single-byte PUSH/POP/INC/DEC opcodes, and elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg16 {
    Ax,
    Cx,
    Dx,
    Bx,
    Sp,
    Bp,
    Si,
    Di,
}

impl Reg16 {
    pub fn code(self) -> u8 {
        match self {
            Self::Ax => 0,
            Self::Cx => 1,
            Self::Dx => 2,
            Self::Bx => 3,
            Self::Sp => 4,
            Self::Bp => 5,
            Self::Si => 6,
            Self::Di => 7,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ax" => Some(Self::Ax),
            "cx" => Some(Self::Cx),
            "dx" => Some(Self::Dx),
            "bx" => Some(Self::Bx),
            "sp" => Some(Self::Sp),
            "bp" => Some(Self::Bp),
            "si" => Some(Self::Si),
            "di" => Some(Self::Di),
            _ => None,
        }
    }
}

/// 8086 8-bit general-purpose registers. The byte encoding for each
/// is the standard x86 "reg" field (0..7): AL=0, CL=1, DL=2, BL=3,
/// AH=4, CH=5, DH=6, BH=7. Used both in ModR/M's reg field and as
/// the low 3 bits of single-byte `MOV r8, imm8` opcodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg8 {
    Al,
    Cl,
    Dl,
    Bl,
    Ah,
    Ch,
    Dh,
    Bh,
}

impl Reg8 {
    pub fn code(self) -> u8 {
        match self {
            Self::Al => 0,
            Self::Cl => 1,
            Self::Dl => 2,
            Self::Bl => 3,
            Self::Ah => 4,
            Self::Ch => 5,
            Self::Dh => 6,
            Self::Bh => 7,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "al" => Some(Self::Al),
            "cl" => Some(Self::Cl),
            "dl" => Some(Self::Dl),
            "bl" => Some(Self::Bl),
            "ah" => Some(Self::Ah),
            "ch" => Some(Self::Ch),
            "dh" => Some(Self::Dh),
            "bh" => Some(Self::Bh),
            _ => None,
        }
    }
}

/// Conditional-jump opcodes (Jcc rel8 family, 0x70-0x7F).
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
    /// `ja` / `jnbe` — CF=0 and ZF=0 (unsigned above). Used by BCC
    /// for the jump-table bounds check: `cmp bx,N / ja default`.
    A,
    /// `jae` / `jnb` — CF=0 (unsigned above-or-equal). Used by BCC
    /// for `if (u_a < u_b)` skip-branches with unsigned operands.
    Ae,
    /// `jb` / `jnae` — CF=1 (unsigned below).
    B,
    /// `jbe` / `jna` — CF=1 or ZF=1 (unsigned below-or-equal).
    Be,
}

impl JmpCond {
    pub fn opcode_byte(self) -> u8 {
        match self {
            Self::E => 0x74,
            Self::Ne => 0x75,
            Self::B => 0x72,
            Self::Ae => 0x73,
            Self::Be => 0x76,
            Self::A => 0x77,
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
    /// Segment-relative 16-bit offset (M=1, location=1), frame method
    /// F1 (GRPDEF), target method T2 (EXTDEF, no displacement). Used
    /// for `mov ax,word ptr DGROUP:_g` when `_g` is declared via
    /// `extrn _g:word` (fixture 163). Same shape as SegRelGroupTarget
    /// except the target is an external symbol instead of a segment.
    SegRelGroupExtern { group_idx: u8, extdef_idx: u8 },
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

