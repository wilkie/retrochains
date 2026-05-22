# Bitfields

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## sizeof types pinned (char=1, int=2, long=4, double=8); sizeof doesn't eval; signed bitfield = shl+sar; zero-width forces align; cross-byte uses word ops

Fixtures `2297`-`2302` cover sizeof and bitfield
mechanics.

- `2297` (**sizeof types**): all folded at parse
  time. Returns `mov ax, 15` (= 1+2+4+8). BCC
  type sizes:
  - char: 1
  - short / int / near ptr: 2
  - long / float / far ptr / huge ptr: 4
  - double: 8
- `2298` (**sizeof doesn't evaluate**): `sizeof
  (++i)` does NOT emit the increment. `i` stays
  5. Per C standard — sizeof's argument is an
  unevaluated context.
- `2299` (**sizeof arr vs ptr arg**):
  - `sizeof(arr)` in declaring scope = actual
    array size (20 for `int[10]`)
  - `sizeof(p)` where p is a parameter declared as
    `int p[10]` = 2 (decays to near pointer)
  Returns 20 + 2 = 22.
- `2300` (**signed vs unsigned bitfield**):
  ```
  ; struct S { signed int s : 4; unsigned int u : 4; };
  ; x.s = -1; x.u = 15; return x.s + (int)x.u;
  
  ; Read x.u (unsigned, low byte high nibble):
  mov al, [byte]
  mov cl, 4
  shr ax, cl                 ; logical right shift
  and ax, 0x0F               ; mask to 4 bits
  
  ; Read x.s (signed, low byte low nibble):
  mov al, [byte]
  mov cl, 12
  shl ax, cl                 ; shift left to align sign bit with byte's bit 15
  mov cl, 12
  sar ax, cl                 ; arithmetic right shift (sign-extends)
  ```
  Signed bitfield extraction uses shl + sar
  pattern for sign extension. Unsigned uses shr +
  and.
- `2301` (**cross-byte bitfield via word ops**):
  ```
  ; struct X { unsigned int lo : 6; unsigned int hi : 6; };
  ; hi spans bits 6-11 (across byte boundary)
  
  ; Read lo (within byte 0): use byte op
  mov al, [byte0]
  and ax, 0x3F
  
  ; Read hi (cross-byte): use WORD op + shift + mask
  mov dx, word [bp-N]        ; load both bytes
  mov cl, 6
  shr dx, cl                  ; shift to position
  and dx, 0x3F                ; mask
  ```
- `2302` (**zero-width bitfield = alignment**):
  ```
  ; struct Z { unsigned int a : 3; unsigned int : 0; unsigned int b : 3; };
  ; The :0 forces next field to the next byte
  
  ; Write a to bits 0-2 of byte 0:
  and byte [bp-2], 0xF8 / or byte [bp-2], 0x05
  
  ; Write b to bits 0-2 of byte 1 (NEW byte due to :0):
  and byte [bp-1], 0xF8 / or byte [bp-1], 0x06
  
  ; sizeof(struct Z) = 2
  ```

**Bitfield extraction patterns**:
| Type | Position | Method |
|------|----------|--------|
| Unsigned in byte | bits 0-N | `mov al, [m] / and ax, mask` |
| Unsigned in byte | bits N-M | `mov al, [m] / shr ax, N / and ax, mask` |
| Signed in byte | bits 0-N | `mov al, [m] / shl ax, (16-N-1) / sar ax, (16-N-1)` |
| Unsigned cross-byte | spans bytes | `mov dx, [m] (word) / shr dx, N / and dx, mask` |
| Signed cross-byte | spans bytes | same + shl/sar for sign |

**Bitfield write pattern**:
1. Load byte (or word for cross-byte)
2. AND with NOT(mask << position) — clear field
3. AND value with mask, shift to position
4. OR into the loaded byte/word
5. Store back

For within-byte writes, BCC uses byte ops (`80
/N r/m8, imm8`). For cross-byte, word ops (`81
/N r/m16, imm16`).

**Bitfield rules (BCC 2.0)**:
- LSB-first packing
- Don't naturally cross storage-unit boundaries
  unless declared so (or sized to span)
- Zero-width unnamed bitfield (`: 0`) forces
  alignment to next storage unit
- Signed bitfields sign-extend on read (shl + sar)
- Unsigned bitfields zero-extend (shr + and)

For the Rust reimplementation:
- Bitfield layout: track byte/bit position per
  field, considering :0 alignment forcers.
- Extraction: emit byte or word ops based on
  field span.
- Sign-extend signed fields with shl/sar pair.

## Cross-byte bitfield = word op; signed bitfield read = `shl/sar` sign-extend; 1-bit `=1` skips clear

Fixtures `2105` (1-bit flag bitfields), `2106`
(cross-byte bitfield), `2107` (signed bitfield)
deepen the bitfield characterisation.

- `2105` (**1-bit flag, multi-byte spanning field**):
  ```c
  struct Flags { unsigned f1:1; unsigned f2:1; unsigned val:14; };
  ```
  - `fl.f1 = 1`: just `or byte [bp+disp], 1` (no
    clear-first — assumes bit was 0; works only
    if storage was zero-initialised, **buggy
    for auto locals**).
  - `fl.f2 = 0`: `and byte [bp+disp], 0xfd`
    (clear bit).
  - `fl.val = 1000`: val spans bits 2..15, so
    needs WORD ops: `and word [m], 0x0003` (clear
    val bits) + `or word [m], 0x0fa0` (set val
    bits = 1000 << 2).
- `2106` (**cross-byte spanning bitfield**):
  ```c
  struct Cross { unsigned lo:6; unsigned mid:6; unsigned hi:4; };
  ```
  Storage: lo at byte 0 bits 0..5, mid spans
  byte 0 bits 6..7 + byte 1 bits 0..3, hi at
  byte 1 bits 4..7.
  
  For mid (crosses byte boundary): word op
  required:
  ```
  and word [bp+disp], 0xf03f      ; clear mid bits
  or  word [bp+disp], 0x0a00       ; set mid = 40 << 6
  ```
- `2107` (**signed bitfield read = shl/sar**):
  for `int x : 4`, read uses sign-extension via
  shift-left-then-arithmetic-right:
  ```
  mov al, [bp+disp]
  mov cl, 12 / shl ax, cl          ; align field in bits 12..15
  mov cl, 12 / sar ax, cl          ; arithmetic right = sign-extend
  ```
  10 bytes per signed-bitfield read. Beautiful
  8086 trick — putting the field in the high
  bits and arithmetic-shifting back fills the
  high bits with the sign.

**Bitfield encoding summary**:
| Operation | Single-byte field | Cross-byte field |
|-----------|-------------------|-------------------|
| Write `field = K` (unsigned, K fits) | `and byte [m], mask / or byte [m], K<<shift` (8 bytes) | `and word [m], mask / or word [m], K<<shift` (10 bytes) |
| Write `field = 1` (1-bit) | `or byte [m], (1<<shift)` (4 bytes — clear skipped) | (same word) |
| Read unsigned | `mov al, [m] / shr / and width-mask` | `mov ax, [m] / shr / and width-mask` |
| Read signed | `mov al, [m] / shl (16-w-pos) / sar (16-w)` | same with word load |

So **cross-byte bitfields force word ops**; signed
bitfields use the sign-extending shift trick.

For the Rust reimplementation:
- Detect when bitfield spans byte boundary →
  emit word-sized and/or.
- For `unsigned 1-bit = 1`: skip the clear (BCC's
  optimization — but be aware of correctness
  caveats for uninitialised auto vars).
- Signed bitfield read: shl to align in high
  bits, sar back to extract.

## Nested struct flat-laid; union shares storage (little-endian observable); bitfields packed LSB-first

Fixtures `2102` (nested struct init), `2103`
(union), `2104` (bitfields) cover three composite-
type patterns.

- `2102` (**nested struct flat-laid**):
  ```c
  struct Outer { int id; struct Inner inner; int tail; };
  static struct Outer o = {1, {10, 20}, 100};
  ```
  Data emits 8 bytes flat: `01 00 0a 00 14 00 64
  00`. Brace nesting in init parses but
  doesn't change layout — same as `{1, 10, 20,
  100}` with flat layout. Inner fields accessed
  as direct offsets from Outer's base.
- `2103` (**union shares storage, little-endian**):
  `u.as_int = 0x4142` writes a word; reading via
  `u.as_bytes[0]` returns 0x42 (low byte), `u.
  as_bytes[1]` returns 0x41 (high byte). **Little-
  endian observable**. Union size = max(member
  sizes).
- `2104` (**bitfields packed LSB-first**):
  ```c
  struct Bits { unsigned a:3; unsigned b:5; unsigned c:8; };
  ```
  Storage: 2 bytes total. Layout:
  - byte 0 bits 0..2 = a (LSB-first)
  - byte 0 bits 3..7 = b
  - byte 1 = c
  
  Write pattern (e.g., `bf.a = 5`):
  ```
  and byte [bp+disp], 0xf8        ; clear field bits
  or  byte [bp+disp], 5            ; set new value
  ```
  Read pattern (e.g., `bf.b`):
  ```
  mov al, [bp+disp]
  shr ax, 3 (× 3, unrolled)        ; shift field to LSB
  and ax, 0x1f                      ; mask field width (5 bits = 0x1f)
  ```

**Composite-init/access summary**:
| Construct | Layout/Storage | Codegen pattern |
|-----------|----------------|------------------|
| Nested struct | Flat (no padding inserted) | Direct offsets |
| Union | Shared, size = max(members) | Same offset for all members |
| Bitfields | Packed LSB-first in storage units | and+or for write; shr+and for read |

For the Rust reimplementation:
- Nested struct: emit as flat byte sequence
  without padding.
- Union: track member offsets all as 0; storage
  size = max.
- Bitfields: pack LSB-first; emit and/or for
  writes, shr+and for reads.

## Bitfield byte vs word granularity; enum = int; typedef = parser-level alias

Fixtures `1880` (bitfield spans byte boundary),
`1881` (enum semantics), and `1882` (typedef
chain) cover three type-system shapes.

- `1880` (**bitfield granularity follows field
  bounds**): for `struct {hi:6; span:6; lo:4}`,
  BCC picks read/write granularity per-field:
  - `hi:6` (bits 0-5 of byte 0, fits in 1 byte):
    **byte ops** — `80 66 disp` AND, `80 4e disp`
    OR
  - `span:6` (bits 6-7 byte 0 + bits 0-3 byte 1,
    crosses boundary): **word ops** — `81 66
    disp` AND-word, `81 4e disp` OR-word
  - `lo:4` (bits 4-7 of byte 1, fits in 1 byte):
    byte ops on [bp-1]
  
  So **fields that fit in a byte use byte ops;
  fields that cross use word ops**. Reads follow
  the same rule (mov al vs mov ax).
- `1881` (**enum is int**): enum constants resolve
  at parse time to integer values. `enum Color c
  = GREEN;` compiles to `c = 2` (literal int).
  `c + RED` compiles to `c + 1`. No type
  distinction at codegen.
- `1882` (**typedef is parser alias**): `typedef
  int my_int; typedef my_int alias_int;` is
  purely lexical. Identical codegen to direct
  `int` declarations. Type aliases are resolved
  at parse time; only the underlying type reaches
  codegen.

So the C type system has two layers:
- **Compile-time only** (parser/checker): enum,
  typedef, const qualifier, `(unsigned)` int cast
- **Runtime-affecting**: signedness (changes
  sar/shr/idiv/div choice), pointer types
  (changes far/near, segment fixups), struct
  layout (offsets), bitfield positions

For the Rust reimplementation:
- enum constants → integer values at parse time
- typedef → resolve to base type at parse time
- bitfield granularity: choose byte vs word ops
  based on whether the field crosses a byte
  boundary

## >4B struct return uses hidden ptr + N_SCOPY@ (×2); shift+or = or-with-mem; bitfields work

Fixtures `1877` (6B struct return), `1878` (byte
packing `(hi<<8)|lo`), and `1879` (bitfields in
BCC 2.0) cover remaining struct/bit shapes.

- `1877` (**>4B struct return = hidden ptr +
  N_SCOPY@ × 2**):
  Caller side:
  ```
  ; allocates t (6B) AND temp (6B) — 12 bytes total
  ; push hidden FAR ptr to TEMP (4B)
  call _make_t
  pop / pop                ; cleanup hidden ptr
  ; push FAR ptr to TEMP (source), FAR ptr to t (dest)
  mov cx, 6
  call N_SCOPY@            ; copy temp → t
  ```
  Callee side (`make_t`):
  ```
  ; fill local r
  ; push hidden dest ptr as source-for-copy
  ; push FAR ptr to local r as source
  mov cx, 6
  call N_SCOPY@            ; copy local r → hidden dest
  mov ax, [bp+4]           ; return dest offset in AX
  ```
  So **TWO copies** happen: local r → caller's
  temp (via N_SCOPY@ in callee), then temp →
  caller's named destination (via N_SCOPY@ in
  caller). This wasteful double-copy is a result
  of how BCC chains the temp-buffer protocol.
  
  N_SCOPY@ likely uses pascal/stdcall (callee
  cleans args) since no cleanup visible after the
  in-caller N_SCOPY@ call.
- `1878` (**`(hi << 8) | lo` = shift then or-with-
  mem**):
  ```
  mov ax, [hi]
  mov cl, 8 / shl ax, cl       ; shift
  or ax, [lo]                  ; or-with-memory (0b /r m16)
  ```
  The OR uses `0b 46 fc` (or r16, [bp+disp]) — 3
  bytes. No fusion of shift+or into a single
  operation, but each step uses the optimal
  encoding.
- `1879` (**bitfields WORK in BCC 2.0**): a
  `struct {unsigned a:4; unsigned b:4; unsigned
  c:8;}` is packed into a word at byte granularity:
  - `b.a`: low nibble of byte 0
  - `b.b`: high nibble of byte 0
  - `b.c`: byte 1
  
  Write codegen: clear-mask (AND) + set-mask (OR)
  via **byte ops**:
  ```
  and byte [bp+disp], ~mask    ; 80 66 disp mask  — clear bits
  or  byte [bp+disp], val      ; 80 4e disp val   — set bits
  ```
  Read codegen: load byte → AND with mask →
  shift down if not at LSB → AND again for safety.
  
  Notable: bitfields prefer **byte granularity** when
  the field doesn't cross a byte boundary. `b.c`
  (full byte) uses [bp-1] directly.

For the Rust reimplementation:
- For >4B struct returns: caller allocates temp,
  pushes hidden ptr, then N_SCOPY@'s the temp to
  the named dest. Callee uses N_SCOPY@ to copy
  its local result into the hidden dest.
- `(x << K) | y` lowers to shl then or-with-mem
  (no fusion).
- Bitfields: byte-granular when possible; clear-
  then-set via byte AND/OR with masks.

## Cross-byte bitfield uses word ops; typedef = no-op; volatile = stack + no CSE

Fixtures `1694` (cross-byte bitfield), `1695`
(typedef int alias), and `1696` (volatile int)
finalise the per-byte-vs-word bitfield rule and
confirm two semantic-only qualifiers.

- `1694` (**cross-byte bitfield**): when a bitfield
  spans a byte boundary, BCC emits **word `81 /N`
  operations** with a 2-byte mask. Layout for
  `a:6, b:6, c:4` (= 16 bits):
  - `a` (bits 0-5, in byte 0): byte ops `80 /N`,
    mask `0xc0` for clear.
  - `b` (bits 6-11, spans bytes 0-1): **word ops**
    `81 /N`, mask `0xf03f` for clear, set value
    `0x0080` (2 << 6).
  - `c` (bits 12-15, in byte 1): byte ops `80 /N`,
    mask `0x0f` for clear (high 4 bits of byte 1).
  - Read of cross-byte `b` uses word load + 6-bit
    shift + 6-bit mask.
  So the rule is **per-field, not per-struct**:
  fields fitting in one byte use byte ops; fields
  crossing use word ops. The clear mask is the
  bitwise-NOT of the field's bit-pattern within
  the storage unit.
- `1695` (**`typedef int`**): produces **byte-
  identical** code to using the underlying type
  directly. `typedef int u16; u16 add(u16, u16)`
  is fully equivalent to `int add(int, int)`.
  `typedef` is a **parse-time alias** with zero
  codegen impact — same OBJ bytes as the
  cdecl-explicit fixture (1656).
- `1696` (**`volatile int`**): forces three things:
  1. Variable lives in **memory** (not enregistered)
     — the use-count heuristic is overridden.
  2. **Every read re-fetches** from memory — no
     CSE, even immediately after a write. The
     `mov ax, [bp-2]` before return executes even
     though we just wrote `[bp-2]` two instructions
     earlier.
  3. **Every write commits** to memory immediately.
  So `volatile` disables enregistration and CSE for
  this variable. Compare to plain `int n = 5; n =
  n + 1; return n;` — n would normally enregister
  into SI/DI with no memory traffic.

For the Rust reimplementation:
- Bitfield codegen: pick byte vs word ops based on
  whether `(bit_offset + bit_width) <= 8` for
  byte-fits, otherwise word ops.
- `typedef`: handle at parser level only, never
  reaches codegen.
- `volatile`: tag the symbol; force memory
  storage; emit load on every read, store on every
  write; bypass register allocation.

## Bitfields: byte-level and/or mask+shift; union shares storage; enum const-folded

Fixtures `1691` (bitfield struct), `1692` (union),
and `1693` (enum constants) characterise three
remaining C semantic shapes.

- `1691` (**bitfield codegen**): a `struct B { a:4;
  b:4; c:8; }` totals 16 bits = 2 bytes. BCC uses
  **byte-level operations** for fields that fit
  within a single byte:
  - **Write `a` (bits 0-3 of byte 0)**: `and byte
    [m], 0xf0` (clear) + `or byte [m], value`
    (set). Uses 4-byte `80 /N` form.
  - **Write `b` (bits 4-7 of byte 0)**: `and byte
    [m], 0x0f` (clear) + `or byte [m], (value <<
    4)` (set). Shifted value at parse time.
  - **Write `c` (whole byte 1)**: `and byte [m+1],
    0x00` (clear) + `or byte [m+1], value` (set).
  - **Read `a`**: `mov al, [m] / and ax, 0x000f`
    (mask).
  - **Read `b`**: `mov dl, [m] / mov cl, 4 / shr
    dx, cl / and dx, 0x000f` (shift+mask).
  - **Read `c`**: `mov dl, [m+1] / and dx, 0x00ff`
    (mask only — no shift needed).

  Cross-byte bitfields would presumably use word
  `81 /N` operations — not yet probed.
- `1692` (**union**): a `union U { int i; char
  c[2]; }` allocates `max(sizeof(members))` = 2
  bytes. Both fields share the same storage —
  `u.i = 0x1234` writes the word; `u.c[0]`
  reads the low byte (= 0x34 since 8086 is little-
  endian). No special codegen — each field is just
  a typed view at offset 0.
- `1693` (**enum constants**): enum values are
  **constant-folded at parse time**. `x + HIGH -
  LOW` where HIGH=20, LOW=5 lowers to `add ax,
  15` directly — no enum symbol in the OBJ, no
  runtime fetch, no separate constant pool. Enums
  are essentially typed `#define` substitutions
  with full constant arithmetic evaluation.

Bitfield encoding policy is a key finding for
Rust reimplementation:
- Track per-field byte offset + bit offset + width.
- Generate `and`/`or` byte ops when field fits in
  one byte; word ops otherwise.
- Shift only when bit-offset within the byte is
  non-zero.
- Mask only when reading (writes use clear+set
  semantics that don't need a read first).


## Bitfield write+read — `and/or` byte-mask pair, read via `shr; and`

Fixture `2529-bitfield-pack-obj`:

```c
struct Flags {
  unsigned a : 3;
  unsigned b : 5;
};
struct Flags f;
int main(void) {
  f.a = 5;
  f.b = 17;
  return f.b;
}
```

```
55 8b ec                            prologue
                                    ; --- f.a = 5 (bits 0..2) ---
80 26 00 00 f8                      and byte [_f+0], 0xF8    ; mask = 11111000
80 0e 00 00 05                      or  byte [_f+0], 0x05
                                    ; --- f.b = 17 (bits 3..7) ---
80 26 00 00 07                      and byte [_f+0], 0x07    ; mask = 00000111
80 0e 00 00 88                      or  byte [_f+0], 0x88    ; 17 << 3 = 0x88
                                    ; --- return f.b ---
a0 00 00                            mov al, [_f]             ; FIXUPP
d1 e8                               shr ax, 1                ; position-shift
d1 e8                               shr ax, 1
d1 e8                               shr ax, 1
25 1f 00                            and ax, 0x001F           ; width-mask
eb 00 5d c3                         epilogue
```

Findings:
- **Bitfield write** = clear-then-set:
  `and byte [mem], ~bitfield-mask; or byte [mem], shifted-value`.
  Each store is 5 bytes (`80 26/0e disp16 imm8` with FIXUPP). The
  imm8 is a byte-mask because both fields fit in one byte.
- **Bitfield read** = byte-load (`a0` moffs8, 3 bytes) + position-
  shift (3× `d1 e8` for a shift of 3 — unroll under N≤3 rule) +
  width-mask (`and ax, 0x001F` = 3 bytes).
- The width-mask is wider than necessary (mask AX, not AL) — but
  it's correct because AH might contain garbage after the byte
  load; the `and ax, 0x1F` cleans both halves. Cleaner than doing
  `mov al, [mem]; xor ah, ah; ...` (would add 2B for the clear).
- The struct `{ unsigned a:3; unsigned b:5; }` fits in **1 byte**
  total — BCC packs bitfields into the smallest container that
  holds them. (To probe: what happens when the next field crosses
  a byte boundary?)
- All four stores reference [_f + 0] with FIXUPP — bitfield storage
  starts at offset 0 in the struct.
- Note: bitfield writes are not load-modify-store — they use
  `and byte [mem], imm; or byte [mem], imm` (direct memory ops).
  This is shorter than load-into-reg, modify, store-back.


## Bitfield crossing byte boundary — switches to word-sized and/or

Fixture `2533-bitfield-cross-byte-obj`:

```c
struct Wide {
  unsigned a : 5;
  unsigned b : 5;
  unsigned c : 5;
};
struct Wide w;
int main(void) {
  w.b = 17;
  return w.b;
}
```

```
55 8b ec                            prologue
                                    ; --- w.b = 17 (bits 5..9, crosses byte boundary) ---
81 26 00 00 1f fc                   and word [_w+0], 0xFC1F   ; 16-bit mask
81 0e 00 00 20 02                   or  word [_w+0], 0x0220   ; 17 << 5 = 0x220
                                    ; --- return w.b ---
a1 00 00                            mov ax, [_w+0]            ; word load
b1 05                               mov cl, 5
d3 e8                               shr ax, cl                ; position-shift via cl
25 1f 00                            and ax, 0x001F            ; width-mask
eb 00 5d c3                         epilogue
```

Findings:
- When a bitfield CROSSES a byte boundary, BCC switches to:
  - **`and word [mem], imm16` / `or word [mem], imm16`** (6 bytes each).
  - **Word load** (`a1` moffs16, 3 bytes) for the read.
- Compare to single-byte case (`2529`): byte versions (`80 26/0e`,
  `a0`) — 1 byte shorter per instruction.
- BCC picks the SMALLEST container that holds all the struct's
  bitfields:
  - `5+5+5 = 15` bits → 2-byte word container, `sizeof(struct) = 2`.
  - `3+5 = 8` bits → 1-byte container, `sizeof(struct) = 1`.
- **Bit packing is LSB-first** (little-endian by bit position): field a
  is bits 0..4, field b is bits 5..9, field c is bits 10..14.
  Bit 15 unused (the 15th bit of the container).
- Position shift uses cl-form (shift count 5 ≥ 4 → cl).

