# Glossary

Definitions for terms used across the specs and code. Organized by
topic, not alphabetically — adjacent terms tend to make sense together.

When a definition refers to another glossary entry, that entry is
**bolded**. Cross-references to specs use the form
`specs/<file>.md`.

---

## Project-specific terms

### Oracle
The original DOS-era toolchain we're reverse-engineering — `BCC.EXE`,
`TASM.EXE`, `TLINK.EXE` shipped in `BC2.zip` at the repo root, lazily
unpacked into `.bc2/` and run under DOSBox by `crates/oracle/`. The
DOSBox spawn is wrapped in `faketime` and input-file mtimes are
pinned, for byte-exact determinism. The oracle is the ground truth:
whatever it produces is by definition correct, and our Rust
reimplementation is graded against its bytes. See
`specs/RUNNING_BCC.md` for the invocation mechanics.

### Fixture
A directory under `fixtures/<NNN>-<name>/` holding a small input
program (e.g. `HELLO.C`), an `invocation.toml` describing how to
invoke a tool, and an `expected/` subdir with the **oracle**'s captured
output. The corpus is roughly ordered by complexity — the first
fixture in a new feature pins the simplest case, later ones add
edge cases. See `specs/FIXTURES.md`.

### Capture / verify
The two modes of the `xfix` harness. **Capture** runs the oracle
and writes its output into `expected/`. **Verify** re-runs either
the oracle (determinism check) or our Rust reimplementation
(`--toolchain ours`) and diffs the result against `expected/`. A
**byte-exact** match is the bar.

### Byte-exact
A goal: our reimplementation produces output that is identical to
the oracle's *byte for byte*, including whitespace, line endings,
ordering, and label numbering. This is much stronger than
semantically equivalent — it lets us be confident we've understood
all of BCC's choices, and it's the foundation for the eventual
**fingerprinter** (you can only recognize a compiler if you know
exactly what it emits).

### Format-aware diff
A diff that understands the structure of a particular file format
(`.asm`, OMF, MZ EXE) and can report differences in a more useful
way than a raw byte diff — "different label number at line 53"
beats "byte 412 differs". Built per-format as needed.

### Fingerprint / fingerprinter
A **fingerprint** is a pattern in compiled output that suggests a
specific compiler produced it. The **fingerprinter** is a (future)
tool that, given an unknown binary, accumulates fingerprint
evidence and reports which compiler most likely built it.
Fingerprints are rated **DEFINITIVE** (one sighting is conclusive),
**STRONG** (highly indicative but might be shared with related
toolchains), or **WEAK** (typical of the era, useful only in
aggregate). See `specs/FINGERPRINTS.md`.

### Decompiler / recompilable decompilation
A future tool that goes the other way — given a binary, produces
C source that, when re-fed to BCC, yields a byte-exact match
("**recompilable decompilation**" or "binary-to-source resynthesis").
The fingerprinter establishes "this was built with BCC", and the
decompiler then exploits BCC's known choices to reverse them.

### Slice
A self-contained increment of work: pick a C feature, capture
fixtures for its observable shapes, implement enough of the
compiler to make those fixtures pass byte-exactly, document the
findings. Slices build on each other (chars → unary → ++/-- →
compound assignment → switch → …).

---

## Compiler / codegen terms

### Peephole (peephole optimization)
A small, local code-generation improvement — the compiler looks
at a tiny window of instructions ("through a peephole") and
substitutes a shorter or faster sequence when it recognizes a
specific pattern. Examples in BCC:
- `x = x + 1` → `inc ax` instead of `add ax, 1` (1 byte vs. 3)
- `x = x + 2` → `inc ax / inc ax` instead of `add ax, 2` (2 vs. 3)
- `cmp <reg>, 0` → `or <reg>, <reg>` (2 vs. 3)
- `x = 0` → `xor ax, ax` instead of `mov ax, 0` (2 vs. 3)

Each peephole is also a **fingerprint** — knowing BCC chose the
shorter form and another compiler didn't tells you something.

### Codegen
"Code generation" — the pass that turns an AST (or IR) into
target asm. Our codegen lives in `crates/bcc/src/codegen/`.

### Single-pass codegen
A codegen design that emits asm directly while walking the AST,
without first building any intermediate representation. BCC's
output strongly suggests this style (source-line comments
interleave with asm in source order, no global reordering), and
our reimplementation mirrors it.

### AST
"Abstract syntax tree" — the parser's output, a tree of `Stmt`
and `Expr` nodes. Each carries a `Span` (byte range in the
source) so codegen can re-emit the original source lines as
comments. Defined in `crates/bcc/src/ast/`.

### IR
"Intermediate representation" — a compiler-internal program form
between AST and asm. We *don't* have one; **single-pass codegen**
walks the AST straight to bytes.

### Span
A `(start, end)` pair of byte offsets into the source string,
carried on every AST node. Codegen uses spans to find the
original source line(s) for emitting `;` comment blocks and to
key into the **label planner**'s slot map.

### Constant folding / fold
A compile-time simplification that replaces a constant
sub-expression with its value before emitting code. `return 1 + 2;`
emits `mov ax, 3` (no `add`). Lives in `codegen/fold.rs`.

### Planner / label plan
A pre-codegen pass that walks the AST and assigns a **slot**
number to every label-bearing construct (`if`, loop, `&&`/`||`,
comparison-as-value, switch). Codegen then queries the planner
during emission instead of choosing slot numbers ad-hoc. See
`crates/bcc/src/codegen/plan.rs`.

### Slot
A small integer identifying one label position within a function.
BCC numbers labels as `@<func>@<50 + 24·slot>`, so slot 0 is
label `@1@50`, slot 1 is `@1@74`, etc. The slot counter increments
strictly during planning — even slots that don't end up as
emitted labels still **burn** the counter ("ghost slots").

### Ghost slot
A slot the **planner** reserves but no actual label lands on.
Switch dispatch is the prominent case: chained-compare switches
reserve `#cases + 2` ghost slots before any case body, visible
only as a numbering gap (fixture 072's first case starts at slot
5, not slot 0). Why BCC reserves them is an open fingerprint
question.

### Emitter
The component that writes asm bytes to a buffer. In our codebase
the `FunctionEmitter` (in `codegen/mod.rs`) holds the per-function
state — output buffer, source/line map, locals layout, label plan,
loop stack — and exposes `emit_*` methods.

### Enregister
"To place in a register." BCC enregisters locals with ≥ 3 textual
uses into a fixed pool (SI for the most-used int; then DI, DX,
BX, CX in source order; DL/BL/CL for chars). The opposite is
**spill** (stay on the stack).

### Spill
Keeping a value on the stack rather than in a register. Used in
two senses:
1. *Permanent spill*: a local that wasn't eligible for a register
   stays at its `[bp-N]` slot for the whole function.
2. *Temporary spill*: a register's contents written to a stack
   slot to free the register for another use. BCC's linear-search
   switch uses this for the scrutinee (`[bp-4]` in fixture 074).

### Address-taken
A variable that appears as the target of `&x` somewhere in the
function. Address-taken variables must be **stack-resident** — a
register has no address to give. BCC's locals analyzer excludes
them from the register pool regardless of use count. _Fixture_: 080.

### Decay
In C, an array name used in most expression contexts implicitly
becomes a pointer to its first element. `f(a)` where `a` is an
array is equivalent to `f(&a[0])`; `int *p = a;` is equivalent to
`int *p = &a[0]`. BCC lowers both to the same `lea ax, word ptr
[bp-N]` that `&a[0]` would produce. Fixtures 090 (`int *p = a;`)
and 095 (`sum(a)`) pin this.

### Direct deref
A pointer dereference in a syntactic form BCC recognizes as a
single addressed-load idiom — `*p`, `p[i]`, or `*(p + <constant>)`.
Direct derefs contribute **2** to the pointer's enregistration
use-count (vs. 1 for plain arithmetic like `p + i` or `*(p + i)`).
The distinction shapes whether a pointer ends up in a register or
on the stack — see fixtures 091 vs. 092.

### Global / file-scope variable
A variable declared outside any function. Persists for the
program's whole lifetime. BCC routes globals through `DGROUP:_<name>`
references and partitions them between `_DATA` (initialized) and
`_BSS` (uninitialized). Fixtures 083–087.

### `s@` block
The label at the start of the trailing `_DATA` block that anchors
**string literals**. We'd been emitting `s@ label byte / _DATA ends`
as an empty marker in every prior fixture; with literals, the block
fills with `db '<chars>' / db 0` runs, one per unique literal.
The label itself is part of the BCC fingerprint — most compilers
use `LC0` / `$SG0` / etc.

### String pool
The data structure in our codegen (`StringPool` in `codegen/mod.rs`)
that accumulates string literals during emission, assigns each a
byte offset within the eventual `s@` block, and dedupes
identical literals. Filled per translation unit and drained when
the file tail is written.

### `_DATA` / `_BSS` / `_TEXT`
The three segments BCC partitions program content into under the
small memory model:
- **`_TEXT`**: code. Functions live here.
- **`_DATA`**: initialized data — globals with `= K` initializers,
  string literals, jump tables.
- **`_BSS`**: uninitialized data — globals with no initializer,
  declared by reservation (`db N dup (?)`).
The trailing `_TEXT segment ... _TEXT ends` after the data tail is
an empty re-open that BCC always emits, presumably as a "this
file's `_TEXT` is closed" marker for the linker.

### `DGROUP`
A linker-level grouping of related data segments. BCC groups
`_DATA` and `_BSS` into `DGROUP`. Every global access carries an
explicit `DGROUP:` segment override (`word ptr DGROUP:_g`), even
though `DS:` would normally resolve to the same thing — the
explicit override is a strong fingerprint of BCC.

### Lvalue / Rvalue
- **Lvalue** ("locator-value"): something that has an address —
  a variable, an array element, a pointer dereference. Can appear
  on the left side of `=`.
- **Rvalue**: a value with no permanent storage — a literal, the
  result of an expression. Cannot appear on the left of `=`.

The same syntax can be lvalue or rvalue depending on context:
`*p` on the right of `=` reads through the pointer, on the left
writes through it.

### Working register
The register a compiler treats as the default scratch / temporary.
For BCC, **AX** — values are computed into AX, then moved
elsewhere if a longer-lived storage is needed. AX is also the
return-value register.

### Callee-saved / caller-clobbered
A calling-convention classification of registers:
- **Callee-saved**: the function must preserve them across the
  call — if the function uses one, it pushes on entry and pops on
  exit. BCC treats SI and DI as callee-saved.
- **Caller-clobbered** (also "caller-saved" or "scratch"):
  the function may overwrite them freely. AX, BX, CX, DX, and
  their byte halves DL/BL/CL fall here for BCC. A value the
  caller wants to keep across a call must be in a callee-saved
  register or on the stack.

### Prologue / epilogue
The boilerplate at the start (**prologue**) and end (**epilogue**)
of a function. BCC's prologue: `push bp / mov bp,sp /
<allocate stack> / push <callee-saved regs>`. Epilogue is the
reverse: `pop <regs> / mov sp,bp / pop bp / ret`. Detailed in
`specs/bcc/ASM_OUTPUT.md`.

### Short-circuit
The C semantics of `&&` and `||` — the right operand is *not*
evaluated when the left already determines the result. BCC
implements this by jumping over the right operand's code, never
by computing it and combining.

---

## Assembly / x86 terms

### Small memory model
The x86 16-bit segmentation flavor where code fits in one 64 KB
**segment** and data fits in another — so all code addresses are
**near** (16-bit, intra-segment) and all data addresses are also
near. Selected via `bcc -ms`. Other flavors: **medium** (multi-
segment code, near data), **compact** (near code, far data),
**large** (far both), **huge** (large + large statics).

### Segment / segment override
A segment is a 64 KB block of the 8086 address space, addressed
via a segment register (CS, DS, ES, SS). A **segment override**
prefix (`cs:`, `ds:`, etc.) on an instruction tells the CPU to
use a non-default segment register for that one access. BCC uses
`cs:` overrides to read jump-table data co-located with code.

### Near vs far
- **Near**: a 16-bit offset within the current segment.
  `call near ptr _foo` pushes only a return offset.
- **Far**: a 32-bit segment:offset pair. `call far ptr _foo`
  pushes a segment too. BCC's **small model** only uses near.

### bp-relative addressing
Addressing memory as `[bp + N]` or `[bp - N]`, where BP is the
frame pointer set up in the **prologue** (`mov bp, sp`). Locals
live at negative offsets, **stack params** at positive ones.

### Real mode
The 8086/8088's only operating mode — segmented 20-bit address
space, no memory protection, no privilege levels. BCC targets
real-mode DOS.

### Mnemonic
The textual name of an instruction (`mov`, `add`, `cmp`, `je`,
…). Distinct from the binary opcode. BCC's `-S` output is text;
TASM converts mnemonics to opcodes.

### Signed vs unsigned jumps
x86 has two families of conditional jumps after a `cmp`: signed
(`jl/jle/jg/jge`) and unsigned (`jb/jbe/ja/jae`). BCC uses
**signed** for `int` comparisons (since C `int` is signed by
default), even when both operands are non-negative. The
exception is `ja` for switch bounds checks — using unsigned
catches negative scrutinees too, since their two's-complement
wrap sits above any reasonable max-case value.

### TASM, TLINK
**TASM** = "Turbo Assembler" — Borland's assembler, consumes
`.ASM` text and produces OMF `.OBJ`. **TLINK** = "Turbo Linker" —
consumes `.OBJ` and produces MZ EXEs. Both are part of the
**oracle** toolchain.

### OMF
"Object Module Format" — Intel's segmented-binary object format,
used for `.OBJ` files that TLINK consumes. See `specs/formats/`
(when populated).

### MZ executable
The DOS `.EXE` format, named for its "MZ" magic bytes. What
TLINK emits in real-mode mode. See `specs/formats/`.

### Translation unit (TU)
One `.C` file plus everything it includes, viewed as the unit of
compilation. BCC processes one TU at a time and emits one `.ASM`
(or `.OBJ`) per TU.

### LIFO
"Last in, first out." Used to describe BCC's `public` symbol
ordering at the end of an `.ASM`: symbols come out in the reverse
of the order they were defined, suggesting BCC walks its symbol
table as a stack.

### CRLF
Carriage return + line feed (`\r\n`), DOS line ending. BCC uses
CRLF throughout its `.ASM` output — a `.ASM` with bare LFs
wasn't built by BCC.

### 0x1A (DOS EOF)
The DOS end-of-text-file marker (Ctrl-Z). BCC appends a single
`0x1A` byte after the final newline of every `.ASM`. Weak
fingerprint — many DOS-era tools do this.

---

## Reverse-engineering / observation terms

### Determinism / clock pinning
A property we engineer into the oracle: the same input always
produces the same output, regardless of when it's run. The
mechanism is two-layered: input file mtimes are pinned with
`touch`, and DOSBox itself is wrapped in `faketime` so any
internal clock reads are stable. Without this, BCC's debug
records (which embed timestamps) would change every capture.
See `specs/RUNNING_BCC.md`.

### Advisory difference (vs. gating)
A diff the harness reports but doesn't fail on — typically
stdout/stderr from the oracle's noisy DOSBox session. Only files
listed as **gating** (the actual artifacts: `.ASM`, `.OBJ`, the
exit code) count for pass/fail. See `specs/FIXTURES.md`.

### Source-line comment block
BCC's `;` comments that mirror each source line into the asm,
emitted as a three-line block (blank-comment / source / blank-
comment) before the first asm instruction tied to that line.
Distinctive vs. MSC's single-line form. See
`specs/bcc/ASM_OUTPUT.md` "Source-line comments".

### C-num (data label)
The `<num>` in a `@<func>@C<num>` label. BCC uses these for
data tables co-located with code (switch jump tables, switch
linear-search tables). The numbering scheme is deterministic but
**not yet understood** — empirical fits work for our fixtures
but the constants have no clear source. Open question in
`specs/bcc/ASM_OUTPUT.md`.

---

## Fingerprint rating tiers

The three labels used in `specs/FINGERPRINTS.md`:

### DEFINITIVE
One sighting is essentially conclusive — assuming the binary
wasn't hand-edited, this pattern is so specific to BCC that no
other compiler is plausible.

### STRONG
Highly indicative on its own, but might be shared with closely
related toolchains (other Borland products of the same era).
Several STRONG hits together are conclusive.

### WEAK
Typical of the era — many compilers do this. Only useful as
corroborating evidence alongside STRONG / DEFINITIVE hits.
