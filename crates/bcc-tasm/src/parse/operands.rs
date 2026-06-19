use super::*;

pub(crate) fn split_comma(s: &str) -> Option<(&str, &str)> {
    s.find(',').map(|i| (s[..i].trim(), s[i + 1..].trim()))
}
/// If `operand` contains a `<seg>:` segment-override prefix on its
/// memory-operand part (e.g. `word ptr ss:[si]` or `byte ptr es:[bx]`),
/// return the segment and the operand with the prefix removed. Used by
/// `parse_mov` to wrap an otherwise-recognized instruction in
/// `Instr::SegOverride`. The existing hardcoded `es:[bx]` shapes in
/// the far-pointer codegen don't go through here — this fires only
/// when removing the seg:`:` substring yields a parse the inner mov
/// can handle. Fixtures 4063–4068.
pub(crate) fn strip_segment_override(operand: &str) -> Option<(crate::ir::SegReg, String)> {
    let operand = operand.trim();
    for (kw, seg) in &[
        ("ss:", crate::ir::SegReg::Ss),
        ("es:", crate::ir::SegReg::Es),
        ("cs:", crate::ir::SegReg::Cs),
        ("ds:", crate::ir::SegReg::Ds),
    ] {
        if let Some(idx) = operand.find(kw) {
            // Only strip when `<seg>:` is immediately followed by `[`
            // (a memory operand). Avoids stripping `ss` that appears
            // as a register name (e.g. `mov ds, ss`).
            let after = &operand[idx + kw.len()..];
            if !after.starts_with('[') {
                continue;
            }
            let before = &operand[..idx];
            return Some((*seg, format!("{before}{after}")));
        }
    }
    None
}
/// Parse a decimal literal as an 8-bit value. None if the string
/// isn't a bare decimal in `-128..=255`.
pub(crate) fn parse_imm8(s: &str) -> Option<u8> {
    let s = s.trim();
    let v = s.parse::<i32>().ok()?;
    if (-128..=255).contains(&v) {
        Some(v as u8)
    } else {
        None
    }
}
/// Parse a decimal literal as a signed 8-bit value (sign-extends to
/// i16 at the instruction's caller). Used for `cmp <reg16>,<imm>`
/// where 83 /7 takes a sign-extended imm8. We also accept u16 values
/// in the upper half (32768..65535) by reinterpreting them as the
/// equivalent i16 — codegen frequently passes negative constants as
/// their unsigned 16-bit bit pattern (e.g. -5 = 65531). Fixture 563.
pub(crate) fn parse_imm8_signed(s: &str) -> Option<i8> {
    let s = s.trim();
    let v = s.parse::<i32>().ok()?;
    if let Ok(b) = i8::try_from(v) {
        return Some(b);
    }
    if (32_768..=65_535).contains(&v) {
        let as_i16 = v as i16; // reinterpret bit pattern
        return i8::try_from(as_i16).ok();
    }
    None
}
/// Parse a decimal literal as a 16-bit value. Returns `None` if the
/// string isn't a bare decimal. (BCC always uses bare decimals in
/// operands — no hex `42h` or octal forms.)
pub(crate) fn parse_imm16(s: &str) -> Option<u16> {
    let s = s.trim();
    // Accept C-style `0x<hex>` literals in addition to decimal. BCC's
    // inline-asm translates source hex to TASM's `<digits>H` form,
    // but our codegen leaves hex literals verbatim. Fixture 4056.
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
        && let Ok(v) = u32::from_str_radix(hex, 16)
        && v <= 0xFFFF
    {
        return Some(v as u16);
    }
    if let Ok(v) = s.parse::<i32>() {
        if (-32_768..=65_535).contains(&v) {
            return Some(v as u16);
        }
    }
    None
}
/// Parse `offset <symbol>` (no group prefix) — e.g. `offset _f`.
/// The frame for such fixups is the target's own segment (F5).
pub(crate) fn parse_offset_symbol(s: &str) -> Option<&str> {
    let s = s.trim();
    let sym = s.strip_prefix("offset ")?;
    let sym = sym.trim();
    // Reject `offset GROUP:sym` forms here — those route through
    // parse_offset_group_symbol instead.
    if sym.contains(':') {
        return None;
    }
    if sym.is_empty() {
        return None;
    }
    Some(sym)
}
/// Parse `offset <group>:<symbol>` (e.g. `offset DGROUP:s@`).
pub(crate) fn parse_offset_group_symbol(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let inside = s.strip_prefix("offset ")?;
    let (group, sym) = inside.split_once(':')?;
    let group = group.trim();
    let sym = sym.trim();
    if group.is_empty() || sym.is_empty() {
        return None;
    }
    Some((group, sym))
}
/// Parse `word ptr <group>:<symbol>` (e.g. `word ptr DGROUP:_x`).
/// Returns `(group, symbol)`.
pub(crate) fn parse_group_symbol(s: &str) -> Option<(&str, &str)> {
    parse_group_symbol_with_width(s, "word ptr ")
}
/// Same, but requires `byte ptr` (`byte ptr DGROUP:_g`).
pub(crate) fn parse_byte_group_symbol(s: &str) -> Option<(&str, &str)> {
    parse_group_symbol_with_width(s, "byte ptr ")
}
pub(crate) fn parse_group_symbol_with_width<'a>(s: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let s = s.trim();
    let inside = s.strip_prefix(prefix)?;
    let (group, sym) = inside.split_once(':')?;
    let group = group.trim();
    let sym = sym.trim();
    if group.is_empty() || sym.is_empty() {
        return None;
    }
    // Discriminate against `cs:_TEXT` style addressing-prefix uses by
    // requiring the symbol to look like a BCC-emitted symbol: start
    // with `_`/`@`, or be one of BCC's reserved aggregate-pool labels
    // (`s@` for the constant string/blob pool, `d@` for the data
    // pool). Without the explicit allowlist, `mov ax, word ptr
    // DGROUP:s@` would fail to parse and stack-init reads (1612,
    // 1613) wouldn't assemble.
    let leading = sym.chars().next();
    let is_pool_label = sym == "s@" || sym == "d@" || sym.starts_with("s@") || sym.starts_with("d@");
    if !matches!(leading, Some('_') | Some('@')) && !is_pool_label {
        return None;
    }
    Some((group, sym))
}
/// Parse `word ptr [si+K]` or `word ptr [si-K]` (also accepts `[si]`,
/// returning disp=0). Returns the signed displacement.
pub(crate) fn parse_word_si_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("word ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "si" {
        return Some(0);
    }
    let rest = inside.strip_prefix("si")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}
pub(crate) fn parse_word_di_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("word ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "di" {
        return Some(0);
    }
    let rest = inside.strip_prefix("di")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}
/// Parse `word ptr [bx]` or `word ptr [bx+K]`/`word ptr [bx-K]` —
/// BX-based addressing with optional disp8. Returns the (signed)
/// displacement (0 if absent). Used by the global-pointer compound
/// path `p[K] += y` where BCC loads the pointer into BX and emits
/// `<op> word ptr [bx+offset], ax` (fixture 862).
/// Parse `byte ptr [bp+si+K]` / `byte ptr [bp+si-K]` /
/// `byte ptr [bp+si]`, returning the signed disp8. Fixture 2488
/// (char-array index via `[BP+SI+disp]`).
pub(crate) fn parse_byte_bp_si_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("byte ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "bp+si" {
        return Some(0);
    }
    let rest = inside.strip_prefix("bp+si")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}
/// Parse `<prefix>+<disp>]` returning the disp byte. Caller passes
/// the leading `"word ptr es:[bx"` (or byte variant) so this just
/// peels off the trailing `+K]` or accepts `]` as disp=0. Disp=0
/// returns None because the no-disp encoding is a different opcode
/// (`MovEsBxAx` / `MovEsBxImm16`) — the caller's other rules pick
/// that up. Used for the indexed far-pointer store family
/// (fixture 1870).
pub(crate) fn parse_es_bx_disp(s: &str, prefix: &str) -> Option<u8> {
    let s = s.strip_prefix(prefix)?;
    let inside = s.strip_suffix(']')?;
    let rest = inside.strip_prefix('+')?;
    let signed: i32 = rest.parse().ok()?;
    u8::try_from(signed).ok()
}
pub(crate) fn parse_word_bx_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("word ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "bx" {
        return Some(0);
    }
    let rest = inside.strip_prefix("bx")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}
/// Byte-width sibling of [`parse_word_bx_disp`]. Recognizes
/// `byte ptr [bx]` and `byte ptr [bx+K]`/`byte ptr [bx-K]` used
/// by char-pointer subscripts (`char *p; p[K] op= …`, fixture 865).
pub(crate) fn parse_byte_bx_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("byte ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "bx" {
        return Some(0);
    }
    let rest = inside.strip_prefix("bx")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}
/// Parse `<width> ptr <group>:<sym>[<base>+K]` shapes for a specific
/// base register (`bx` or `si`) and width (`byte` or `word`).
/// Returns `(group, sym, disp)` where `disp` is the symbol's offset
/// plus any literal `+K` / `-K` on the symbol or the base expression.
pub(crate) fn parse_group_symbol_base_disp<'a>(
    s: &'a str,
    base: &str,
    width: &str,
) -> Option<(&'a str, &'a str, u16)> {
    let s = s.trim().strip_prefix(width)?.trim_start().strip_prefix("ptr ")?;
    let (group, rest) = s.split_once(':')?;
    let group = group.trim();
    let (sym_part, idx_part) = rest.split_once('[')?;
    let sym_part = sym_part.trim();
    let (sym, sym_disp): (&str, i32) = if let Some(idx) = sym_part.rfind('+') {
        let (s, d) = sym_part.split_at(idx);
        let d_val = d[1..].parse::<i32>().ok()?;
        (s, d_val)
    } else if let Some(idx) = sym_part.rfind('-') {
        if idx == 0 {
            (sym_part, 0)
        } else {
            let (s, d) = sym_part.split_at(idx);
            let d_val = d.parse::<i32>().ok()?;
            (s, d_val)
        }
    } else {
        (sym_part, 0)
    };
    if !sym.starts_with('_') && !sym.starts_with('@') {
        return None;
    }
    let idx = idx_part.strip_suffix(']')?.trim();
    let base_disp = if idx == base {
        0i32
    } else if let Some(k) = idx.strip_prefix(&format!("{base}+")) {
        k.trim().parse::<i32>().ok()?
    } else if let Some(k) = idx.strip_prefix(&format!("{base}-")) {
        -k.trim().parse::<i32>().ok()?
    } else {
        return None;
    };
    let total = (sym_disp + base_disp) as i16 as u16;
    Some((group, sym, total))
}
/// Parse `word ptr <group>:<sym>[bx]` or `word ptr <group>:<sym>[bx+K]`,
/// returning `(group, sym, disp)`. The displacement defaults to 0 when
/// `[bx]` has no `+K`. Used by variable-indexed long-array reads
/// (fixture 303: `mov ax, word ptr DGROUP:_a[bx+2]`).
pub(crate) fn parse_group_symbol_bx_disp(s: &str) -> Option<(&str, &str, u16)> {
    parse_group_symbol_bx_disp_width(s, "word")
}
/// Byte-width sibling of `parse_group_symbol_bx_disp`.
pub(crate) fn parse_byte_group_symbol_bx_disp(s: &str) -> Option<(&str, &str, u16)> {
    parse_group_symbol_bx_disp_width(s, "byte")
}
/// `byte ptr <group>:<sym>[<reg>]` for an arbitrary index register
/// (used for SI/DI alongside the BX shape).
pub(crate) fn parse_byte_group_symbol_reg_disp<'a>(s: &'a str, reg: &str) -> Option<(&'a str, &'a str, u16)> {
    let prefix = "byte ptr ";
    let s = s.trim().strip_prefix(prefix)?;
    let (group, rest) = s.split_once(':')?;
    let group = group.trim();
    let (sym_part, idx_part) = rest.split_once('[')?;
    let sym_part = sym_part.trim();
    let (sym, sym_disp): (&str, i32) = if let Some(idx) = sym_part.rfind('+') {
        let (s, d) = sym_part.split_at(idx);
        let d_val = d[1..].parse::<i32>().ok()?;
        (s, d_val)
    } else if let Some(idx) = sym_part.rfind('-') {
        if idx == 0 {
            (sym_part, 0)
        } else {
            let (s, d) = sym_part.split_at(idx);
            let d_val = d.parse::<i32>().ok()?;
            (s, d_val)
        }
    } else {
        (sym_part, 0)
    };
    if !sym.starts_with('_') && !sym.starts_with('@') {
        return None;
    }
    let idx = idx_part.strip_suffix(']')?.trim();
    let reg_disp = if idx == reg {
        0i32
    } else if let Some(k) = idx.strip_prefix(&format!("{reg}+")) {
        k.trim().parse::<i32>().ok()?
    } else if let Some(k) = idx.strip_prefix(&format!("{reg}-")) {
        -k.trim().parse::<i32>().ok()?
    } else {
        return None;
    };
    let total = sym_disp.checked_add(reg_disp)?;
    let disp = u16::try_from(total & 0xFFFF).ok()?;
    Some((group, sym, disp))
}
pub(crate) fn parse_group_symbol_bx_disp_width<'a>(s: &'a str, width: &str) -> Option<(&'a str, &'a str, u16)> {
    let prefix = format!("{width} ptr ");
    let s = s.trim().strip_prefix(prefix.as_str())?;
    let (group, rest) = s.split_once(':')?;
    let group = group.trim();
    // rest is `_sym[bx]`, `_sym[bx+K]`, `_sym+K[bx]`, `_sym-K[bx]`, or
    // combinations.
    let (sym_part, idx_part) = rest.split_once('[')?;
    let sym_part = sym_part.trim();
    // Allow a `+K` or `-K` suffix on the symbol (the FIXUPP-disp
    // contribution from a folded constant array offset like
    // `arr[i+1]` → `_arr+2[bx]`).
    let (sym, sym_disp): (&str, i32) = if let Some(idx) = sym_part.rfind('+') {
        let (s, d) = sym_part.split_at(idx);
        let d_val = d[1..].parse::<i32>().ok()?;
        (s, d_val)
    } else if let Some(idx) = sym_part.rfind('-') {
        // `_sym-K` — guard against false-positive at position 0
        // (e.g. `-` at start of a label, though none of our symbols
        // start with `-`).
        if idx == 0 {
            (sym_part, 0)
        } else {
            let (s, d) = sym_part.split_at(idx);
            let d_val = d.parse::<i32>().ok()?; // includes the minus sign
            (s, d_val)
        }
    } else {
        (sym_part, 0)
    };
    if !sym.starts_with('_') && !sym.starts_with('@') {
        return None;
    }
    let idx = idx_part.strip_suffix(']')?.trim();
    let bx_disp = if idx == "bx" {
        0i32
    } else if let Some(k) = idx.strip_prefix("bx+") {
        k.trim().parse::<i32>().ok()?
    } else if let Some(k) = idx.strip_prefix("bx-") {
        -k.trim().parse::<i32>().ok()?
    } else {
        return None;
    };
    let total = (sym_disp + bx_disp) as i16 as u16;
    Some((group, sym, total))
}
/// Strip a trailing `+<integer>` from a symbol, returning
/// `(name, offset)`. `_a+2` → `("_a", 2)`. No `+` → `(s, 0)`.
pub(crate) fn split_sym_offset(s: &str) -> (&str, i16) {
    if let Some((name, off)) = s.split_once('+') {
        if let Ok(n) = off.trim().parse::<i16>() {
            return (name.trim(), n);
        }
    }
    (s, 0)
}
/// Parse `word ptr [bp<sign><offset>]` or `[bp<sign><offset>]`.
/// Returns the signed displacement.
pub(crate) fn parse_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Any)
}
/// Same as [`parse_bp_relative`] but requires an explicit `byte ptr`
/// prefix. Used when an 8-bit operand context shouldn't accidentally
/// accept a `word ptr` reference.
pub(crate) fn parse_byte_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Byte)
}
/// Parse `byte ptr [si]` or `byte ptr [si+K]`/`byte ptr [si-K]` —
/// SI-based byte addressing with optional disp8. Returns the (signed)
/// displacement. Used by the char-pointer subscript byte-store path
/// (fixture 1016: `p[K] = 'X'` with p in SI).
pub(crate) fn parse_byte_si_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("byte ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "si" {
        return Some(0);
    }
    let rest = inside.strip_prefix("si")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}
/// DI sibling of [`parse_byte_si_disp`] — `byte ptr [di]` / `[di+disp]`.
pub(crate) fn parse_byte_di_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("byte ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "di" {
        return Some(0);
    }
    let rest = inside.strip_prefix("di")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}
/// Same as [`parse_bp_relative`] but requires an explicit `word ptr`
/// prefix. Used on LHS stack-store opcodes where the width prefix
/// chooses the opcode (C6 vs C7).
pub(crate) fn parse_word_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Word)
}
pub(crate) fn parse_dword_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Dword)
}
pub(crate) fn parse_qword_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Qword)
}
pub(crate) fn parse_bp_relative_with_width(s: &str, width: BpWidth) -> Option<i16> {
    let s = s.trim();
    let inside = match width {
        BpWidth::Any => s
            .strip_prefix("word ptr ")
            .or_else(|| s.strip_prefix("byte ptr "))
            .unwrap_or(s),
        BpWidth::Byte => s.strip_prefix("byte ptr ")?,
        BpWidth::Word => s.strip_prefix("word ptr ")?,
        BpWidth::Dword => s.strip_prefix("dword ptr ")?,
        BpWidth::Qword => s.strip_prefix("qword ptr ")?,
    };
    let inside = inside.strip_prefix('[')?.strip_suffix(']')?;
    let inside = inside.strip_prefix("bp")?;
    let inside = inside.trim_start();
    if inside.is_empty() {
        return Some(0);
    }
    inside.parse::<i16>().ok()
}
pub(crate) fn unquote(s: &str) -> Option<&str> {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}
pub(crate) fn decode_hex(s: &str, line_no: usize) -> AsmResult<Vec<u8>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.len() % 2 != 0 {
        return Err(AsmError::new(
            line_no,
            format!("?debug C: hex blob has odd length: {trimmed:?}"),
        ));
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let b = trimmed.as_bytes();
    for chunk in b.chunks_exact(2) {
        let hi = hex_digit(chunk[0], line_no)?;
        let lo = hex_digit(chunk[1], line_no)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}
pub(crate) fn hex_digit(c: u8, line_no: usize) -> AsmResult<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        other => Err(AsmError::new(
            line_no,
            format!("invalid hex digit: {:?}", char::from(other)),
        )),
    }
}
