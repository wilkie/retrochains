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
    /// `sub sp,<imm>` — picks the smallest encoding: `83 EC ii` when
    /// the value fits in i8sx (≤ 127), else `81 EC lo hi` for larger
    /// frames (fixture 1739: 200-byte int[100] local). The carrier
    /// type is u16 since BCC frames can exceed 127 bytes.
    SubSpImm(u16),
    /// `add sp,<imm>` — sibling of [`Self::SubSpImm`]. `83 C4 ii` for
    /// small values, `81 C4 lo hi` for larger.
    AddSpImm(u16),
    /// `mov word ptr [bp+<offset>],<imm16>` — BCC uses signed
    /// offsets (negative for locals, positive for params).
    MovBpRelImm { offset: i16, imm: u16 },
    /// `mov word ptr [bp+<offset>],offset <group>:<symbol>[+<sym_offset>]`
    /// — `C7 46 dd lo hi` + FIXUPP relocating the imm16 to the
    /// symbol's offset. Used by BCC's `<stack-local> = &<global>`
    /// peephole (fixture 601 stores `&g` into a stack pointer local
    /// directly: `mov word ptr [bp-2], offset DGROUP:_g`).
    MovBpRelOffsetGroupSym { offset: i16, group: String, symbol: String, sym_offset: i16 },
    /// `mov <reg16>,word ptr [bp+<offset>]` — generic 16-bit load
    /// from a stack local into any 16-bit register. Encoding:
    /// `8B xx dd` where ModR/M xx = mod=01 reg=<dst> r/m=110.
    MovReg16BpRel { reg: Reg16, offset: i16 },
    /// `mov <reg16>,word ptr <group>:<sym>[bx+disp]` — bx-indexed
    /// load from a data-segment global. Used by variable-indexed
    /// long-array reads (fixture 303). Encoding: `8B xx lo hi`
    /// where ModR/M xx = mod=10 reg=<dst> r/m=111([bx]+disp16), and
    /// the 16-bit displacement is FIXUPP-patched to the symbol's
    /// offset plus the literal `disp`.
    MovReg16GroupSymBxDisp {
        reg: Reg16,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `mov word ptr <group>:<sym>[bx+disp],<imm16>` — bx-indexed
    /// store of an immediate to a data-segment global. Used by
    /// variable-indexed long-array writes (fixture 305). Encoding:
    /// `C7 87 lo hi imm_lo imm_hi`. ModR/M 87 = mod=10 /0(MOV)
    /// r/m=111([bx]+disp16).
    MovGroupSymBxDispImm {
        group: String,
        symbol: String,
        disp: u16,
        imm: u16,
    },
    /// `mov word ptr <group>:<sym>[bx+disp],<reg16>` — bx-indexed
    /// store of a register to a data-segment global. Used by
    /// variable-indexed int-array writes where the RHS is a
    /// register-resident local (fixture 510: `a[i] = i` with `i`
    /// in SI). Encoding: `89 mod=10 reg r/m=111 disp_lo disp_hi`.
    MovGroupSymBxDispReg16 {
        group: String,
        symbol: String,
        disp: u16,
        reg: Reg16,
    },
    /// `mov byte ptr <group>:<sym>[si+disp], imm8` — SI-indexed
    /// byte store to a data-segment global. Encoding: `C6 84 lo hi
    /// ii` where ModR/M 84 = mod=10 /0(MOV) r/m=100([SI]+disp16),
    /// and the disp16 is FIXUPP-patched to the symbol's offset.
    /// Fixture 1366 (`char buf[5]; for (i=0..4) buf[i] = 'X';` →
    /// `mov byte ptr [si + _buf], 'X'` in the loop body).
    MovGroupSymSiDispByteImm8 {
        group: String,
        symbol: String,
        disp: u16,
        imm: u8,
    },
    /// `mov byte ptr <group>:<sym>[si+disp], <reg8>` — sibling
    /// where the source is an 8-bit register. Encoding: `88 mod=10
    /// reg=<reg> r/m=100 lo hi`.
    MovGroupSymSiDispReg8 {
        group: String,
        symbol: String,
        disp: u16,
        reg: Reg8,
    },
    /// `mov <reg8>, byte ptr <group>:<sym>[si+disp]` — SI-indexed
    /// byte load. Encoding: `8A mod=10 reg=<reg> r/m=100 lo hi`.
    MovReg8GroupSymSiDisp {
        reg: Reg8,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `mov <reg16>, word ptr <group>:<sym>[si+disp]` — SI-indexed
    /// word load. Encoding: `8B mod=10 reg=<reg> r/m=100 lo hi`.
    /// Used by variable-indexed int-array reads (`return a[i];`).
    MovReg16GroupSymSiDisp {
        reg: Reg16,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `mov word ptr <group>:<sym>[si+disp], <reg16>` — SI-indexed
    /// word store. Encoding: `89 mod=10 reg=<reg> r/m=100 lo hi`.
    MovGroupSymSiDispReg16 {
        group: String,
        symbol: String,
        disp: u16,
        reg: Reg16,
    },
    /// `mov word ptr <group>:<sym>[si+disp], <imm16>` — SI-indexed
    /// word-immediate store. Encoding: `C7 84 lo hi imm_lo imm_hi`.
    MovGroupSymSiDispImm16 {
        group: String,
        symbol: String,
        disp: u16,
        imm: u16,
    },
    /// `add ax,word ptr [bp+<offset>]` — 03 46 dd
    AddAxBpRel { offset: i16 },
    /// `adc dx, word ptr [bp+disp8]` — `13 56 dd`. ADC r16,r/m16
    /// with DX dst and bp-relative source. High-half carry
    /// propagation for return-from-long-add (fixture 285).
    AdcDxBpRel { offset: i16 },
    /// `sbb dx, word ptr [bp+disp8]` — `1B 56 dd`. SBB r16,r/m16
    /// companion to `AdcDxBpRel` for `return a - b;` of long params.
    SbbDxBpRel { offset: i16 },
    /// `add dx, word ptr [bp+disp8]` — `03 56 dd`. Low-half add for
    /// stack-local long arithmetic where the result goes to memory
    /// (AX=high, DX=low globals convention). Fixture 329.
    AddDxBpRel { offset: i16 },
    /// `adc ax, word ptr [bp+disp8]` — `13 46 dd`. High-half adc
    /// companion to `AddDxBpRel`. Fixture 329.
    AdcAxBpRel { offset: i16 },
    /// `sub dx, word ptr [bp+disp8]` — `2B 56 dd`. Low-half sub for
    /// stack-local long arithmetic. Fixture 330.
    SubDxBpRel { offset: i16 },
    /// `sbb ax, word ptr [bp+disp8]` — `1B 46 dd`. High-half sbb
    /// companion to `SubDxBpRel`. Fixture 330.
    SbbAxBpRel { offset: i16 },
    /// `and dx, word ptr [bp+disp8]` — `23 56 dd`. Low-half AND for
    /// stack-local long bitwise arithmetic. Fixture 333.
    AndDxBpRel { offset: i16 },
    /// `or dx, word ptr [bp+disp8]` — `0B 56 dd`. Low-half OR for
    /// stack-local long bitwise arithmetic. Fixture 334.
    OrDxBpRel { offset: i16 },
    /// `xor dx, word ptr [bp+disp8]` — `33 56 dd`. Low-half XOR for
    /// stack-local long bitwise arithmetic.
    XorDxBpRel { offset: i16 },
    /// `add word ptr [bp+disp8],dx` — `01 56 dd`. Memory-destination
    /// add of DX (low half of RHS) into a stack local's low half.
    /// Long stack-local compound `+=` with non-constant RHS
    /// (fixture 339).
    AddBpRelDx { offset: i16 },
    /// `adc word ptr [bp+disp8],ax` — `11 46 dd`. Carry-propagation
    /// partner to `AddBpRelDx`. High half of `x += y` where `y` was
    /// loaded with AX=high, DX=low.
    AdcBpRelAx { offset: i16 },
    /// `sub word ptr [bp+disp8],dx` — `29 56 dd`. Compound `-=` low
    /// half on a long stack local with a register-loaded RHS.
    /// Fixture 340.
    SubBpRelDx { offset: i16 },
    /// `sbb word ptr [bp+disp8],ax` — `19 46 dd`. Borrow-propagation
    /// partner to `SubBpRelDx`.
    SbbBpRelAx { offset: i16 },
    /// `and word ptr [bp+disp8],dx` — `21 56 dd`. Long-stack compound
    /// `&=` low half with register-loaded RHS (fixture 342).
    AndBpRelDx { offset: i16 },
    /// `and word ptr [bp+disp8],ax` — `21 46 dd`. High-half partner
    /// to `AndBpRelDx`.
    AndBpRelAx { offset: i16 },
    /// `or word ptr [bp+disp8],dx` — `09 56 dd`. Long-stack compound
    /// `|=` low half with register-loaded RHS (fixture 343).
    OrBpRelDx { offset: i16 },
    /// `or word ptr [bp+disp8],ax` — `09 46 dd`.
    OrBpRelAx { offset: i16 },
    /// `xor word ptr [bp+disp8],dx` — `31 56 dd`. Long-stack compound
    /// `^=` low half with register-loaded RHS (fixture 344).
    XorBpRelDx { offset: i16 },
    /// `xor word ptr [bp+disp8],ax` — `31 46 dd`.
    XorBpRelAx { offset: i16 },
    /// `add word ptr [bp+disp8],ax` — `01 46 dd`. Low half of
    /// `long += int x` for a stack LHS: AX holds the int RHS,
    /// DX holds the cwd sign-extension. Fixture 765.
    AddBpRelAx { offset: i16 },
    /// `adc word ptr [bp+disp8],dx` — `11 56 dd`. High-half carry
    /// partner to `AddBpRelAx` for the same `long += int` shape.
    AdcBpRelDx { offset: i16 },
    /// `sub word ptr [bp+disp8],ax` — `29 46 dd`. Long-stack
    /// `-= int` low half.
    SubBpRelAx { offset: i16 },
    /// `sbb word ptr [bp+disp8],dx` — `19 56 dd`. High-half borrow
    /// partner to `SubBpRelAx`.
    SbbBpRelDx { offset: i16 },
    /// `add byte ptr [bp+disp8],al` — `00 46 dd`. Memory-destination
    /// byte add of AL into a stack char. Char compound `+=` with a
    /// char-lvalue RHS.
    AddBpRelByteAl { offset: i16 },
    /// `sub byte ptr [bp+disp8],al` — `28 46 dd`. Sibling.
    SubBpRelByteAl { offset: i16 },
    /// `and byte ptr [bp+disp8],al` — `20 46 dd`. Sibling.
    AndBpRelByteAl { offset: i16 },
    /// `or  byte ptr [bp+disp8],al` — `08 46 dd`. Sibling.
    OrBpRelByteAl { offset: i16 },
    /// `xor byte ptr [bp+disp8],al` — `30 46 dd`. Char element
    /// compound `^=` with a char lvalue RHS (fixture 1447).
    XorBpRelByteAl { offset: i16 },
    /// `add <dst>,<src>` between 16-bit registers — 03 xx with
    /// ModR/M mod=11 reg=dst r/m=src. Used to fold a register-resident
    /// operand into AX (fixture 127: `add ax,si`).
    AddReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `adc <dst>,<src>` between 16-bit registers — 13 xx with
    /// ModR/M mod=11 reg=dst r/m=src. Carry-propagation high-half
    /// partner to a register-to-register `add` (fixture 257's
    /// long-plus-int sum where the widened int sits in DX:AX and
    /// the long accumulator in BX:CX).
    AdcReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `sub <dst>,<src>` between 16-bit registers — 2B xx with
    /// ModR/M mod=11 reg=dst r/m=src (fixture 258's long-minus-int
    /// low-half subtract).
    SubReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `sbb <dst>,<src>` between 16-bit registers — 1B xx with
    /// ModR/M mod=11 reg=dst r/m=src (fixture 258's long-minus-int
    /// high-half borrow propagation).
    SbbReg16Reg16 { dst: Reg16, src: Reg16 },
    /// `and <dst>,<src>` between 16-bit registers — 23 xx with
    /// ModR/M mod=11 reg=dst r/m=src (fixture 259's long-and-int).
    AndReg16Reg16 { dst: Reg16, src: Reg16 },
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
    /// `cmp <reg16>,<imm16>` — 81 (F8+rc) lo hi (Grp1 r/m16,imm16).
    /// Used when the immediate doesn't fit in a signed byte for a
    /// non-AX register. AX uses `CmpAxImm` (3-byte 3D form).
    /// Fixture 2001 (`cmp si, 500`), 2952 (`cmp si, 200`).
    CmpReg16Imm16 { reg: Reg16, imm: u16 },
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
    /// `sub ax,imm16` — `2D lo hi`. AX-accumulator subtract; same
    /// length (3 bytes) as the imm8sx form but BCC picks this
    /// shape unconditionally for AX (fixture 3578: `unsigned x -
    /// 5` → `2D 05 00`).
    SubAxImm { imm: u16 },
    /// `cmp word ptr [bp+<offset>],<imm8>` — 83 7E dd ii. Compare a
    /// stack local directly against a small sign-extended immediate.
    /// BCC uses this for short-circuit logical lowering of patterns
    /// like `if (x < K) ...` (fixture 149).
    CmpBpRelImm8 { offset: i16, imm: i8 },
    /// `cmp word ptr [bp+<offset>],<imm16>` — 81 7E dd lo hi.
    /// Wide-immediate sibling of `CmpBpRelImm8` for constants that
    /// don't fit a signed-byte immediate (fixture 563: `if (x <
    /// -5)` where -5 sign-extends to 0xFFFB which fits imm8sx, but
    /// e.g. -10 also does — the trigger is anything outside
    /// [-128, 127] and many wider negatives end up here).
    CmpBpRelImm16 { offset: i16, imm: u16 },
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
    /// `add ax,word ptr [di]` — 03 05. Companion to `AddAxFromSiPtr`
    /// for the second pointer-local when BCC enregisters two ptrs
    /// (fixture 625's `*p + *q` with p in SI and q in DI).
    AddAxFromDiPtr,
    /// `add <reg16>, word ptr [bx]` — `03 (mod=00 reg=<r> r/m=111)`.
    /// Memory-direct add through BX to any non-AX register dest.
    /// Used for `sum += a[i]` after the address compute lands in BX
    /// (fixture 1822: `sum` in DI, `a[i]` reached via BX).
    AddReg16FromBxPtr { reg: Reg16 },
    /// `add <reg16>, word ptr [di]` — `03 (mod=00 reg=<r> r/m=101)`.
    /// DI sibling. Fixture 1325 (`a += *p` for `int *p` in DI).
    AddReg16FromDiPtr { reg: Reg16 },
    /// `add <reg16>, word ptr [si]` — `03 (mod=00 reg=<r> r/m=100)`.
    /// SI sibling for non-AX destinations.
    AddReg16FromSiPtr { reg: Reg16 },
    /// `add ax,word ptr [si+disp8]` — `03 44 dd`. ModR/M 44 =
    /// mod=01 reg=AX r/m=100 ([si]+disp8). Used when the right
    /// operand is `p[K]` for register-resident pointer `p` in SI
    /// with a non-zero K (fixture 1472: `p[0] + p[1]`).
    AddAxSiDisp { disp: i8 },
    /// `add ax,word ptr [di+disp8]` — `03 45 dd`. DI sibling of
    /// `AddAxSiDisp`.
    AddAxDiDisp { disp: i8 },
    /// `add <reg16>,word ptr [si+disp8]` — `03 (mod=01 reg=<r>
    /// r/m=100) dd`. Generic dst-reg sibling of `AddAxSiDisp`,
    /// used by compound `<reg> += <ptr>-><field>` for register-
    /// resident pointers. Fixture 3343 (`s += p->v` with p in SI).
    AddReg16SiDisp { reg: Reg16, disp: i8 },
    /// `add <reg16>,word ptr [di+disp8]` — `03 (mod=01 reg=<r>
    /// r/m=101) dd`. DI sibling of `AddReg16SiDisp`.
    AddReg16DiDisp { reg: Reg16, disp: i8 },
    /// `sub ax,word ptr [si+disp8]` — `2B 44 dd`. Sibling of
    /// `SubAxFromSiPtr` with displacement.
    SubAxSiDisp { disp: i8 },
    /// `sub ax,word ptr [di+disp8]` — `2B 45 dd`.
    SubAxDiDisp { disp: i8 },
    /// `and ax,word ptr [bp+<offset>]` — 23 46 dd
    AndAxBpRel { offset: i16 },
    /// `or ax,word ptr [bp+<offset>]` — 0B 46 dd
    OrAxBpRel { offset: i16 },
    /// `xor ax,word ptr [bp+<offset>]` — 33 46 dd
    XorAxBpRel { offset: i16 },
    /// `and <reg16>,word ptr [bp+<offset>]` — `23 (mod=01 reg=<r>
    /// r/m=110) dd`. Generic int-register-vs-stack bitwise AND for
    /// compound `&=` on a register local (fixture 655: `x &= y`
    /// with x in SI, y at [bp-2] → `and si, word ptr [bp-2]` =
    /// `23 76 dd`). AX keeps its dedicated variant above.
    AndReg16BpRel { reg: Reg16, offset: i16 },
    /// `or <reg16>,word ptr [bp+<offset>]` — `0B (mod=01 reg=<r>
    /// r/m=110) dd`. Sibling of `AndReg16BpRel` for compound `|=`
    /// (fixture 656).
    OrReg16BpRel { reg: Reg16, offset: i16 },
    /// `xor <reg16>,word ptr [bp+<offset>]` — `33 (mod=01 reg=<r>
    /// r/m=110) dd`. Sibling for compound `^=` (fixture 657).
    XorReg16BpRel { reg: Reg16, offset: i16 },
    /// `add <reg16>,word ptr [bp+<offset>]` — `03 (mod=01 reg=<r>
    /// r/m=110) dd`. Sibling for compound `+=` on a non-AX reg
    /// local (fixture 661: `x += y` with x in SI, y at [bp-2]
    /// → `add si, word ptr [bp-2]` = `03 76 dd`). AX keeps its
    /// own `AddAxBpRel` variant.
    AddReg16BpRel { reg: Reg16, offset: i16 },
    /// `sub <reg16>,word ptr [bp+<offset>]` — `2B (mod=01 reg=<r>
    /// r/m=110) dd`. Sibling for compound `-=` (fixture 660).
    SubReg16BpRel { reg: Reg16, offset: i16 },
    /// `add <reg16>, word ptr <group>:<sym>[bx+disp]` — bx-indexed
    /// load + add for `<reg> += <global-arr>[<var>]`. ADD r16,r/m16
    /// (opcode 03) with mod=10 r/m=111 ([bx]+disp16). FIXUPP-patched
    /// disp16 = sym-offset + literal disp. Fixture 1462 (`s += a[i]`
    /// for int global array, var index, reg-resident s).
    AddReg16GroupSymBxDisp {
        reg: Reg16,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `inc word ptr <group>:<sym>[bx]` — `FF 87 lo hi`. Grp5 /0
    /// against an indexed global element. Fixture 2949
    /// (`arr[i] += 1`).
    IncGroupSymBxDisp { group: String, symbol: String, disp: u16 },
    /// `dec word ptr <group>:<sym>[bx]` — `FF 8F lo hi`. Grp5 /1.
    DecGroupSymBxDisp { group: String, symbol: String, disp: u16 },
    /// `add word ptr <group>:<sym>[bx], imm8sx` — `83 87 lo hi ii`.
    /// Grp1 /0 with an indexed global memory operand and a
    /// sign-extended byte immediate.
    AddGroupSymBxDispImm8Sx {
        group: String,
        symbol: String,
        disp: u16,
        imm: i8,
    },
    /// `add word ptr <group>:<sym>[bx], imm16` — `81 87 lo hi LL HH`.
    AddGroupSymBxDispImm16 {
        group: String,
        symbol: String,
        disp: u16,
        imm: u16,
    },
    /// `sub word ptr <group>:<sym>[bx], imm8sx` — `83 AF lo hi ii`.
    SubGroupSymBxDispImm8Sx {
        group: String,
        symbol: String,
        disp: u16,
        imm: i8,
    },
    /// `sub word ptr <group>:<sym>[bx], imm16` — `81 AF lo hi LL HH`.
    SubGroupSymBxDispImm16 {
        group: String,
        symbol: String,
        disp: u16,
        imm: u16,
    },
    /// `add word ptr <group>:<sym>[bx], <reg16>` — `01 (mod=10
    /// reg=<r> r/m=111) lo hi`. Indexed memory += register.
    /// Fixture 3593 (`arr[i] += arr[j]`).
    AddGroupSymBxDispReg16 {
        reg: Reg16,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `sub word ptr <group>:<sym>[bx], <reg16>` — `29 (mod=10
    /// reg=<r> r/m=111) lo hi`. Sibling.
    SubGroupSymBxDispReg16 {
        reg: Reg16,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `cmp word ptr <group>:<sym>[bx], imm8sx` — `83 BF lo hi ii`.
    /// Memory-direct word compare against a sign-extended byte
    /// immediate, indexed via BX. The 16-bit displacement is the
    /// symbol's segment-relative offset (FIXUPP-patched). Fixture
    /// 1309 (`while (a[i])` for int global array a).
    CmpGroupSymBxDispImm8 {
        group: String,
        symbol: String,
        disp: u16,
        imm: i8,
    },
    /// `cmp word ptr <group>:<sym>[bx], imm16` — `81 BF lo hi LL HH`.
    /// Wide-immediate sibling for constants outside [-128, 127].
    CmpGroupSymBxDispImm16 {
        group: String,
        symbol: String,
        disp: u16,
        imm: u16,
    },
    /// `cmp byte ptr <group>:<sym>[bx], imm8` — `80 BF lo hi ii`.
    /// Byte-form sibling for char-array boolean tests.
    CmpByteGroupSymBxDispImm8 {
        group: String,
        symbol: String,
        disp: u16,
        imm: u8,
    },
    /// `add <reg16>, word ptr <group>:<sym>[+<offset>]` — `03 (mod=00
    /// reg=<r> r/m=110) lo hi`. Memory-direct add from a data-segment
    /// global to a non-AX register destination (AX uses the dedicated
    /// `AddAxGroupSym`). Fixture 1303 (`a += g` with a in SI).
    AddReg16GroupSym {
        reg: Reg16,
        group: String,
        symbol: String,
        offset: i16,
    },
    /// `or <reg16>, word ptr <group>:<sym>[+<offset>]` — `0B (mod=00
    /// reg=<r> r/m=110) lo hi`. OR sibling of `AddReg16GroupSym`.
    /// Fixture 1383 (`a |= s.x` with a in SI).
    OrReg16GroupSym {
        reg: Reg16,
        group: String,
        symbol: String,
        offset: i16,
    },
    /// `cmp ax,word ptr [bp+<offset>]` — 3B 46 dd
    CmpAxBpRel { offset: i16 },
    /// `cmp dx,word ptr [bp+disp8]` — 3B 56 dd. Low-half companion to
    /// `CmpAxBpRel` for the long-vs-long 3-jump compare on stack
    /// locals (fixture 297).
    CmpDxBpRel { offset: i16 },
    /// `cmp <reg16>,word ptr [bp+<offset>]` — `3B (mod=01 reg=<r>
    /// r/m=110) dd`. Generic register-vs-stack-local compare for
    /// register-resident locals tested against memory. Fixture 648
    /// (`i < n` with i in SI and n at `[bp-2]` → `cmp si, word ptr
    /// [bp-2]` = `3B 76 dd`). AX (`3B 46`) and DX (`3B 56`) keep
    /// their dedicated variants since they predate this one and the
    /// long-compare paths reference them by name.
    CmpReg16BpRel { reg: Reg16, offset: i16 },
    /// `cmp word ptr [bp+<offset>], <reg16>` — `39 (mod=01 reg=<r>
    /// r/m=110) dd`. Memory-on-left sibling of `CmpReg16BpRel`,
    /// preserves the operand order of `<stack-mem> <relop> <reg>`
    /// so the caller can emit the natural relop mnemonic instead of
    /// the swapped one. Fixture 3588 (`a > b` with a stack, b in
    /// SI → `cmp word ptr [bp+4], si` = `39 76 04`).
    CmpBpRelReg16 { reg: Reg16, offset: i16 },
    /// `imul word ptr [bp+<offset>]` — F7 6E dd. Single-operand signed
    /// multiply: AX = AX * src; high half goes to DX (discarded for
    /// `int * int` returning `int`).
    ImulBpRel { offset: i16 },
    /// `idiv word ptr [bp+<offset>]` — F7 7E dd. Single-operand
    /// signed divide of DX:AX by src; quotient in AX, remainder in
    /// DX. Always preceded by `cwd` to sign-extend AX into DX.
    IdivBpRel { offset: i16 },
    /// `div word ptr [bp+<offset>]` — F7 76 dd. Single-operand
    /// unsigned divide of DX:AX by src; quotient in AX, remainder
    /// in DX. Always preceded by `xor dx, dx` to zero the upper
    /// half (vs signed which uses `cwd`). ModR/M `76` = mod=01
    /// /6(DIV) r/m=110. Fixture 946.
    DivBpRel { offset: i16 },
    /// `imul word ptr <group>:<symbol>[+<offset>]` — `F7 /5 r/m16`
    /// with mod=00 r/m=110 → `F7 2E lo hi` + FIXUPP. Single-operand
    /// signed multiply against a data-segment global. Fixture 809
    /// (`int g; int h; g *= h`).
    ImulGroupSym { group: String, symbol: String, offset: i16 },
    /// `idiv word ptr <group>:<symbol>[+<offset>]` — `F7 /7 r/m16`
    /// with mod=00 r/m=110 → `F7 3E lo hi` + FIXUPP. Signed
    /// divide against a data-segment global. Fixture 810.
    IdivGroupSym { group: String, symbol: String, offset: i16 },
    /// `imul word ptr [si]` — `F7 /5 r/m16` with mod=00 r/m=100
    /// → `F7 2C`. Single-operand signed multiply against the
    /// word pointed to by SI. Fixture 824's deref sibling.
    ImulSiPtr,
    /// `idiv word ptr [si]` — `F7 /7 r/m16` with mod=00 r/m=100
    /// → `F7 3C`. Signed divide against the word pointed to by
    /// SI. Fixture 825 (`g /= *p` with `p` register-resident).
    IdivSiPtr,
    /// `imul byte ptr [bp+<offset>]` — `F6 (mod=01 /5 r/m=110) dd`
    /// = `F6 6E dd`. 8-bit single-operand signed multiply: AX =
    /// AL * src. Char compound `*=` with mem-resident RHS (fixture
    /// 672: `c *= d` → `mov al, dl; imul byte ptr [bp-1]; mov dl, al`).
    ImulByteBpRel { offset: i16 },
    /// `idiv byte ptr [bp+<offset>]` — `F6 (mod=01 /7 r/m=110) dd`
    /// = `F6 7E dd`. 8-bit single-operand signed divide of AX by
    /// src; quotient in AL, remainder in AH. Char compound `/=`
    /// and `%=` with mem-resident RHS (fixture 673: `c /= d` →
    /// `mov al, dl; cbw; idiv byte ptr [bp-1]; mov dl, al`).
    IdivByteBpRel { offset: i16 },
    /// `div al,byte ptr [bp+<offset>]` — `F6 (mod=01 /6 r/m=110) dd`
    /// = `F6 76 dd`. 8-bit single-operand unsigned divide of AX by
    /// src. Unsigned-char compound `/=` and `%=` with mem-resident
    /// RHS (fixture 677). TASM emits the explicit `al,` operand
    /// in the listing for this case, so the parser/asm-listing
    /// path must match that spelling.
    DivByteBpRel { offset: i16 },
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
    /// `mov byte ptr [si+<disp>],<imm8>` — C6 (mod /0 r/m=100) ii.
    /// disp=0 encodes as `C6 04 ii` (3 bytes, mod=00); disp!=0
    /// fitting i8 encodes as `C6 44 dd ii` (4 bytes, mod=01). Used
    /// by char-pointer subscript writes through an SI-resident
    /// pointer: `p[K] = 'B'` lowers to a memory-direct byte store
    /// through SI. Fixture 1016.
    MovByteSiDispImm8 { disp: i16, imm: u8 },
    /// `mov <reg8>,byte ptr [si+<disp>]` — 8A (mod reg r/m=100).
    /// disp=0 → `8A xx04` (mod=00, 2 bytes total); disp!=0 fitting
    /// i8 → `8A xx44 dd` (mod=01, 3 bytes). Char-pointer subscript
    /// read through an SI-resident pointer: `return p[K];` loads the
    /// byte into AL. Fixture 1019.
    MovReg8ByteSiDisp { reg: Reg8, disp: i16 },
    /// `inc <reg8>` — FE C0+rc. Increment an 8-bit register.
    IncReg8 { reg: Reg8 },
    /// `dec <reg8>` — FE C8+rc. Decrement an 8-bit register.
    DecReg8 { reg: Reg8 },
    /// `cmp <reg8>,<imm8>` — 80 F8+rc ii. Compare an 8-bit register
    /// to a constant.
    CmpReg8Imm8 { reg: Reg8, imm: u8 },
    /// `cmp al,byte ptr [bp+<offset>]` — 3A 46 dd. CMP r8, r/m8 with
    /// AL as reg and `[bp+disp8]` as r/m. ModR/M 46 = mod=01 reg=000
    /// (AL) r/m=110 (BP). Used by the char-vs-char compare peephole
    /// — both operands are bytes, so no widening needed. Fixture
    /// 951 (`c == d`), 952 (`c < d`).
    CmpAlBpRel { offset: i16 },
    /// `add al,<imm8>` — 04 ii. AL-specific accumulator form (2
    /// bytes vs. 3 for the generic `80 C0 ii`). Fixture 529 (char
    /// compound add through AL).
    AddAlImm8 { imm: u8 },
    /// `sub al,<imm8>` — 2C ii. AL-specific sibling of `AddAlImm8`.
    SubAlImm8 { imm: u8 },
    /// `and al,<imm8>` — 24 ii. AL-specific sibling.
    AndAlImm8 { imm: u8 },
    /// `or al,<imm8>` — 0C ii. AL-specific sibling.
    OrAlImm8 { imm: u8 },
    /// `xor al,<imm8>` — 34 ii. AL-specific sibling.
    XorAlImm8 { imm: u8 },
    /// `and <reg8>,<imm8>` — 80 (mod=11 /4 r/m=<reg-code>) ii.
    /// Generic byte-register bitwise AND, used for non-AL registers
    /// (AL has the shorter `AndAlImm8` form `24 ii`). Fixture 556
    /// (`char c &= 31` with c in DL).
    AndReg8Imm8 { reg: Reg8, imm: u8 },
    /// `or <reg8>,<imm8>` — 80 (mod=11 /1 r/m=<reg-code>) ii.
    OrReg8Imm8 { reg: Reg8, imm: u8 },
    /// `xor <reg8>,<imm8>` — 80 (mod=11 /6 r/m=<reg-code>) ii.
    XorReg8Imm8 { reg: Reg8, imm: u8 },
    /// `add <reg8>,<reg8>` — `02 (mod=11 reg=<dst> r/m=<src>)`.
    /// Char compound `+=` between two byte locals: dst in a
    /// byte register, src already loaded into AL via
    /// `MovReg8BpRel`. Fixture 665 (`c += d` with c in DL,
    /// d in AL → `add dl, al` = `02 D0`).
    AddReg8Reg8 { dst: Reg8, src: Reg8 },
    /// `sub <reg8>,<reg8>` — `2A (mod=11 reg=<dst> r/m=<src>)`.
    /// Char compound `-=` sibling of `AddReg8Reg8`.
    SubReg8Reg8 { dst: Reg8, src: Reg8 },
    /// `and <reg8>,<reg8>` — `22 (mod=11 reg=<dst> r/m=<src>)`.
    /// Char compound `&=` sibling.
    AndReg8Reg8 { dst: Reg8, src: Reg8 },
    /// `or <reg8>,<reg8>` — `0A (mod=11 reg=<dst> r/m=<src>)`.
    /// Char compound `|=` sibling.
    OrReg8Reg8 { dst: Reg8, src: Reg8 },
    /// `xor <reg8>,<reg8>` — `32 (mod=11 reg=<dst> r/m=<src>)`.
    /// Char compound `^=` sibling.
    XorReg8Reg8 { dst: Reg8, src: Reg8 },
    /// `shl ax,cl` — D3 E0. Variable-count logical left shift of AX.
    ShlAxCl,
    /// `sar ax,cl` — D3 F8. Variable-count arithmetic (signed) right
    /// shift of AX. BCC uses SAR for signed `int >> ...`.
    SarAxCl,
    /// `shr ax,cl` — D3 E8. Variable-count logical (unsigned) right
    /// shift of AX. BCC uses SHR for `unsigned >> ...` (fixture 176).
    ShrAxCl,
    /// `shl <reg16>,cl` — D3 (mod=11 /4 r/m=<reg>). Variable-count
    /// logical left shift of any 16-bit register. Fixture 537
    /// (`int x; x <<= 4` lowers to `mov cl, 4; shl si, cl`).
    ShlReg16Cl { reg: Reg16 },
    /// `sar <reg16>,cl` — D3 (mod=11 /7 r/m=<reg>).
    SarReg16Cl { reg: Reg16 },
    /// `shr <reg16>,cl` — D3 (mod=11 /5 r/m=<reg>).
    ShrReg16Cl { reg: Reg16 },
    /// `shl <reg8>,cl` — `D2 (mod=11 /4 r/m=<reg>)`. Byte-register
    /// variable-count logical left shift. Char compound `<<=` with
    /// non-constant RHS.
    ShlReg8Cl { reg: Reg8 },
    /// `sar <reg8>,cl` — `D2 (mod=11 /7 r/m=<reg>)`. Signed byte
    /// arithmetic right shift. Used by char `>>=` (BCC picks SAR
    /// for plain `char`, fixture 670).
    SarReg8Cl { reg: Reg8 },
    /// `shr <reg8>,cl` — `D2 (mod=11 /5 r/m=<reg>)`. Unsigned-char
    /// `>>=` variant, sibling of `SarReg8Cl`.
    ShrReg8Cl { reg: Reg8 },
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
    /// `mov word ptr <group>:<dst>[+<offset>], offset <group>:<src>` —
    /// store a globals' OFFSET as an immediate into another global's
    /// slot. Encodes as `C7 06 <dst-disp> <src-imm16>` with TWO
    /// FIXUPPs: one on the dst displacement, one on the imm16. Used
    /// by `p = &x;` where both `p` and `x` are file-scope globals
    /// (fixture 480).
    MovGroupSymOffsetGroupSym {
        dst_group: String,
        dst_symbol: String,
        dst_offset: i16,
        src_group: String,
        src_symbol: String,
        src_offset: i16,
    },
    /// `mov byte ptr <group>:<symbol>[+<offset>], imm8` — store
    /// immediate byte to a data-segment global. Encodes as
    /// `C6 06 [disp16 + FIXUPP] [imm8]`. Used by `c = 'A'` for char
    /// globals (fixture 449).
    MovGroupSymImm8 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u8,
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
    /// `adc <reg16>,<imm8sx>` — `83 D(reg) ii`. ModR/M D(reg) =
    /// mod=11 /2(ADC) r/m=<reg>. Used for the high-half carry
    /// propagation in long-return arith (e.g. `adc dx, 0` after
    /// `add ax, K`). Fixture 362.
    AdcReg16Imm8Sx { reg: Reg16, imm: i8 },
    /// `sbb <reg16>,<imm8sx>` — `83 D(reg) ii`. ModR/M D(reg) =
    /// mod=11 /3(SBB) r/m=<reg>. Used for the high-half borrow
    /// back-propagation in long unary neg at return boundary
    /// (e.g. `sbb dx, 0` closing out `neg dx / neg ax`). Fixture
    /// 371.
    SbbReg16Imm8Sx { reg: Reg16, imm: i8 },
    /// `add <reg16>, imm16` — Grp1 r/m16,imm16. Encoding
    /// `81 C(rm) lo hi` where ModR/M is `mod=11 /0(ADD) rm=<reg>`.
    /// Wider sibling to `AddReg16Imm8Sx` when the immediate doesn't
    /// fit i8sx (fixture 275: `add dx,1000`).
    AddReg16Imm16 { reg: Reg16, imm: u16 },
    /// `sub <reg16>,<imm8sx>` — `83 E(reg) ii`. ModR/M E(reg) =
    /// mod=11 /5(SUB) r/m=<reg>. Sibling of `AddReg16Imm8Sx` for
    /// pointer-arithmetic compound subtract (fixture 564: `p -=
    /// 2;` lowers to `sub si, 4`).
    SubReg16Imm8Sx { reg: Reg16, imm: i8 },
    /// `sub <reg16>, imm16` — `81 E(reg) lo hi`. Wide-immediate
    /// sibling.
    SubReg16Imm16 { reg: Reg16, imm: u16 },
    /// `or <reg16>, imm16` — `81 C(reg|8) lo hi`. Grp1 /1=OR with
    /// ModR/M mod=11 r/m=<reg>. Used by long-return bitwise paths
    /// where the high-half OR with a constant is emitted as a
    /// dedicated `or dx, hi_k` (fixture 2876: `a | 0x100L`).
    OrReg16Imm16 { reg: Reg16, imm: u16 },
    /// `and <reg16>, imm16` — `81 E(reg) lo hi`. Grp1 /4=AND
    /// sibling.
    AndReg16Imm16 { reg: Reg16, imm: u16 },
    /// `xor <reg16>, imm16` — `81 F(reg) lo hi`. Grp1 /6=XOR
    /// sibling.
    XorReg16Imm16 { reg: Reg16, imm: u16 },
    /// `add word ptr <group>:<symbol>[+<offset>], imm16` — Grp1
    /// r/m16,imm16 with /0=ADD (`81 06 lo hi imm_lo imm_hi`,
    /// 6 bytes). Wider sibling to `AddGroupSymImm8Sx` for
    /// compound assigns where K doesn't fit i8sx (fixture 276).
    AddGroupSymImm16 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u16,
    },
    /// `adc ax, imm16` — `15 lo hi`. AX-specific add-with-carry
    /// short form. Fixture 207's high-half carry propagation.
    AdcAxImm16 { imm: u16 },
    /// `sbb ax, imm16` — `1D lo hi`. AX-specific subtract-with-borrow
    /// short form. Used by long unary minus (fixture 226) to combine
    /// the carry from the low-half `neg` into the high half.
    SbbAxImm16 { imm: u16 },
    /// `and ax, imm16` — `25 lo hi`. AX-specific bitwise-AND short
    /// form. Used by `c & K` after the char load + cbw widen (fixture
    /// 609 — `25 04 00`).
    AndAxImm16 { imm: u16 },
    /// `or ax, imm16` — `0D lo hi`. AX-specific bitwise-OR short
    /// form (fixture 611's `x | 8` → `0D 08 00`).
    OrAxImm16 { imm: u16 },
    /// `xor ax, imm16` — `35 lo hi`. AX-specific bitwise-XOR short
    /// form (fixture 612's `x ^ 3` → `35 03 00`).
    XorAxImm16 { imm: u16 },
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
    /// `mov <reg8>,byte ptr <group>:<symbol>[+<offset>]` for non-AL
    /// dst — `8A (mod=00 reg=<r> r/m=110) lo hi` + FIXUPP. AL keeps
    /// its shorter `A0`-form `MovAlGroupSym`. Used when BCC loads a
    /// byte global into a non-AL byte register, e.g. `mov cl, byte
    /// ptr DGROUP:_h` for long-shift-by-long-variable RHS (fixture
    /// 739).
    MovReg8GroupSym {
        reg: Reg8,
        group: String,
        symbol: String,
        offset: i16,
    },
    /// `mov <reg8>, byte ptr <group>:<sym>[bx+disp]` — bx-indexed
    /// byte load. Encoding `8A (mod=10 reg=<r> r/m=111) lo hi` +
    /// FIXUPP. Fixture 2613 (`arr[i]` for char global array, var
    /// index in BX → `mov al, byte ptr DGROUP:_arr[bx]`).
    MovReg8GroupSymBxDisp {
        reg: Reg8,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `mov byte ptr <group>:<sym>[bx+disp], <reg8>` — bx-indexed
    /// byte store. Encoding `88 (mod=10 reg=<r> r/m=111) lo hi` +
    /// FIXUPP. Sibling of the above for write-back.
    MovGroupSymBxDispReg8 {
        reg: Reg8,
        group: String,
        symbol: String,
        disp: u16,
    },
    /// `mov byte ptr <group>:<sym>[bx+disp], imm8` — bx-indexed
    /// byte store with immediate. Encoding `C6 87 lo hi ii`.
    MovGroupSymBxDispImm8 {
        group: String,
        symbol: String,
        disp: u16,
        imm: u8,
    },
    /// `mov byte ptr <group>:<symbol>[+<offset>], al` — 8-bit moffs8
    /// store (`A2 lo hi`) + FIXUPP. AL-specific short form for
    /// writing back to a data-segment byte global; used by the
    /// char-global compound-with-constant load-modify-store path
    /// (fixture 683: `g += 5` → `mov al, _g; add al, 5; mov _g,
    /// al`).
    MovGroupSymAl { group: String, symbol: String, offset: i16 },
    /// `mov byte ptr <group>:<symbol>[+<offset>], <reg8>` for non-AL
    /// source — `88 (mod=00 reg=<r> r/m=110) lo hi` + FIXUPP. AL
    /// keeps the shorter `A2` form via `MovGroupSymAl`. Used by
    /// char-global `%= K` to store DL (low byte of the 16-bit idiv
    /// remainder) back: fixture 692 → `88 16 lo hi`.
    MovGroupSymReg8 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg8,
    },
    /// `mov al,byte ptr [bx+si]` — 8A 00. ModR/M 00 = mod=00 reg=AL
    /// r/m=000 ([bx+si]). Indexed byte load via BX base + SI index.
    /// Fixture 1420 (`t += s[i]` for char* s, i in SI — BCC folds
    /// the index-add into the memory operand).
    MovAlFromBxSi,
    /// `mov al,byte ptr [bx+di]` — 8A 01. Sibling for the DI index.
    MovAlFromBxDi,
    /// `mov byte ptr [bx+si], imm8` — C6 00 ii. Indexed byte store.
    /// Fixture 3559 (`buf[i] = 0` for char* buf, i in SI).
    MovBxSiPtrImm8 { imm: u8 },
    /// `mov byte ptr [bx+di], imm8` — C6 01 ii. Sibling.
    MovBxDiPtrImm8 { imm: u8 },
    /// `mov al,byte ptr [si]` — 8A 04. 8-bit load through SI pointer.
    MovAlFromSiPtr,
    /// `mov al,byte ptr [bx]` — 8A 07. 8-bit load through BX pointer.
    /// Fixture 192 dereferences a global char pointer via BX.
    MovAlFromBxPtr,
    /// `mov al,byte ptr [di]` — 8A 05. 8-bit load through DI pointer.
    /// Fixture 1346 paired char-copy with src in DI.
    MovAlFromDiPtr,
    /// `imul <reg16>` — F7 (mod=11 /5 r/m=reg). Single-operand signed
    /// multiply with a register operand. Used when the operand is
    /// register-resident, e.g. `x *= 3` after BCC enregisters x.
    ImulReg16 { reg: Reg16 },
    /// `idiv <reg16>` — F7 (mod=11 /7 r/m=reg). Single-operand signed
    /// divide with a register operand. Used for `int reg-local /= K`
    /// (fixture 584) where BCC loads the divisor into BX.
    IdivReg16 { reg: Reg16 },
    /// `div <reg16>` — F7 (mod=11 /6 r/m=reg). Single-operand
    /// unsigned divide with a register operand. Sibling of
    /// `IdivReg16` for the unsigned-RHS path with an immediate
    /// divisor (BCC loads K into BX, then `div bx`). Fixture 948.
    DivReg16 { reg: Reg16 },
    /// `add ax,word ptr <group>:<symbol>[+<offset>]` — ADD r16,r/m16
    /// with disp16-only addressing (`03 06 lo hi`). Same FIXUPP
    /// shape; offset added to the symbol's location.
    AddAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `add ax,offset <group>:<symbol>[+<offset>]` — AX-accumulator
    /// ADD with an immediate symbol offset (`05 lo hi`). Used for
    /// pointer arithmetic `arr + <expr>`: scale the int into AX,
    /// then add the array base symbol as a link-time-resolved
    /// immediate. Fixture 3439 (`arr + (c ? 1 : 2)`).
    AddAxOffsetGroupSym { group: String, symbol: String, offset: i16 },
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
    /// `adc dx,word ptr <group>:<symbol>[+<offset>]` — ADC r16,r/m16
    /// with DX dst (`13 16 lo hi`). Companion to `AdcAxGroupSym`
    /// for the commuted `g = i + g` shape (fixture 281).
    AdcDxGroupSym { group: String, symbol: String, offset: i16 },
    /// `add word ptr <group>:<symbol>[+<offset>], dx` — ADD r/m16,r16
    /// (`01 16 lo hi`). Memory-destination low-half add of DX into
    /// a global/struct-field low half, for `s.x += y` (variable RHS)
    /// at a struct-field destination. Fixture 391.
    AddGroupSymDx { group: String, symbol: String, offset: i16 },
    /// `adc word ptr <group>:<symbol>[+<offset>], ax` — ADC r/m16,r16
    /// (`11 06 lo hi`). High-half carry partner to `AddGroupSymDx`
    /// for struct-field `+=` with variable RHS. Fixture 391.
    AdcGroupSymAx { group: String, symbol: String, offset: i16 },
    /// `sbb word ptr <group>:<symbol>[+<offset>], ax` — SBB r/m16,r16
    /// (`19 06 lo hi`). High-half borrow partner for long-global
    /// `g -= h` with both operands global (fixture 735).
    SbbGroupSymAx { group: String, symbol: String, offset: i16 },
    /// `adc word ptr <group>:<symbol>[+<offset>], dx` — ADC r/m16,r16
    /// with DX source (`11 16 lo hi`). Used by long-global `+= int`
    /// after `cwd` puts the high-half sign-extension in DX
    /// (fixture 755).
    AdcGroupSymDx { group: String, symbol: String, offset: i16 },
    /// `sbb word ptr <group>:<symbol>[+<offset>], dx` — SBB r/m16,r16
    /// with DX source (`19 16 lo hi`). Sibling for `long -= int`.
    SbbGroupSymDx { group: String, symbol: String, offset: i16 },
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
    /// `cmp ax,word ptr <group>:<symbol>[+<offset>]` — CMP r16,r/m16
    /// with AX dst (`3B 06 lo hi`). High-half comparison for the
    /// signed long-compare 3-jump pattern (fixture 234).
    CmpAxGroupSym { group: String, symbol: String, offset: i16 },
    /// `cmp dx,word ptr <group>:<symbol>[+<offset>]` — CMP r16,r/m16
    /// with DX dst (`3B 16 lo hi`). Low-half companion for the
    /// signed long-compare 3-jump pattern (fixture 234).
    CmpDxGroupSym { group: String, symbol: String, offset: i16 },
    /// `push word ptr <group>:<symbol>[+<offset>]` — `FF 36 lo hi`,
    /// `FF /6 r/m16` with disp16-only addressing. Used by BCC to
    /// push long-arith helper arguments onto the stack (e.g.
    /// fixture 232's `N_LDIV@` call).
    PushGroupSym { group: String, symbol: String, offset: i16 },
    /// `push word ptr [bp+disp8]` — `FF 76 dd`. ModR/M 76 =
    /// mod=01 /6(PUSH) r/m=110 ([bp+disp8]). Used to push a long
    /// argument's halves from a stack local (fixture 323).
    PushBpRel { offset: i16 },
    /// `push word ptr [si]` — `FF 34`. ModR/M 34 = mod=00 /6(PUSH)
    /// r/m=100 ([si]). Low-half push for long-pointer deref-arg
    /// (fixture 325).
    PushSiPtr,
    /// `push word ptr [si+disp8]` — `FF 74 dd`. ModR/M 74 = mod=01
    /// /6(PUSH) r/m=100 ([si+disp8]). High-half push for
    /// long-pointer deref-arg (fixture 325).
    PushSiDisp { disp: i8 },
    /// `push ds` — `1E` (single byte). Pushes the DS segment
    /// register, used to form a far pointer to a DGROUP-resident
    /// symbol before calling helpers like `N_SCOPY@` that take
    /// far-pointer arguments. Fixture 413.
    PushDs,
    /// `push ss` — `16` (single byte). Used analogously to
    /// `PushDs` when the far-pointer target is a stack-resident
    /// location: the segment half of the far pointer is SS rather
    /// than DS. Fixture 416 (stack-destination struct copy via
    /// `N_SCOPY@`).
    PushSs,
    /// `mov <reg16>,<segreg>` — `8C` + ModR/M `mod=11 reg=<sreg>
    /// r/m=<reg16>`. Copies a segment register's value into a
    /// general-purpose register. BCC uses this to form the segment
    /// half of a far pointer in DX before calling helpers that take
    /// `DX:AX` far pointers (e.g. `N_SPUSH@`). The seg reg field is
    /// 011=DS, 010=SS, 000=ES, 001=CS. Fixture 420 (`mov dx,ds`),
    /// future stack-source variant (`mov dx,ss`).
    MovReg16SegReg { dst: Reg16, src: SegReg },
    /// `cmp word ptr <group>:<symbol>[+<offset>], imm16` — Grp1
    /// r/m16,imm16 with /7=CMP and disp16-only addressing
    /// (`81 3E lo hi imm_lo imm_hi`, 6 bytes). Used when K is too
    /// wide for i8sx in the chained-cmp pattern (fixture 282).
    CmpGroupSymImm16 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u16,
    },
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
    /// `cmp byte ptr <group>:<symbol>[+<offset>], imm8` — Grp1
    /// r/m8,imm8 with /7=CMP and disp16-only addressing
    /// (`80 3E lo hi ii`, 5 bytes). Used by char-global compare
    /// against constants (fixture 452: `if (c == 'A')`).
    CmpByteGroupSymImm8 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u8,
    },
    /// `cmp byte ptr [bp+<offset>], imm8` — Grp1 r/m8,imm8 with
    /// /7=CMP and bp-relative addressing
    /// (`80 7E disp8 ii`, 4 bytes). Used by char-local compare
    /// against constants (fixture 524: `if (c == 'B')`).
    CmpByteBpRelImm8 { offset: i16, imm: u8 },
    /// `cmp word ptr [si], imm8sx` — `83 3C ii` (3 bytes).
    /// `cmp word ptr [di], imm8sx` — `83 3D ii` (3 bytes).
    /// `cmp word ptr [bx], imm8sx` — `83 3F ii` (3 bytes).
    /// Memory-direct word compare through a register-resident
    /// pointer with a small sign-extended immediate. Fixture 2925
    /// (`if (*p && *q)` — short-circuit eval needs `cmp word ptr
    /// [<reg>], 0`).
    CmpWordSiPtrImm8Sx { imm: i8 },
    CmpWordDiPtrImm8Sx { imm: i8 },
    CmpWordBxPtrImm8Sx { imm: i8 },
    /// Wide-immediate sibling for constants outside [-128, 127].
    CmpWordSiPtrImm16 { imm: u16 },
    CmpWordDiPtrImm16 { imm: u16 },
    CmpWordBxPtrImm16 { imm: u16 },
    /// `cmp byte ptr [si], imm8` — `80 3C ii` (3 bytes). Grp1 /7=CMP
    /// with ModR/M `3C` = mod=00 r/m=100 ([si]). Used by `while
    /// (*p)` on a char pointer enregistered in SI (fixture 636).
    CmpByteSiPtrImm8 { imm: u8 },
    /// `cmp byte ptr [bx], imm8` — `80 3F ii` (3 bytes). BX sibling
    /// of `CmpByteSiPtrImm8`. Used after a postinc/postdec saves the
    /// pre-update pointer in BX (fixture 2027: `while (*s++)` for
    /// `char *s` enregistered in DI).
    CmpByteBxPtrImm8 { imm: u8 },
    /// `cmp byte ptr [di], imm8` — `80 3D ii` (3 bytes). DI sibling
    /// of `CmpByteSiPtrImm8`. Used by `while (*++p)` for char* p
    /// enregistered in DI (fixture 1311).
    CmpByteDiPtrImm8 { imm: u8 },
    /// `cmp ax, word ptr [di]` — `3B 05` (2 bytes). Compare AX
    /// against a word read through DI. Fixture 1352
    /// (`*a - *b == 0` with b in DI: load *a to AX, then compare
    /// against *b via this form).
    CmpAxFromDiPtr,
    /// `cmp ax, word ptr [si]` — `3B 04` (2 bytes). SI sibling.
    CmpAxFromSiPtr,
    /// `cmp ax, word ptr [bx]` — `3B 07` (2 bytes). BX sibling.
    CmpAxFromBxPtr,
    /// `cmp al, byte ptr [si|di|bx]` — `3A 04|05|07` (2 bytes).
    /// Byte-form sibling for char-pointer deref compares.
    /// Fixture 1352 (`*a == *b` with both char* in SI/DI).
    CmpAlFromSiPtr,
    CmpAlFromDiPtr,
    CmpAlFromBxPtr,
    /// `cmp word ptr [si+disp], imm8sx` — Grp1 r/m16,imm8sx with
    /// /7=CMP and SI-indirect addressing. disp=0 encodes as
    /// `83 3C ii` (mod=00, 3 bytes); disp!=0 fitting i8 encodes as
    /// `83 7C dd ii` (mod=01, 4 bytes). Used by the arrow-field
    /// memory-direct compare peephole (`if (p->x == K)` with p in
    /// SI). Fixture 1007.
    CmpWordSiDispImm8Sx { disp: i16, imm: i8 },
    /// `inc word ptr [bp+<offset>]` — FF 46 dd. Grp5 /0 against a
    /// bp-relative stack local. Fixture 547 (`++a[1]` on an int
    /// local array → `inc word ptr [bp-4]`).
    IncBpRel { offset: i16 },
    /// `dec word ptr [bp+<offset>]` — FF 4E dd. Companion to
    /// `IncBpRel`.
    DecBpRel { offset: i16 },
    /// `shl word ptr <group>:<symbol>[+<offset>],1` — D1 /4 r/m16,1
    /// against a data-segment global. Encoding: `D1 26 lo hi`.
    /// Fixture 539 (`int g; g <<= 2` unrolls to two such).
    ShlGroupSymOne { group: String, symbol: String, offset: i16 },
    /// `sar word ptr <group>:<symbol>[+<offset>],1` — D1 /7 r/m16,1.
    /// Encoding: `D1 3E lo hi`. Signed `>>= 1` on int global.
    SarGroupSymOne { group: String, symbol: String, offset: i16 },
    /// `shr word ptr <group>:<symbol>[+<offset>],1` — D1 /5 r/m16,1.
    /// Encoding: `D1 2E lo hi`. Unsigned `>>= 1` on uint global.
    ShrGroupSymOne { group: String, symbol: String, offset: i16 },
    /// `shl byte ptr <group>:<symbol>[+<offset>],1` — `D0 /4 r/m8,1`
    /// against a byte data-segment global. Encoding:
    /// `D0 26 lo hi` + FIXUPP. 8-bit sibling of `ShlGroupSymOne`.
    /// Char-global `<<=` unrolls K iterations of this for small K
    /// (fixture 688: `g <<= 2` → two `shl byte ptr _g, 1`).
    ShlGroupSymByteOne { group: String, symbol: String, offset: i16 },
    /// `sar byte ptr <group>:<symbol>[+<offset>],1` — D0 /7 r/m8,1.
    /// Encoding: `D0 3E lo hi`. Signed char `>>=` low-K unroll.
    SarGroupSymByteOne { group: String, symbol: String, offset: i16 },
    /// `shr byte ptr <group>:<symbol>[+<offset>],1` — D0 /5 r/m8,1.
    /// Encoding: `D0 2E lo hi`. Unsigned char `>>=` low-K unroll.
    ShrGroupSymByteOne { group: String, symbol: String, offset: i16 },
    /// `shl word ptr <group>:<symbol>[+<offset>],cl` — `D3 /4 r/m16`
    /// with mod=00 r/m=110 → `D3 26 lo hi` + FIXUPP. Int-global
    /// `<<= x` for non-constant shift count (fixture 805:
    /// `mov cl, byte ptr [bp-2]; shl word ptr _g, cl`).
    ShlGroupSymCl { group: String, symbol: String, offset: i16 },
    /// `sar word ptr <group>:<symbol>[+<offset>],cl` — `D3 3E lo hi`.
    /// Signed int-global `>>= x` sibling of `ShlGroupSymCl`.
    SarGroupSymCl { group: String, symbol: String, offset: i16 },
    /// `shr word ptr <group>:<symbol>[+<offset>],cl` — `D3 2E lo hi`.
    /// Unsigned int-global `>>= x` sibling of `ShlGroupSymCl`.
    ShrGroupSymCl { group: String, symbol: String, offset: i16 },
    /// `shl byte ptr <group>:<symbol>[+<offset>],cl` — `D2 /4 r/m8`
    /// with mod=00 r/m=110 → `D2 26 lo hi` + FIXUPP. Char-global
    /// `<<= d` for non-constant shift count (fixture 697:
    /// `mov cl, byte ptr [bp-1]; shl byte ptr _g, cl`).
    ShlGroupSymByteCl { group: String, symbol: String, offset: i16 },
    /// `sar byte ptr <group>:<symbol>[+<offset>],cl` — `D2 3E lo hi`.
    /// Signed char-global `>>= d` sibling of `ShlGroupSymByteCl`.
    SarGroupSymByteCl { group: String, symbol: String, offset: i16 },
    /// `shr byte ptr <group>:<symbol>[+<offset>],cl` — `D2 2E lo hi`.
    /// Unsigned char-global `>>= d` sibling.
    ShrGroupSymByteCl { group: String, symbol: String, offset: i16 },
    /// `inc byte ptr <group>:<symbol>[+<offset>]` — `FE /0 r/m8`
    /// with mod=00 r/m=110 → `FE 06 lo hi` + FIXUPP. Memory-direct
    /// byte increment on a data-segment global. BCC uses this only
    /// for the **post-increment, discarded** path (fixture 702:
    /// `g++;` standalone) — pre-increment routes through AL
    /// instead (fixture 700).
    IncGroupSymByte { group: String, symbol: String, offset: i16 },
    /// `dec byte ptr <group>:<symbol>[+<offset>]` — `FE 0E lo hi`
    /// (Grp4 /1). Sibling of `IncGroupSymByte` for `g--;`.
    DecGroupSymByte { group: String, symbol: String, offset: i16 },
    /// `inc byte ptr [bp+<offset>]` — `FE 46 dd` (Grp4 /0 r/m8,
    /// mod=01 r/m=110). Memory-direct byte increment on a stack
    /// local. Used by char-local-array `a[K]++` discarded
    /// (fixture 721).
    IncBpRelByte { offset: i16 },
    /// `dec byte ptr [bp+<offset>]` — `FE 4E dd`. Sibling for `--`.
    DecBpRelByte { offset: i16 },
    /// `add word ptr <group>:<symbol>[+<offset>], <reg16>` — Grp1
    /// r/m16, r16 with /0=ADD. Encoding: `01 (mod=00 reg=<reg>
    /// r/m=110) lo hi`. Fixture 571 (`a += b;` between two int
    /// globals: load b to ax, then `add [_a], ax`).
    AddGroupSymReg16 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg16,
    },
    /// `sub word ptr <group>:<symbol>[+<offset>], <reg16>` — Grp1
    /// r/m16, r16 with /5=SUB. Encoding: `29 (mod=00 reg=<reg>
    /// r/m=110) lo hi`. Sibling of `AddGroupSymReg16`.
    SubGroupSymReg16 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg16,
    },
    /// `and word ptr <group>:<symbol>[+<offset>], <reg16>` — `21 /r`
    /// with mod=00 r/m=110. Long-global `g &= h` lowers both halves
    /// through this shape (fixture 736).
    AndGroupSymReg16 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg16,
    },
    /// `or word ptr <group>:<symbol>[+<offset>], <reg16>` — `09 /r`.
    OrGroupSymReg16 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg16,
    },
    /// `xor word ptr <group>:<symbol>[+<offset>], <reg16>` — `31 /r`.
    XorGroupSymReg16 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg16,
    },
    /// `add byte ptr <group>:<symbol>[+<offset>], <reg8>` — 8-bit
    /// sibling of `AddGroupSymReg16`. Encoding: `00 (mod=00
    /// reg=<reg> r/m=110) lo hi`. Char compound `+=` on a global
    /// with non-constant RHS (fixture 680: `g += d` with d already
    /// loaded into AL → `add byte ptr DGROUP:_g, al` = `00 06 lo
    /// hi` + FIXUPP).
    AddGroupSymReg8 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg8,
    },
    /// `sub byte ptr <group>:<symbol>[+<offset>], <reg8>` — `28 /r`
    /// sibling. Char compound `-=` on a global (fixture 681).
    SubGroupSymReg8 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg8,
    },
    /// `and byte ptr <group>:<symbol>[+<offset>], <reg8>` — `20 /r`
    /// sibling. Char compound `&=` on a global (fixture 682).
    AndGroupSymReg8 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg8,
    },
    /// `or byte ptr <group>:<symbol>[+<offset>], <reg8>` — `08 /r`
    /// sibling.
    OrGroupSymReg8 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg8,
    },
    /// `xor byte ptr <group>:<symbol>[+<offset>], <reg8>` — `30 /r`
    /// sibling.
    XorGroupSymReg8 {
        group: String,
        symbol: String,
        offset: i16,
        reg: Reg8,
    },
    /// `and byte ptr <group>:<symbol>[+<offset>], imm8` — Grp1 r/m8,
    /// imm8 with /4=AND. Encoding: `80 26 lo hi ii` (ModR/M 26 =
    /// mod=00 reg=100 r/m=110). Char-global compound `&=` with a
    /// constant RHS (fixture 685: `g &= 15` → `80 26 lo hi 0F`).
    /// Bitwise ops use memory-direct on globals while arith ops
    /// (+/-) take an AL load-modify-store detour.
    AndGroupSymImm8 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u8,
    },
    /// `or byte ptr <group>:<symbol>[+<offset>], imm8` — Grp1 /1
    /// sibling. Encoding: `80 0E lo hi ii`.
    OrGroupSymImm8 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u8,
    },
    /// `xor byte ptr <group>:<symbol>[+<offset>], imm8` — Grp1 /6
    /// sibling. Encoding: `80 36 lo hi ii`.
    XorGroupSymImm8 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u8,
    },
    /// `test word ptr <group>:<symbol>[+<offset>], imm16` — TEST
    /// r/m16, imm16 via Grp3 /0 against a data-segment global.
    /// Encoding: `F7 06 lo hi imm_lo imm_hi` (ModR/M 06 = mod=00
    /// reg=000(/0=TEST) r/m=110 → `[disp16]`). Sets ZF/SF from the
    /// AND result without storing it. Fixture 569 (`if (g & 1)`).
    TestGroupSymImm16 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u16,
    },
    /// `test word ptr [bp+<offset>], imm16` — TEST r/m16, imm16 via
    /// Grp3 /0 against a stack-local. Encoding: `F7 46 dd lo hi`
    /// (mod=01 /0(TEST) r/m=110(BP) disp8). Fixture 1853 (`if (x &
    /// 0x40)` for int local x).
    TestBpRelImm16 { offset: i16, imm: u16 },
    /// `test word ptr [bp+<offset>], ax` — TEST r/m16, r16. Encoding:
    /// `85 46 dd` (mod=01 reg=AX r/m=110(BP) disp8). Used for `(x &
    /// y) != 0` where both x and y are int lvalues (fixture 3539).
    TestBpRelAx { offset: i16 },
    /// `test <reg16>, imm16` — TEST r/m16, imm16 via Grp3 /0 against
    /// a register. Encoding: `F7 (0xC0+reg.code()) imm_lo imm_hi`
    /// (ModR/M mod=11 reg=000(/0=TEST) r/m=reg). Sets flags from the
    /// AND result without storing it. Fixture 1415 (`if (x & 1)` with
    /// x in SI — popcount inner loop).
    TestReg16Imm16 {
        reg: Reg16,
        imm: u16,
    },
    /// `inc word ptr <group>:<symbol>[+<offset>]` — INC r/m16 via
    /// Grp5 /0 against a data-segment global. Encoding: `FF 06 lo
    /// hi` (ModR/M 06 = mod=00 reg=000 r/m=110 → `[disp16]`).
    /// Fixture 512 (`int g; g++;`).
    IncGroupSym {
        group: String,
        symbol: String,
        offset: i16,
    },
    /// `dec word ptr <group>:<symbol>[+<offset>]` — DEC r/m16 via
    /// Grp5 /1 against a data-segment global. Encoding: `FF 0E lo
    /// hi`.
    DecGroupSym {
        group: String,
        symbol: String,
        offset: i16,
    },
    /// `add word ptr <group>:<symbol>[+<offset>], imm8 (sx)` — Grp1
    /// r/m16,imm8sx with /0=ADD (`83 06 lo hi ii`, 5 bytes). Used
    /// by long postfix `g++` (fixture 249): low-half memory-add of 1.
    AddGroupSymImm8Sx {
        group: String,
        symbol: String,
        offset: i16,
        imm: i8,
    },
    /// `adc word ptr <group>:<symbol>[+<offset>], imm8 (sx)` — Grp1
    /// r/m16,imm8sx with /2=ADC (`83 16 lo hi ii`, 5 bytes). Used
    /// as the carry-propagation high-half partner to
    /// `AddGroupSymImm8Sx` for long `g++` / `g += K` patterns
    /// (fixture 249).
    AdcGroupSymImm8Sx {
        group: String,
        symbol: String,
        offset: i16,
        imm: i8,
    },
    /// `sub word ptr <group>:<symbol>[+<offset>], imm8 (sx)` — Grp1
    /// r/m16,imm8sx with /5=SUB (`83 2E lo hi ii`, 5 bytes). Used
    /// by long postfix `g--` (fixture 250): low-half memory-sub of 1.
    SubGroupSymImm8Sx {
        group: String,
        symbol: String,
        offset: i16,
        imm: i8,
    },
    /// `sbb word ptr <group>:<symbol>[+<offset>], imm8 (sx)` — Grp1
    /// r/m16,imm8sx with /3=SBB (`83 1E lo hi ii`, 5 bytes). Used
    /// as the borrow-propagation high-half partner to
    /// `SubGroupSymImm8Sx` for long `g--` / `g -= K` (fixture 250).
    SbbGroupSymImm8Sx {
        group: String,
        symbol: String,
        offset: i16,
        imm: i8,
    },
    /// `and word ptr <group>:<symbol>[+<offset>], imm16` — Grp1
    /// r/m16,imm16 with /4=AND (`81 26 lo hi imm_lo imm_hi`, 6 bytes).
    /// BCC uses the wider imm16 form for bitwise compound assigns
    /// even when the immediate fits in an i8sx — unlike arithmetic
    /// `+=`/`-=` which use the shorter `83` form. Fixture 253.
    AndGroupSymImm16 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u16,
    },
    /// `or word ptr <group>:<symbol>[+<offset>], imm16` — Grp1
    /// r/m16,imm16 with /1=OR (`81 0E lo hi imm_lo imm_hi`, 6 bytes).
    /// Same imm16-always rule as `AndGroupSymImm16`.
    OrGroupSymImm16 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u16,
    },
    /// `xor word ptr <group>:<symbol>[+<offset>], imm16` — Grp1
    /// r/m16,imm16 with /6=XOR (`81 36 lo hi imm_lo imm_hi`, 6 bytes).
    /// Same imm16-always rule as `AndGroupSymImm16`.
    XorGroupSymImm16 {
        group: String,
        symbol: String,
        offset: i16,
        imm: u16,
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
    /// `mov word ptr [si],<reg16>` — 89 (mod=00 reg=<src> r/m=100).
    /// Register-store through SI pointer. Used by non-constant
    /// `*p = v` (fixture 595: `*p = *p + 1` → `mov [si], ax`).
    MovSiPtrReg16 { src: Reg16 },
    /// `mov word ptr [di],<reg16>` — 89 (mod=00 reg=<src> r/m=101).
    /// Companion to `MovSiPtrReg16` for the second pointer-local
    /// when BCC enregisters two pointers (fixture 628's `*p = x`
    /// with `p` in DI).
    MovDiPtrReg16 { src: Reg16 },
    /// `mov byte ptr [si], imm8` — `C6 04 ii`. Byte-store immediate
    /// through the SI pointer. ModR/M `04` = `mod=00 /0 r/m=100
    /// ([si])`. Used by `*p = K;` for uchar pointers (fixture 465).
    MovSiPtrImm8 { imm: u8 },
    /// `mov byte ptr [si], <reg8>` — `88 (mod=00 reg=<r> r/m=100)`.
    /// 8-bit reg-to-mem store through SI. For AL → `88 04`. Used
    /// by char-via-pointer compound when the destination is
    /// dereferenced through a register-resident pointer (fixture
    /// 710: `p->c += 5` with p in SI → `mov al, [si]; add al, 5;
    /// mov [si], al`).
    MovSiPtrReg8 { src: Reg8 },
    /// `mov word ptr [si+disp8],imm16` — `C7 44 dd lo hi`. Companion
    /// to `MovSiPtrImm` for the high-half store of a long-pointer
    /// write (`*p = K` where `p: long *`). Fixture 308.
    MovSiDispImm { disp: i8, imm: u16 },
    /// `mov word ptr [si+disp8], <reg16>` — `89 (mod=01 reg=<r>
    /// r/m=100) dd`. Used for struct-field write through a
    /// register pointer in SI. Fixture 1955 (`p->y = z` where p in
    /// SI).
    MovSiDispReg16 { disp: i8, reg: Reg16 },
    /// `mov ax,word ptr [si+disp8]` — `8B 44 dd`. ModR/M 44 = mod=01
    /// reg=AX r/m=100 ([si+disp8]). High-half read for `*p` where
    /// `p: long *` (fixture 309).
    MovAxSiDisp { disp: i8 },
    /// `mov dx,word ptr [si]` — `8B 14`. ModR/M 14 = mod=00 reg=DX
    /// r/m=100. Low-half read for `*p` where `p: long *` (fixture
    /// 309). The displacement-less form is `8B 14`, distinct from
    /// `MovAxSiDisp` with disp=0 because BCC picks the shorter
    /// encoding when no displacement is needed.
    MovDxFromSiPtr,
    /// `mov <reg16>,word ptr [si]` — generic 16-bit load through SI
    /// for the chained-pointer indirection paths (e.g. fixture 2816's
    /// `mov bx,[si]` in `o->p->v`). Encoding: `8B (reg<<3 | 0x04)`
    /// (mod=00 r/m=100 → [SI]). Existing AX-/DX-specific variants stay
    /// for symmetry with their imm8/disp8 siblings.
    MovReg16FromSiPtr { reg: Reg16 },
    /// `mov <reg16>,word ptr [si+disp8]` — sibling with an 8-bit
    /// displacement. Encoding: `8B (mod=01 reg=<dst> r/m=100) dd`.
    MovReg16SiDisp { reg: Reg16, disp: i8 },
    /// `mov <reg16>,word ptr [di]` — DI sibling of
    /// [`Self::MovReg16FromSiPtr`]. Encoding: `8B (reg<<3 | 0x05)`
    /// (mod=00 r/m=101 → [DI]). Used when BCC enregisters two
    /// pointers and the source is the DI-bound one (fixture 2495:
    /// `*dst = *src` with src in DI → `mov ax, [di]`).
    MovReg16FromDiPtr { reg: Reg16 },
    /// `mov <reg16>,word ptr [di+disp8]` — sibling with disp8.
    /// Encoding: `8B (mod=01 reg=<dst> r/m=101) dd`.
    MovReg16DiDisp { reg: Reg16, disp: i8 },
    /// `mov dx,word ptr [si+disp8]` — `8B 54 dd`. ModR/M 54 = mod=01
    /// reg=DX(010) r/m=100 ([si+disp8]). High-half read for `*p`
    /// where `p: long *` in the ABI return convention (DX=high).
    /// Fixture 351.
    MovDxSiDisp { disp: i8 },
    /// `add word ptr [si],<imm8 sign-extended>` — 83 04 ii. Read-
    /// modify-write through SI used by compound member assignment
    /// `p->x += K` when SI holds `p` (fixture 182).
    AddSiPtrImm8 { imm: i8 },
    /// `adc word ptr [si+disp8],<imm8sx>` — `83 54 dd ii`. Carry-
    /// propagation partner for long-pointer `*p += K` (fixture 311).
    AdcSiDispImm8 { disp: i8, imm: i8 },
    /// `add word ptr [si],dx` — `01 14`. ADD r/m16,r16 form; ModR/M
    /// `14` = mod=00 reg=DX(010) r/m=100=SI. Low-half memory-dest
    /// add for `*p += y` (variable RHS through a register-resident
    /// long pointer). Fixture 398.
    AddSiPtrDx,
    /// `add word ptr [si],ax` — `01 04`. ADD r/m16,r16 with
    /// reg=AX(000) r/m=100=SI. Used by `int *p; *p += y;` where
    /// the int RHS is materialized in AX. Fixture 838.
    AddSiPtrAx,
    /// `sub word ptr [si],ax` — `29 04`. Sibling for `*p -= y`.
    SubSiPtrAx,
    /// `and word ptr [si],ax` — `21 04`. Sibling for `*p &= y`.
    AndSiPtrAx,
    /// `or word ptr [si],ax` — `09 04`. Sibling for `*p |= y`.
    OrSiPtrAx,
    /// `xor word ptr [si],ax` — `31 04`. Sibling for `*p ^= y`.
    XorSiPtrAx,
    /// `add word ptr [bx+disp8],ax` — `01 47 dd`. ADD r/m16,r16 with
    /// ModR/M `47` = mod=01 reg=AX(000) r/m=111=BX. Memory-dest
    /// add through a register-resident BX pointer at a small
    /// positive offset. Used by `int *p; p[K] += y` for global
    /// pointer `p` where BCC loads the pointer into BX and emits
    /// `<op> word ptr [bx+K*2], ax` (fixture 862).
    AddBxDispAx { disp: i8 },
    /// `sub word ptr [bx+disp8],ax` — `29 47 dd`. Sibling.
    SubBxDispAx { disp: i8 },
    /// `and word ptr [bx+disp8],ax` — `21 47 dd`. Sibling.
    AndBxDispAx { disp: i8 },
    /// `or word ptr [bx+disp8],ax` — `09 47 dd`. Sibling.
    OrBxDispAx { disp: i8 },
    /// `xor word ptr [bx+disp8],ax` — `31 47 dd`. Sibling.
    XorBxDispAx { disp: i8 },
    /// `add word ptr [si+disp8],ax` — `01 44 dd`. ADD r/m16,r16
    /// with ModR/M `44` = mod=01 reg=AX(000) r/m=100=SI. Memory-
    /// dest add through a register-resident SI pointer at a small
    /// signed offset. Used by `int *p; p[K] += y` for a stack-
    /// local pointer that BCC placed in SI (fixture 863). disp=0
    /// stays with the existing `AddSiPtrAx` (2-byte form).
    AddSiDispAx { disp: i8 },
    /// `sub word ptr [si+disp8],ax` — `29 44 dd`. Sibling.
    SubSiDispAx { disp: i8 },
    /// `and word ptr [si+disp8],ax` — `21 44 dd`. Sibling.
    AndSiDispAx { disp: i8 },
    /// `or word ptr [si+disp8],ax` — `09 44 dd`. Sibling.
    OrSiDispAx { disp: i8 },
    /// `xor word ptr [si+disp8],ax` — `31 44 dd`. Sibling.
    XorSiDispAx { disp: i8 },
    /// `add word ptr [bx+disp8],<imm8sx>` — `83 47 dd ii`. Group-1
    /// `/0` with mod=01 r/m=111 = BX+disp8, imm8 sign-extended to
    /// 16. Const-RHS form of global-pointer subscript compound
    /// (fixture 864: `int *p; p[1] += 5`).
    AddBxDispImm8 { disp: i8, imm: i8 },
    /// `sub word ptr [bx+disp8],<imm8sx>` — `83 6F dd ii`. Group-1
    /// `/5` with mod=01 r/m=111 = BX+disp8.
    SubBxDispImm8 { disp: i8, imm: i8 },
    /// `mov al,byte ptr [bx+disp8]` — `8A 47 dd`. MOV r8,r/m8 with
    /// ModR/M `47` = mod=01 reg=AL(000) r/m=111=BX. 8-bit load
    /// through a BX pointer at a small offset. Used by `char *p;
    /// p[K] op= …` (fixture 865), where BCC reloads BX before
    /// the store too.
    MovAlBxDisp { disp: i8 },
    /// `mov byte ptr [bx+disp8],al` — `88 47 dd`. Store sibling.
    MovBxDispAl { disp: i8 },
    /// `and byte ptr [bx+disp8],al` — `20 47 dd`. AND r/m8,r8 with
    /// ModR/M `47` = mod=01 reg=AL(000) r/m=111=BX. Bitwise
    /// mem-direct form for `char *p; p[K] &= y` (fixture 870).
    AndBxDispAl { disp: i8 },
    /// `or byte ptr [bx+disp8],al` — `08 47 dd`. Sibling.
    OrBxDispAl { disp: i8 },
    /// `xor byte ptr [bx+disp8],al` — `30 47 dd`. Sibling.
    XorBxDispAl { disp: i8 },
    /// `and word ptr [bx+disp8],<imm16>` — `81 67 dd lo hi`. Group-1
    /// `/4` (AND) with mod=01 r/m=111=BX, imm16 form. BCC picks
    /// imm16 for AND/OR/XOR even when the constant fits a signed
    /// byte — the imm8sx peephole only applies to ADD/SUB. Const-
    /// RHS form of int-pointer subscript bitwise compound
    /// (fixture 875: `int *p; p[1] &= 15`).
    AndBxDispImm16 { disp: i8, imm: u16 },
    /// `or word ptr [bx+disp8],<imm16>` — `81 4F dd lo hi`. Group-1
    /// `/1` sibling.
    OrBxDispImm16 { disp: i8, imm: u16 },
    /// `xor word ptr [bx+disp8],<imm16>` — `81 77 dd lo hi`. Group-1
    /// `/6` sibling.
    XorBxDispImm16 { disp: i8, imm: u16 },
    /// `add word ptr [bx],ax` — `01 07`. ADD r/m16,r16 with ModR/M
    /// `07` = mod=00 reg=AX(000) r/m=111=BX. Zero-offset sibling of
    /// `AddBxDispAx` — used by `int *p; p[0] += y` (fixture 879).
    AddBxPtrAx,
    /// `sub word ptr [bx],ax` — `29 07`. Sibling.
    SubBxPtrAx,
    /// `and word ptr [bx],ax` — `21 07`. Sibling.
    AndBxPtrAx,
    /// `or word ptr [bx],ax` — `09 07`. Sibling.
    OrBxPtrAx,
    /// `xor word ptr [bx],ax` — `31 07`. Sibling.
    XorBxPtrAx,
    /// `inc word ptr [bx+disp8]` — `FF 47 dd`. Group FF `/0` (INC
    /// r/m16) with mod=01 r/m=111=BX+disp8. Used by `int *p; p[K]++`
    /// (fixture 880) and `++p[K]` — BCC's `K=1` memory-direct
    /// peephole on a pointer subscript LHS.
    IncBxDisp { disp: i8 },
    /// `dec word ptr [bx+disp8]` — `FF 4F dd`. Group FF `/1` sibling.
    DecBxDisp { disp: i8 },
    /// `inc byte ptr [bx+disp8]` — `FE 47 dd`. Group FE `/0` (INC
    /// r/m8) with mod=01 r/m=111=BX+disp8. Used by `char *p; p[K]++`
    /// (fixture 886) — the byte sibling of `IncBxDisp`.
    IncBxDispByte { disp: i8 },
    /// `dec byte ptr [bx+disp8]` — `FE 4F dd`. Group FE `/1` sibling.
    DecBxDispByte { disp: i8 },
    /// `cmp word ptr [bx+disp8],<imm8sx>` — `83 7F dd ii`. Group-1
    /// `/7` (CMP) imm8sx form with mod=01 r/m=111=BX. Used by `if
    /// (p[K])` — the zero-test BCC emits as `cmp word ptr
    /// [bx+K*2], 0` (fixture 889).
    CmpBxDispImm8 { disp: i8, imm: i8 },
    /// `shl word ptr [bx+disp8],1` — `D1 67 dd`. Group-2 `/4` (SHL)
    /// 1-bit shift with mod=01 r/m=111=BX. Used by `int *p; p[K]
    /// <<= N` (fixture 878: BCC unrolls into N repetitions of the
    /// 1-bit shift on 8086, since `C1 /4 imm8` is 186+).
    ShlBxDispImm1 { disp: i8 },
    /// `sar word ptr [bx+disp8],1` — `D1 7F dd`. Group-2 `/7`
    /// (SAR) sibling.
    SarBxDispImm1 { disp: i8 },
    /// `shr word ptr [bx+disp8],1` — `D1 6F dd`. Group-2 `/5`
    /// (SHR) sibling.
    ShrBxDispImm1 { disp: i8 },
    /// `shl word ptr [bx+disp8],cl` — `D3 67 dd`. Group-2 `/4`
    /// variable-count shift via CL. Used by `int *p; p[K] <<= y`
    /// (fixture 882).
    ShlBxDispCl { disp: i8 },
    /// `sar word ptr [bx+disp8],cl` — `D3 7F dd`. Group-2 `/7`
    /// signed sibling.
    SarBxDispCl { disp: i8 },
    /// `shr word ptr [bx+disp8],cl` — `D3 6F dd`. Group-2 `/5`
    /// unsigned sibling.
    ShrBxDispCl { disp: i8 },
    /// `mov ax,word ptr [bx+disp8]` — `8B 47 dd`. MOV r16,r/m16
    /// with mod=01 reg=AX(000) r/m=111=BX. Used by `int *p;
    /// p[K] *= y` (fixture 883: load LHS through BX into AX).
    MovAxBxDisp { disp: i8 },
    /// `mov word ptr [bx+disp8],ax` — `89 47 dd`. MOV r/m16,r16
    /// store sibling. Used by the `imul`/`idiv` store-back step.
    MovBxDispAx { disp: i8 },
    /// `mov word ptr [bx+disp8],dx` — `89 57 dd`. MOV r/m16,r16
    /// with reg=DX(010) r/m=111=BX+disp8. Used by `int *p; p[K]
    /// %= y` (fixture 884: mod result is in DX after `idiv`).
    MovBxDispDx { disp: i8 },
    /// `mov dx,word ptr [bx+disp8]` — `8B 57 dd`. MOV r16,r/m16
    /// with reg=DX(010) r/m=111=BX+disp8. Used by `long *p; p[K]
    /// <<= N` — DX gets the low half before the inline shift
    /// (fixture 904).
    MovDxBxDisp { disp: i8 },
    /// `mov word ptr [bx+disp8],<imm16>` — `C7 47 dd lo hi`. MOV
    /// r/m16,imm16 (Group with /0) at mod=01 r/m=111=BX+disp8.
    /// Used by `long *p; p[K] = v` — two memory-direct stores at
    /// `[bx+off+2]` (high) and `[bx+off]` (low). Fixture 897.
    MovBxDispImm { disp: i8, imm: u16 },
    /// `adc word ptr [bx+disp8],<imm8sx>` — `83 57 dd ii`. Group-1
    /// `/2` (ADC) imm8sx form with mod=01 r/m=111=BX+disp8. Carry-
    /// propagation partner to `AddBxDispImm8` for long-pointer
    /// subscript compound add (fixture 901).
    AdcBxDispImm8 { disp: i8, imm: i8 },
    /// `sbb word ptr [bx+disp8],<imm8sx>` — `83 5F dd ii`. Group-1
    /// `/3` (SBB) sibling.
    SbbBxDispImm8 { disp: i8, imm: i8 },
    /// `push word ptr [bx+disp8]` — `FF 77 dd`. Group FF `/6` (PUSH
    /// r/m16) with mod=01 r/m=111=BX+disp8. Used by `f(p[K])` —
    /// BCC's memory-operand-push peephole on a global-pointer
    /// subscript arg (fixture 893: skips the `mov ax, [bx+K]; push
    /// ax` pair for the shorter direct push).
    PushBxDisp { disp: i8 },
    /// `add al,byte ptr [bp+<offset>]` — `02 46 dd`. ADD r8,r/m8
    /// with mod=01 reg=AL(000) r/m=110=BP+disp8. Char-array
    /// compound with non-const int RHS truncated to byte (fixture
    /// 847: `char a[3]; a[1] += y` where y is int).
    AddAlBpRel { offset: i16 },
    /// `sub al,byte ptr [bp+<offset>]` — `2A 46 dd`. Sibling.
    SubAlBpRel { offset: i16 },
    /// `and al,byte ptr [bp+<offset>]` — `22 46 dd`. Sibling.
    AndAlBpRel { offset: i16 },
    /// `or al,byte ptr [bp+<offset>]` — `0A 46 dd`. Sibling.
    OrAlBpRel { offset: i16 },
    /// `xor al,byte ptr [bp+<offset>]` — `32 46 dd`. Sibling.
    XorAlBpRel { offset: i16 },
    /// `shl word ptr [si],cl` — `D3 24`. Grp2 /4(SHL) r/m16 with
    /// mod=00 r/m=100=SI. Variable-count shift through SI for
    /// `*p <<= y`. Fixture 840.
    ShlSiPtrCl,
    /// `sar word ptr [si],cl` — `D3 3C`. Signed sibling.
    SarSiPtrCl,
    /// `shr word ptr [si],cl` — `D3 2C`. Unsigned sibling.
    ShrSiPtrCl,
    /// `adc word ptr [si+disp8],ax` — `11 44 dd`. ADC r/m16,r16
    /// form; ModR/M `44 dd` = mod=01 reg=AX(000) r/m=100=SI with
    /// disp8. High-half carry partner to `AddSiPtrDx` for the
    /// variable-RHS long pointer compound. Fixture 398.
    AdcSiDispAx { disp: i8 },
    /// `adc word ptr [si+disp8],dx` — `11 54 dd`. ADC r/m16,r16
    /// with reg=DX(010) r/m=100=SI. High-half carry partner for
    /// `long *p; *p += int x` (signed widening) where AX holds
    /// the int low half and DX the cwd sign-extension. Fixture
    /// 849.
    AdcSiDispDx { disp: i8 },
    /// `sbb word ptr [si+disp8],dx` — `19 54 dd`. Borrow sibling
    /// for `long *p -= int x`.
    SbbSiDispDx { disp: i8 },
    /// `sub word ptr [si],<imm8sx>` — `83 2C ii`. Low-half partner
    /// for long-pointer `*p -= K`.
    SubSiPtrImm8 { imm: i8 },
    /// `add byte ptr [si], <reg8>` — `00 (mod=00 reg=<r> r/m=100)`.
    /// Char-via-pointer arith compound with variable RHS already
    /// loaded into the byte register (fixture 713: `*p += d` with
    /// p in SI, d→AL → `mov al, byte ptr [bp-1]; add byte ptr
    /// [si], al` = `00 04`).
    AddSiPtrReg8 { src: Reg8 },
    /// `inc byte ptr [si]` — `FE 04` (Grp4 /0 r/m8, mod=00
    /// r/m=100). Memory-direct byte increment through SI. BCC
    /// uses this for postfix `(*p)++` discarded (fixture 714);
    /// prefix `++*p` and explicit `*p += 1` take the AL detour.
    IncSiPtrByte,
    /// `inc word ptr [si]` — `FF 04`. ModR/M 04 = mod=00 /0(INC)
    /// r/m=100([SI]). Int sibling of `IncSiPtrByte` (fixture 1290:
    /// `p->x++` with p in SI and x at offset 0).
    IncSiPtrWord,
    /// `dec word ptr [si]` — `FF 0C`. ModR/M 0C = mod=00 /1(DEC).
    DecSiPtrWord,
    /// `dec byte ptr [si]` — `FE 0C`. Sibling for `(*p)--`.
    DecSiPtrByte,
    /// `sub byte ptr [si], <reg8>` — `28 04` sibling.
    SubSiPtrReg8 { src: Reg8 },
    /// `and byte ptr [si], imm8` — `80 24 ii`. Grp1 r/m8,imm8 with
    /// /4=AND, mod=00 r/m=100. Char-via-pointer bitwise compound
    /// (fixture 712: `*p &= 15`). Char arith goes through AL, but
    /// bitwise stays memory-direct — same asymmetry as char-global.
    AndSiPtrByteImm8 { imm: u8 },
    /// `or byte ptr [si], imm8` — `80 0C ii`. Sibling for `|=`.
    OrSiPtrByteImm8 { imm: u8 },
    /// `xor byte ptr [si], imm8` — `80 34 ii`. Sibling for `^=`.
    XorSiPtrByteImm8 { imm: u8 },
    /// `and byte ptr [bp+<offset>], imm8` — `80 (mod=01 /4 r/m=110)
    /// dd ii` = `80 66 dd ii`. Grp1 r/m8 imm8 against a stack
    /// local. Char-local-array bitwise compound (fixture 720:
    /// `char a[4]; a[2] &= 15`).
    AndBpRelByteImm8 { offset: i16, imm: u8 },
    /// `or byte ptr [bp+<offset>], imm8` — `80 4E dd ii`. Sibling.
    OrBpRelByteImm8 { offset: i16, imm: u8 },
    /// `xor byte ptr [bp+<offset>], imm8` — `80 76 dd ii`. Sibling.
    XorBpRelByteImm8 { offset: i16, imm: u8 },
    /// `sbb word ptr [si+disp8],<imm8sx>` — `83 5C dd ii`. High-half
    /// borrow-propagation partner for long-pointer `*p -= K`.
    SbbSiDispImm8 { disp: i8, imm: i8 },
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
    /// `adc word ptr [bp+disp8],<imm8 sign-extended>` — 83 56 dd ii.
    /// Carry-propagation partner to `AddBpRelImm8` for the high half
    /// of a long-local compound add (fixture 288).
    AdcBpRelImm8 { offset: i16, imm: i8 },
    /// `sub word ptr [bp+disp8],<imm8 sign-extended>` — 83 6E dd ii.
    /// Compound `-=` low half on a long stack local.
    SubBpRelImm8 { offset: i16, imm: i8 },
    /// `sbb word ptr [bp+disp8],<imm8 sign-extended>` — 83 5E dd ii.
    /// Borrow-propagation partner to `SubBpRelImm8`.
    SbbBpRelImm8 { offset: i16, imm: i8 },
    /// `and word ptr [bp+disp8],<imm16>` — 81 66 dd lo hi.
    /// Compound `&=` on a long stack local; matches BCC's `81`
    /// (imm16) selection for bitwise compound even when the
    /// constant fits i8sx (fixture 289, mirrors fixture 253's
    /// global rule).
    AndBpRelImm16 { offset: i16, imm: u16 },
    /// `or word ptr [bp+disp8],<imm16>` — 81 4E dd lo hi.
    /// Compound `|=` partner to `AndBpRelImm16`.
    OrBpRelImm16 { offset: i16, imm: u16 },
    /// `xor word ptr [bp+disp8],<imm16>` — 81 76 dd lo hi.
    /// Compound `^=` partner to `AndBpRelImm16`.
    XorBpRelImm16 { offset: i16, imm: u16 },
    /// `mov ax,word ptr [si]` — 8B 04. Load through SI pointer.
    MovAxFromSiPtr,
    /// `mov word ptr [bx],<imm16>` — C7 07 lo hi. Store through BX
    /// (used by indexed array writes after `lea bx,base + scale*i`).
    MovBxPtrImm { imm: u16 },
    /// `mov byte ptr [bx],<imm8>` — C6 07 ii. Byte-store immediate
    /// through BX. Fixture 3559 (`buf[i] = 0` for char* — bx is
    /// the post-scaling indexed address).
    MovBxPtrImm8 { imm: u8 },
    /// `mov word ptr [di],<imm16>` — C7 05 lo hi.
    MovDiPtrImm { imm: u16 },
    /// `add word ptr [si],<imm16>` — 81 04 lo hi (Grp1 /0=ADD,
    /// mod=00 r/m=100). Memory-direct add of a wide immediate
    /// through SI pointer. Fixture 1492 (`*p += 1000`).
    AddSiPtrImm16 { imm: u16 },
    /// `xor word ptr [di],<reg16>` — `31 (mod=00 reg=<r> r/m=101)`.
    /// Memory-direct xor of a register into [di]. Fixture 3638
    /// (xor-swap idiom — `*p ^= *q` shape).
    XorDiPtrReg16 { reg: Reg16 },
    /// `mov byte ptr [bx],al` — 88 07. ModR/M 07 = mod=00 reg=000(AL)
    /// r/m=111(BX). Char-element store after variable-index BX
    /// computation. Fixture 1219.
    MovBxPtrAl,
    /// `mov word ptr [bx],ax` — 89 07. Same ModR/M as the byte form
    /// but the word opcode. Int-element store sibling. Fixture 1468.
    MovBxPtrAx,
    /// `mov word ptr [bx],<reg16>` — generic register store through
    /// BX. `89 (mod=00 reg=<r> r/m=111)`. AX uses the same opcode
    /// but kept as `MovBxPtrAx` for legacy callers; this variant
    /// covers SI/DI/CX/DX. Fixture 2244 (`arr[i] = i` with i in SI).
    MovBxPtrReg16 { reg: Reg16 },
    /// `mov ax,word ptr [bx]` — 8B 07. Load through BX.
    MovAxFromBxPtr,
    /// `mov bx,word ptr [bx]` — 8B 1F. Chain step in `**p` style
    /// double-indirect loads (fixture 195): keeps the running
    /// pointer in BX while peeling one level of indirection.
    MovBxFromBxPtr,
    /// `shl <reg16>,1` — D1 (mod=11 /4 r/m=reg). The 1-bit shift form
    /// (BCC uses this to compute `i*2` for word-array indexing).
    ShlReg16One { reg: Reg16 },
    /// `shl <reg8>,1` — D0 (mod=11 /4=SHL r/m=reg-code). 8-bit
    /// sibling of `ShlReg16One`. Fixture 535 (`char c <<= 2`
    /// unrolls to two `shl dl, 1`).
    ShlReg8One { reg: Reg8 },
    /// `sar <reg8>,1` — D0 (mod=11 /7=SAR r/m=reg-code).
    SarReg8One { reg: Reg8 },
    /// `shr <reg8>,1` — D0 (mod=11 /5=SHR r/m=reg-code).
    ShrReg8One { reg: Reg8 },
    /// `rcl <reg16>,1` — D1 (mod=11 /2 r/m=reg). Rotate-left through
    /// carry; used as the high-half partner to `shl` for long left
    /// shift by 1 (fixture 227).
    RclReg16One { reg: Reg16 },
    /// `sar <reg16>,1` — D1 (mod=11 /7 r/m=reg). Arithmetic shift
    /// right (sign-fill); high-half operation for signed long right
    /// shift by 1 (fixture 229).
    SarReg16One { reg: Reg16 },
    /// `shr <reg16>,1` — D1 (mod=11 /5 r/m=reg). Logical shift
    /// right (zero-fill); high-half operation for unsigned long
    /// right shift by 1 (fixture 243).
    ShrReg16One { reg: Reg16 },
    /// `rcr <reg16>,1` — D1 (mod=11 /3 r/m=reg). Rotate-right through
    /// carry; low-half partner for `sar` in long right shift by 1
    /// (fixture 229).
    RcrReg16One { reg: Reg16 },
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
    /// `mov word ptr [bp+disp8], <reg16>` — `89 (mod=01 reg rm=110) dd`.
    /// Generic 16-bit-reg store to a bp-relative stack slot.
    /// Companion to `MovBpRelAx` for non-AX sources (fixture 286
    /// stores the low half via DX).
    MovBpRelReg16 { offset: i16, reg: Reg16 },
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
    // 8087 FPU instructions. The opcode family selects the operand
    // width (`D9` for 32-bit `dword`/single, `DD` for 64-bit `qword`/
    // double); the ModR/M reg field selects the operation (0 = `fld`,
    // 3 = `fstp`). Memory addressing reuses the same `[bp+disp]`
    // and `<group>:<sym>` shapes as the integer instructions.
    /// `fld dword ptr [bp+<offset>]` — push a 32-bit float from a
    /// bp-relative slot onto the FPU stack. Encoding: `D9 /0 [bp+disp]`
    /// — `D9 46 dd` (disp8) or `D9 86 lo hi` (disp16).
    FldDwordBpRel { offset: i16 },
    /// `fstp dword ptr [bp+<offset>]` — pop the FPU top into a 32-bit
    /// float slot. Encoding: `D9 /3 [bp+disp]`.
    FstpDwordBpRel { offset: i16 },
    /// `fld qword ptr [bp+<offset>]` — 64-bit double load. Encoding:
    /// `DD /0 [bp+disp]`.
    FldQwordBpRel { offset: i16 },
    /// `fstp qword ptr [bp+<offset>]` — 64-bit double store. Encoding:
    /// `DD /3 [bp+disp]`.
    FstpQwordBpRel { offset: i16 },
    /// `fld dword ptr <group>:<sym>[+<offset>]` — push a 32-bit float
    /// from a data-segment address. Encoding: `D9 06 lo hi` plus a
    /// SegRelGroupTarget FIXUPP on the disp16. Used by float
    /// constants pooled in `s@` and by global float reads.
    FldDwordGroupSym { group: String, symbol: String, offset: i16 },
    /// `fld qword ptr <group>:<sym>[+<offset>]` — 64-bit double load
    /// from a data-segment address. Encoding: `DD 06 lo hi` + same
    /// FIXUPP shape as `FldDwordGroupSym`. Used by global doubles.
    FldQwordGroupSym { group: String, symbol: String, offset: i16 },
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

/// 8086 segment registers. The byte encoding goes in ModR/M's reg
/// field for `MOV r/m, sreg` (opcode `8C`): ES=0, CS=1, SS=2, DS=3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegReg {
    Es,
    Cs,
    Ss,
    Ds,
}

impl SegReg {
    pub fn code(self) -> u8 {
        match self {
            Self::Es => 0,
            Self::Cs => 1,
            Self::Ss => 2,
            Self::Ds => 3,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "es" => Some(Self::Es),
            "cs" => Some(Self::Cs),
            "ss" => Some(Self::Ss),
            "ds" => Some(Self::Ds),
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
    /// Segment-relative 16-bit offset (M=1, location=1), frame method
    /// F4 (frame is the target's own segment, no frame datum), target
    /// method T6 (EXTDEF, no displacement). Real TASM emits one of
    /// these for every 8087 memory instruction, targeting `FIDRQQ`
    /// (the floating-point library's marker symbol). The linker uses
    /// these fixups to rewrite the site if emulation is enabled;
    /// with the hardware FPU present they're a no-op marker. Fix
    /// Data byte: 0x46.
    SegRelExternFrameTarget { extdef_idx: u8 },
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

