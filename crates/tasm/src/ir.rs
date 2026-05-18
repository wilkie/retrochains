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
    /// `imul word ptr [bp+<offset>]` — F7 6E dd. Single-operand signed
    /// multiply: AX = AX * src; high half goes to DX (discarded for
    /// `int * int` returning `int`).
    ImulBpRel { offset: i16 },
    /// `idiv word ptr [bp+<offset>]` — F7 7E dd. Single-operand
    /// signed divide of DX:AX by src; quotient in AX, remainder in
    /// DX. Always preceded by `cwd` to sign-extend AX into DX.
    IdivBpRel { offset: i16 },
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
    /// `inc <reg8>` — FE C0+rc. Increment an 8-bit register.
    IncReg8 { reg: Reg8 },
    /// `dec <reg8>` — FE C8+rc. Decrement an 8-bit register.
    DecReg8 { reg: Reg8 },
    /// `cmp <reg8>,<imm8>` — 80 F8+rc ii. Compare an 8-bit register
    /// to a constant.
    CmpReg8Imm8 { reg: Reg8, imm: u8 },
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
    /// `mov al,byte ptr [si]` — 8A 04. 8-bit load through SI pointer.
    MovAlFromSiPtr,
    /// `mov al,byte ptr [bx]` — 8A 07. 8-bit load through BX pointer.
    /// Fixture 192 dereferences a global char pointer via BX.
    MovAlFromBxPtr,
    /// `imul <reg16>` — F7 (mod=11 /5 r/m=reg). Single-operand signed
    /// multiply with a register operand. Used when the operand is
    /// register-resident, e.g. `x *= 3` after BCC enregisters x.
    ImulReg16 { reg: Reg16 },
    /// `idiv <reg16>` — F7 (mod=11 /7 r/m=reg). Single-operand signed
    /// divide with a register operand. Used for `int reg-local /= K`
    /// (fixture 584) where BCC loads the divisor into BX.
    IdivReg16 { reg: Reg16 },
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
    /// `cmp byte ptr [si], imm8` — `80 3C ii` (3 bytes). Grp1 /7=CMP
    /// with ModR/M `3C` = mod=00 r/m=100 ([si]). Used by `while
    /// (*p)` on a char pointer enregistered in SI (fixture 636).
    CmpByteSiPtrImm8 { imm: u8 },
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
    /// `adc word ptr [si+disp8],ax` — `11 44 dd`. ADC r/m16,r16
    /// form; ModR/M `44 dd` = mod=01 reg=AX(000) r/m=100=SI with
    /// disp8. High-half carry partner to `AddSiPtrDx` for the
    /// variable-RHS long pointer compound. Fixture 398.
    AdcSiDispAx { disp: i8 },
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

