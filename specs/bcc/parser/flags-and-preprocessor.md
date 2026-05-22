# Compiler flags and preprocessor

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## Preprocessor mechanics: object/fn-like macros expand inline + folded; `#if/elif/else` chooses one branch; `#undef`+redefine works; multi-line via `\`

Fixtures `2285`-`2290` cover preprocessor
mechanisms.

- `2285` (**object macro**): `#define ANSWER 42`
  then `return ANSWER` → `mov ax, 42`. Substituted
  at preprocess time, then parser const-folds.
- `2286` (**function-like macro**): `MAX(a,b)`
  expands inline at the call site as the ternary
  expression — NO function call:
  ```
  ; return MAX(x, y);  → return ((x) > (y) ? (x) : (y));
  mov si, 7              ; x
  mov di, 12             ; y
  cmp si, di
  jle false
  mov ax, si             ; x is max
  jmp end
  false:
    mov ax, di           ; y is max
  ```
- `2287` (**#if/#elif/#else**): branch chosen at
  preprocess time. Only the taken branch ends up
  in the parsed source. With MODE=2: returns 20.
- `2288` (**#undef + redefine**): `#undef` removes
  the macro; subsequent `#define` gives a new
  value. Final value (20) at use site.
- `2289` (**nested macros**): expanded transitively
  until no more substitutions possible. Then
  resulting arithmetic is const-folded:
  `C = (B*2) = ((A+3)*2) = ((5+3)*2) = 16`.
- `2290` (**multi-line macro via `\`**): line
  continuation merges into one logical body.
  `ADD3(1,2,3)` → 6.

**Preprocessor mechanics summary**:
| Directive | Behavior |
|-----------|----------|
| `#define NAME val` | Object macro; substitute literally |
| `#define NAME(args) body` | Fn-like macro; substitute with args bound |
| `#undef NAME` | Remove macro binding |
| `#if expr` | Evaluate at preprocess time; branch on result |
| `#ifdef NAME` | True if NAME is defined |
| `#ifndef NAME` | True if NAME is NOT defined |
| `#elif expr` | else-if for #if |
| `#else` | else clause |
| `#endif` | Close #if/#ifdef/#ifndef |
| `#include "file"` | Inline contents of file (project) |
| `#include <file>` | Inline contents of file (system) |
| `#error msg` | Abort compilation with msg |
| `#line N` | Set current line number |
| `#pragma X` | Implementation-defined directive |
| `\` at line end | Continuation (next line is same logical line) |

**Macro expansion order**:
1. Tokenize source
2. Expand all #define/#undef/#include directives
3. Evaluate #if/#elif/#else, removing dead branches
4. Expand function-like macros at use sites
5. Apply token-paste (##) and stringize (#) operators
6. Re-tokenize expansion result if it contains directives

The C preprocessor does NOT recursively expand a
macro within its own expansion (prevents infinite
recursion). After full expansion, the result is
plain C tokens for the parser.

**No codegen distinction**: in the OBJ, there's
no trace of whether `return 42` came from a
literal or `return ANSWER` with `#define ANSWER
42`. All preprocessor work happens before AST
construction.

For the Rust reimplementation:
- Implement a separate preprocessor pass.
- Track macro bindings in a stack (for
  redefinition behavior).
- Resolve #if expressions at preprocess time.
- Apply # and ## operators per C standard.

## `-2` (286) = mostly same as -1 in trivial cases; `-D NAME=val` macro substitutes at parse; `-O` strips `eb 00` no-ops

Fixtures `2279` (-2 286), `2280` (-D define),
`2281` (-O optimize) probe additional CLI flags.

- `2279` (**-2 80286**): for basic trivial code,
  output is essentially same as -1 (still ENTER/
  LEAVE, c1 shift, 6a push). The 286-specific
  instructions (imul reg, imm; bound; pusha/popa)
  are only used in specific patterns BCC may not
  hit in simple programs.
- `2280` (**-D DEBUG=42 + #ifdef**): macro
  resolved at preprocess time:
  ```
  ; Source #ifdef DEBUG ... return DEBUG ... #endif
  ; With -DDEBUG=42, becomes:
  ;     return 42;
  
  mov ax, 0x002A           ; b8 2a 00 = 42
  ret
  ```
- `2281` (**-O strip eb 00**): peephole pass
  removes `eb 00` no-op jumps that BCC inserts as
  basic-block markers in unoptimized output:
  ```
  ; Without -O: blocks have eb 00 between them
  ;     for example, end of if-body: ... eb 00 ...
  
  ; With -O: the eb 00 is stripped
  ```
  Output bytes are typically 5-15% smaller.

**Preprocessor flag forms**:
| Flag | Effect |
|------|--------|
| `-DNAME` | Define NAME with value 1 |
| `-DNAME=value` | Define NAME with value |
| `-UNAME` | Undefine NAME |
| `-Ipath` | Add include path |
| `-i` | Print include file names |

**Optimization-flag effects** (`-O`):
- Peephole pass after main codegen
- Removes `eb 00` (jmp +0) markers
- Removes redundant `mov reg, reg` (if any)
- Does NOT do CSE, inlining, register allocation
- Does NOT add ENTER/LEAVE or 186/286 instructions
  (those are gated by `-1`/`-2`)

**Cumulative CPU + opt flag combos** (BCC defaults):
| Flags | Behavior |
|-------|----------|
| (default) | 8086, no -O peephole |
| `-O` | 8086, peephole on |
| `-1` | 80186 forms, no -O peephole |
| `-1 -O` | 80186 + peephole (smallest output) |
| `-2` | 80286 forms (effectively superset of -1) |
| `-2 -O` | 80286 + peephole |

For the Rust reimplementation:
- Preprocessor: handle -D, -U, -I.
- Codegen: emit `eb 00` markers between basic
  blocks; strip them if `-O`.
- 186/286 forms gated by target CPU flag.

## `int f()` empty-paren = `(void)` in def; nested `#if` works; `#line` no OBJ effect

Fixtures `2165` (empty paren), `2166` (nested
`#if`), `2167` (`#line` directive) cover three
remaining syntactic patterns.

- `2165` (**`int trivial()` empty parens**): in a
  function **definition**, treated as `(void)`
  (no parameters). At call sites in K&R C, such
  a fn would accept any args (unchecked); BCC's
  parser handles both forms uniformly. OBJ
  identical to `int trivial(void)`.
- `2166` (**nested `#if`**): `#if LEVEL > 0 / #if
  LEVEL > 1 / #if LEVEL > 2 / #endif × 3` —
  each level evaluated independently at PP time.
  With LEVEL=2:
  - Outer: TAKEN (10 > 0)
  - Middle: TAKEN (LEVEL > 1)
  - Inner: NOT TAKEN
  
  Result: `x = 10 + 100 = 110`. Only the taken
  branches reach the compiler.
- `2167` (**`#line N "fname"`**): updates the
  preprocessor's idea of the current line and
  file. Used for `__LINE__`, `__FILE__`, warning/
  error message attribution. **No OBJ effect** —
  purely diagnostic.

**Preprocessor directive summary** (final):
| Directive | Effect | OBJ change |
|-----------|--------|------------|
| `#define X V` | Macro substitution | (indirect via expansion) |
| `#undef X` | Remove macro | (indirect) |
| `#ifdef X / #ifndef X / #endif` | Conditional inclusion | (indirect) |
| `#if expr / #elif / #else / #endif` | General conditional | (indirect) |
| `#include "f"` / `#include <f>` | File inclusion | (indirect via content) |
| `#pragma ...` | BCC-specific options | (varies) |
| `#error msg` | Compile error | (none — kills compile) |
| `#line N "fname"` | Override line/file | NONE |
| `defined(X)` | PP-expr operator | (indirect) |

**Function-declaration form-equivalence**:
| Form | Treatment in definition | Treatment in call |
|------|---------------------------|---------------------|
| `int f()` | No params (= void) | Any args (K&R: unchecked) |
| `int f(void)` | No params (= void) | Zero args required |
| `int f(int a, int b)` | Two params | Two args required |
| `int f(a, b) int a, b;` | K&R style, 2 params | Two args |
| `int f(int, ...)` | Varargs (at least 1) | At least 1 arg |

For the Rust reimplementation:
- Empty parens in fn def = void params.
- Nested `#if`: implement with a stack of "is
  this branch taken" flags.
- `#line` directive: update PP's source-location
  state; no codegen effect.

## `-O` consistent across expr sites; `-r-` disables reg-alloc (vars in mem); recursion -O removes ~4B

Fixtures `2126` (-O multi expr), `2127` (-r-
disable reg-alloc), `2128` (-O recursive
factorial) confirm/refine flag effects.

- `2126` (**-O across multi-expr**): no `eb 00`
  between expressions or before epilogue. Both x
  and y still enregister (SI/DI) — `-O` doesn't
  change register allocation, just removes the
  trailing no-op.
- `2127` (**`-r-` disables register allocation**):
  3 locals → 3 stack slots (`83 ec 06` for 6
  bytes). All accesses via `[bp+disp]`. None
  enregistered into SI/DI/BX:
  ```
  c7 46 fe 05 00          ; x = 5 (stack)
  c7 46 fc 0a 00          ; y = 10 (stack)
  c7 46 fa 14 00          ; z = 20 (stack)
  mov ax, [x] / add ax, [y] / add ax, [z]
  ```
  Larger code, more memory traffic. Useful only
  for debug/analysis.
- `2128` (**-O recursive factorial**): `-O`
  removes `eb 00` at every expression site. In
  recursive functions with multiple expressions,
  saves 2 bytes per site. Net savings = 2 × N
  sites.

**Optimization-flag effects** (full):
| Flag | Effect | Per-fn bytes saved |
|------|--------|---------------------|
| `-O` | Remove `eb 00` no-ops at expression ends | ~2 × #expr |
| `-d` | Merge duplicate string literals | varies |
| `-G` | (no observable trivial effect) | 0 |
| `-r` (default on) | Enable register allocation | (vars in SI/DI/BX) |
| `-r-` | DISABLE register allocation | -varies (worse code) |

**Register-allocation control summary**:
- Default (`-r` on): up to ~3 enregistered ints
  per function (pool {SI, BX, DI, CX, DX} per
  rules).
- `-r-`: all locals on stack.

For the Rust reimplementation:
- `-O`: trivially implementable as "don't emit
  the trailing `eb 00`."
- `-r-`: skip the register-allocation phase;
  emit every local as a stack slot.

## `-O` strips trailing `eb 00` no-op (-2B per expr); `-d` merges string dupes; `-G` no observable effect

Fixtures `2123` (-G flag), `2124` (-d merge
strings), `2125` (-O jump opt) probe BCC command-
line flags' codegen effects.

- `2123` (**`-G` flag**): byte-identical to default
  for the trivial case. May affect more complex
  cases (it's documented as "select for speed").
  No observable effect here.
- `2124` (**`-d` merge duplicate strings**):
  identical string literals `"hello"` and
  `"hello"` (declared in separate global decls)
  are **merged** to a single copy in `_DATA`.
  Both pointers reference the same offset. With
  `-d`, `a == b` is true; without it, they're
  separate copies.
- `2125` (**`-O` jump optimization**): strips the
  trailing **`eb 00` (jmp +0)** no-op that BCC
  normally emits at the end of expressions:
  ```
  ; Default:
  ... 33 c0 / eb 00 / 8b e5 / 5d c3  (15 bytes)
  
  ; With -O:
  ... 33 c0 / 8b e5 / 5d c3  (13 bytes)
  ```
  Saves **2 bytes per expression site**. Also
  shortens preceding jcc distances. The
  ubiquitous `eb 00` we've seen throughout the
  corpus is BCC's structural no-op — `-O`
  recognizes and removes it.

**BCC command-line flag summary** (codegen-
relevant):
| Flag | Effect |
|------|--------|
| `-c` | Compile only (no link) |
| `-ms` / `-mc` / `-mm` / `-ml` / `-mh` | Memory model |
| `-O` | Optimize jumps — removes `eb 00` no-ops |
| `-G` | Optimize for speed (no observable diff trivial) |
| `-d` | Merge duplicate strings in `_DATA` |
| `-w<class>` | Warning control |
| `-D<name>=<val>` | Define preprocessor symbol |
| `-U<name>` | Undefine preprocessor symbol |
| `-I<dir>` | Include path |
| `-v` | Source debug info |

For the Rust reimplementation:
- Match `-O` by removing the trailing-`eb 00`
  emission (and shortening preceding jcc by 2).
- Match `-d` by deduplicating string literals
  in `_DATA` at link/emission time.

## `#undef`+`#define` redefines; `defined()` operator; `asm` keyword = literal inline assembly

Fixtures `2117` (undef), `2118` (defined()), `2119`
(asm) cover three preprocessor/extension idioms.

- `2117` (**`#undef` then `#define` redefines**):
  ```c
  #define X 10
  int a = X;        // a = 10
  #undef X
  #define X 99
  int b = X;        // b = 99
  ```
  Macros are dictionary-style: definition-order
  matters. Use-site reflects the definition at
  that point.
- `2118` (**`defined()` operator in `#if`**):
  `#if defined(NAME)` ≡ `#ifdef NAME`, and `#if
  !defined(NAME)` ≡ `#ifndef NAME`. Both
  recognised in BCC's PP.
- `2119` (**`asm` keyword**): Borland-specific
  inline assembly. Each `asm <instr>` emits one
  literal assembly instruction:
  ```c
  asm mov ax, x;             // emits 8b 46 fe (mov ax, [bp-2])
  asm add ax, 1;             // emits 05 01 00 (add ax, 1, AX-form imm16)
  ```
  The inline assembler **does NOT optimise**:
  `asm add ax, 1` emits the literal `05 01 00`
  (3 bytes), NOT `inc ax` (1 byte). This contrasts
  with BCC's normal `+1` optimisation for C code.
  
  Multiple `asm` statements can be chained. Local
  C variables (`x`) can be referenced — BCC
  generates the right `[bp+disp]` for them.

**Preprocessor/extension summary (updated)**:
| Construct | Effect |
|-----------|--------|
| `#define X V` | Macro substitution |
| `#undef X` | Remove macro |
| `#if defined(X)`, `#ifdef X` | Conditional compilation |
| `#if !defined(X)`, `#ifndef X` | Inverse |
| `asm <instruction>` | Inline ASM (Borland extension, literal — no opts) |

For the Rust reimplementation:
- Preprocessor: dictionary semantics for define/
  undef; track definition order.
- `defined()` operator: implement in PP-expression
  evaluator.
- `asm` keyword: parse as Borland-specific
  statement; emit literal opcodes from the
  assembler.

## `//` comments supported (extension); `#define` expanded at PP; `#ifdef` removes untaken branch

Fixtures `2114` (C++ comments), `2115` (#define
macros), `2116` (#ifdef) cover preprocessor
behaviour.

- `2114` (**C++-style `//` comments**): BCC 2.0
  **supports `//` comments as an extension** (not
  part of C89). Same OBJ output as if the
  comments weren't there — stripped at PP.
- `2115` (**`#define` macros**): both object-like
  (`#define MAX 100`) and function-like (`#define
  DOUBLE(x) ((x)*2)`) macros expand at PP.
  `DOUBLE(20)` substitutes to `((20)*2) = 40`,
  which constant-folds. Compiler sees only the
  post-expansion source. Symbol `MAX` becomes
  literal 100 wherever used.
- `2116` (**`#ifdef`/`#else`/`#endif`**): resolves
  at preprocessing. Only the **taken branch** is
  in the compiled OBJ. The untaken branch is
  invisible — no conditional code at all.

**Preprocessor summary**:
| Directive | Resolution | Output effect |
|-----------|------------|----------------|
| `//` comment | Lex strip | None |
| `/* */` comment | Lex strip | None |
| `#define X 100` | Lex substitution | `X` → `100` |
| `#define F(x) ((x)*2)` | Lex substitution + paren | Function-like macro expansion |
| `#ifdef X / #else / #endif` | PP-time | Only one branch compiled |
| `#include "file"` | PP-time | File inlined |
| `#include <file>` | PP-time | System file inlined |
| `#pragma`, `#error`, etc. | (BCC-specific) | Various |

So preprocessing is **fully lexical** and runs
before BCC's tokenizer sees the source.

For the Rust reimplementation:
- Implement `//` comments alongside `/* */`.
- Macro expansion in lex/PP phase.
- `#ifdef` etc. control inclusion before the
  parser runs.

