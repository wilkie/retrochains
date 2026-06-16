# TLINK `.MAP` listing

The map file TLINK writes with `/m` — a segment table plus the public symbols,
twice (by name and by value), and the entry point. Reproduced byte-exact by
`crates/bcc-tlink` (`map.rs`), gated by the standalone-linker fixtures
(`fixtures/c/linking/standalone/`). CRLF line endings throughout.

## Layout

```
<blank>
 Start  Stop   Length Name               Class
<blank>
 00000H 00005H 00006H _TEXT              CODE
 …one row per combined segment, in load order…
<blank>
  Address         Publics by Name
<blank>
 0000:0008       ANSWER
 …one row per public, sorted by name (ASCII)…
<blank>
  Address         Publics by Value
<blank>
 …same publics, sorted by frame:offset…
<blank>
Program entry point at 0000:0000
<blank>
```

## Fields

- **Segment row** — fixed columns: `' '`, `start` (`{:05X}H`), `' '`, `stop`,
  `' '`, `length`, `' '`, `name` left-justified in 19 chars, `class`. `start` is
  the segment's linear load address; `stop = start + length − 1` (or `start`
  when `length = 0`); empty segments are listed too. Rows are in load order.
- **Public row** — `' '`, `frame` (`{:04X}`), `':'`, `offset` (`{:04X}`), seven
  spaces, then the name. `frame` is the symbol's segment paragraph,
  `offset = addr − frame·16`.
- **Entry point** — `Program entry point at <cs>:<ip>` from the MODEND start
  address.

The map is the high-signal EXE-level fingerprint distinguishing TLINK from MS
LINK (segment class vocabulary, column layout); see
[`../../linkers/DIFFERENCES.md`](../../linkers/DIFFERENCES.md).
