# Shared file formats

References for file formats that more than one tool in the Borland chain
touches. Per-tool quirks (what BCC actually emits, what TLINK actually
consumes) live in the respective tool's directory; this directory is the
general spec — the format envelope, framing rules, and shared
conventions.

## Format coverage

| File         | Format                                | Status   | Produced by | Consumed by |
|--------------|---------------------------------------|----------|-------------|-------------|
| [`ASM.md`](ASM.md)   | TASM-flavored MASM text assembly | Drafted  | BCC `-S`     | TASM         |
| [`OMF.md`](OMF.md)   | Intel/Microsoft Object Module    | Drafted  | BCC `-c`, TASM | TLINK      |
| `MZ_EXE.md`  | DOS MZ executable                     | Stub     | TLINK        | DOS / DOSBox |
| [`LIB_ARCHIVE.md`](LIB_ARCHIVE.md) | Microsoft `.LIB` archive    | Drafted  | TLIB         | TLINK        |
| `DEBUG_INFO.md` | Borland Turbo Debugger symbols     | Stub     | BCC          | TD / TLINK   |

For each tool's specific use of these formats — what records BCC chooses
to emit, what TLINK's parser tolerates, what TASM's record ordering quirks
are — see the tool-specific specs in `../bcc/`, `../tasm/`, `../tlink/`.

Cite the spec source (Intel TIS, Borland docs, observed-from-fixtures) per
section so future readers can re-verify. Most fixtures so far have been
`-S` (text-ASM) and `-c` (OBJ); the EXE and LIB shapes will land when we
push past the assembler/object-emitter boundary.
