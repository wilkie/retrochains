//! Codegen: walk a [`Function`] AST and emit the per-function asm bytes
//! BCC's `-S` would have produced. The format-emitter (`emit_s.rs`) calls
//! us between the file-level scaffolding (preamble + debug records +
//! segment scaffold) and the tail.
//!
//! Single-pass-ish shape: we don't build any IR; we walk the AST in
//! source order and write asm directly. Source-line comments are
//! emitted just before the asm for each new source line we encounter
//! (mirroring BCC's interleaving — see `specs/bcc/ASM_OUTPUT.md`).
//! There are two preparatory passes per function: a local-layout
//! analyzer (`locals.rs`) and a label planner (`plan.rs`).

use std::collections::HashMap;
use std::io::Write as _;

use crate::ast::{
    BinOp, Expr, ExprKind, Function, LogicalOp, Stmt, StmtKind, SwitchCase, Type, UnaryOp, Unit,
    UpdateOp, UpdatePosition,
};

/// Maps each function's name to the declared types of its parameters,
/// in source order. Built once per translation unit and consulted at
/// call sites so we know whether to push each argument as a byte or
/// a word (fixture 052: `f(1)` where `f` takes `char` becomes
/// `mov al,1 / push ax`, not `mov ax,1 / push ax`).
#[derive(Debug, Default)]
pub struct Signatures {
    map: HashMap<String, FunctionSig>,
}

#[derive(Debug)]
struct FunctionSig {
    params: Vec<Type>,
    ret_ty: Type,
    is_pascal: bool,
    is_far: bool,
    /// True when this entry came from a prototype-only declaration
    /// (no `body` in the AST). Used by the call-site emitter to
    /// distinguish defined-in-TU functions from externs even when
    /// the prototype contributes a known parameter signature.
    is_prototype: bool,
}

impl Signatures {
    #[must_use]
    pub fn from_unit(unit: &Unit) -> Self {
        let map = unit
            .functions
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    FunctionSig {
                        params: f.params.iter().map(|p| p.ty.clone()).collect(),
                        ret_ty: f.ret_ty.clone(),
                        is_pascal: f.is_pascal,
                        is_far: f.is_far,
                        is_prototype: f.body.is_none(),
                    },
                )
            })
            .collect();
        Self { map }
    }

    /// Look up the declared parameter types of a function. Returns
    /// `None` if the name isn't defined in this TU (extern function).
    /// Callers should default to `int` widths for missing signatures —
    /// we have no fixture for extern char-arg calls yet.
    #[must_use]
    pub fn params_of(&self, name: &str) -> Option<&[Type]> {
        self.map.get(name).map(|s| s.params.as_slice())
    }

    /// Look up the declared return type of a function. Returns `None`
    /// for unknown (extern) names. Used by call-site codegen to choose
    /// the right ABI shape for the return value (e.g. fixture 214 —
    /// stash DX:AX after a long-returning call).
    #[must_use]
    pub fn ret_ty_of(&self, name: &str) -> Option<&Type> {
        self.map.get(name).map(|s| &s.ret_ty)
    }

    /// Is this function declared with the `pascal` calling convention?
    /// Determines the call-site argument push order (LTR vs RTL),
    /// whether the caller cleans the stack (no for pascal), and the
    /// symbol name (uppercase / no underscore).
    #[must_use]
    pub fn is_pascal(&self, name: &str) -> bool {
        self.map.get(name).is_some_and(|s| s.is_pascal)
    }

    /// Is this function declared `far`? Call sites push CS before
    /// the near call (simulating a far call).
    #[must_use]
    pub fn is_far(&self, name: &str) -> bool {
        self.map.get(name).is_some_and(|s| s.is_far)
    }

    /// True when `name` is declared via a prototype but has no body
    /// in this TU — i.e. it's an extern function whose definition
    /// lives in another module. Distinct from "not in signatures
    /// at all" which means no declaration was seen either.
    #[must_use]
    pub fn is_extern_function(&self, name: &str) -> bool {
        self.map
            .get(name)
            .map_or(true, |s| s.is_prototype)
    }
}

pub mod fold;
mod line_map;
mod locals;
mod plan;

use fold::try_const_eval;

/// Public re-export so the file-emitter can fold a global-variable
/// initializer down to its constant byte value.
#[must_use]
pub fn fold_const_global(expr: &crate::ast::Expr) -> Option<u32> {
    try_const_eval(expr)
}
use line_map::LineMap;
use locals::{LocalLocation, Locals, ParamLoad, Reg, expr_has_call};

/// File-scope variable lookup. Built once per translation unit from
/// `Unit::globals` and consulted by codegen whenever an `Ident`
/// reference doesn't match a local — at which point the reference
/// lowers to `<width> ptr DGROUP:_<name>` instead of `[bp-N]`.
#[derive(Debug, Default)]
pub struct GlobalTable {
    map: HashMap<String, crate::ast::Type>,
    statics: std::collections::HashSet<String>,
}

impl GlobalTable {
    #[must_use]
    pub fn from_unit(unit: &Unit) -> Self {
        let map = unit
            .globals
            .iter()
            .map(|g| (g.name.clone(), g.ty.clone()))
            .collect();
        let statics = unit
            .globals
            .iter()
            .filter(|g| g.is_static)
            .map(|g| g.name.clone())
            .collect();
        Self { map, statics }
    }

    #[must_use]
    pub fn type_of(&self, name: &str) -> Option<&crate::ast::Type> {
        self.map.get(name)
    }

    /// Iterate over all declared global names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(|k| k.as_str())
    }

    /// True if this global was declared with `static`. BCC emits
    /// its storage label without the `_` prefix and writes a
    /// `_<name> equ <name>` alias in the publics tail.
    #[must_use]
    pub fn is_static(&self, name: &str) -> bool {
        self.statics.contains(name)
    }

    /// Find a struct definition by its tag name. Some AST nodes
    /// (notably `Type::Pointer(Struct{name, fields:[], …})`) carry a
    /// name-only placeholder where the recursive struct definition
    /// would otherwise create a cycle. To resolve fields off such a
    /// pointer we look up a full instance of the same tag among the
    /// globals' types. Returns the first struct found whose tag
    /// matches; the struct definition is unique by tag at file scope.
    #[must_use]
    pub fn lookup_struct_by_tag(&self, tag: &str) -> Option<&crate::ast::Type> {
        fn find<'a>(ty: &'a crate::ast::Type, tag: &str) -> Option<&'a crate::ast::Type> {
            match ty {
                crate::ast::Type::Struct { name: Some(t), fields, .. }
                    if t == tag && !fields.is_empty() =>
                {
                    Some(ty)
                }
                crate::ast::Type::Struct { fields, .. } => {
                    fields.iter().find_map(|f| find(&f.ty, tag))
                }
                crate::ast::Type::Array { elem, .. } => find(elem, tag),
                crate::ast::Type::Pointer(inner) => find(inner, tag),
                _ => None,
            }
        }
        self.map.values().find_map(|t| find(t, tag))
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }
}

/// Accumulator for constant data encountered during codegen of a
/// translation unit: string literals **and** stack-array initializer
/// blobs. Each unique entry gets a stable byte offset within the
/// `s@` block; identical entries deduplicate. Emission of the actual
/// `db` block happens in the tail of the file (`emit_s.rs::write_tail`).
#[derive(Debug, Default)]
pub struct StringPool {
    /// Each entry: (raw bytes, whether to append a NUL terminator).
    /// Strings always set `nul = true`; array-init blobs set `nul =
    /// false` because the array's declared size already includes any
    /// trailing zeros. The running total of `bytes.len() + nul as
    /// usize` is the next available offset.
    entries: Vec<PoolEntry>,
    /// When true, identical string literals share a slot. BCC's `-d`
    /// flag enables this; default is off (each occurrence gets its
    /// own slot). Array-init blobs always dedup regardless — the
    /// flag only affects string-literal interning.
    pub merge_strings: bool,
    /// Per-occurrence offsets keyed by the StringLit AST span. With
    /// `merge_strings = false`, multiple occurrences of the same
    /// content each get their own pool slot — but the codegen emits
    /// in a different order than the pre-intern walk, so we need to
    /// look up offsets by AST identity rather than by content.
    /// `intern_at` populates this map; `intern_for_span` reads it.
    span_offsets: std::collections::HashMap<u32, u32>,
}

#[derive(Debug, Clone)]
pub struct PoolEntry {
    pub bytes: Vec<u8>,
    pub nul: bool,
}

impl StringPool {
    /// Intern a NUL-terminated string literal. Returns the offset of
    /// the first byte within `s@`. With `merge_strings = true`,
    /// identical literals dedupe; otherwise each call creates a
    /// fresh entry.
    pub fn intern(&mut self, bytes: &[u8]) -> u32 {
        self.intern_inner(bytes, true)
    }

    /// Intern at a specific AST span — used by the pre-intern pass
    /// (source-order traversal of call args) to reserve a pool slot
    /// for a specific StringLit occurrence. Stores the resulting
    /// offset under the span key so the later codegen pass can look
    /// it up via `offset_for_span` without creating a duplicate.
    pub fn intern_at(&mut self, span_start: u32, bytes: &[u8]) -> u32 {
        let offset = self.intern_inner(bytes, true);
        self.span_offsets.insert(span_start, offset);
        offset
    }

    /// Look up the pool offset previously reserved at `span_start`
    /// by `intern_at`. Returns `None` if this StringLit didn't go
    /// through the pre-intern pass (e.g. it appears in a context
    /// where pre-intern wasn't run).
    pub fn offset_for_span(&self, span_start: u32) -> Option<u32> {
        self.span_offsets.get(&span_start).copied()
    }

    /// Intern a raw byte blob (e.g. a stack-array initializer image).
    /// No NUL terminator is appended — the blob's declared size is
    /// already baked into the bytes the caller supplies.
    pub fn intern_blob(&mut self, bytes: &[u8]) -> u32 {
        self.intern_inner(bytes, false)
    }

    /// Intern a 32-bit `float` constant. The IEEE 754 bit pattern is
    /// written little-endian (low byte first). BCC stores float
    /// constants in the same `s@` pool as strings and struct-init
    /// blobs, so the only thing distinguishing them at emission time
    /// is the byte image itself — `s@` becomes a heterogeneous mix
    /// of strings and raw numeric bytes, all rendered through the
    /// same `db` emitter. Returns the offset within `s@`.
    pub fn intern_float(&mut self, bits: u32) -> u32 {
        self.intern_inner(&bits.to_le_bytes(), false)
    }

    /// Intern a 64-bit `double` constant. Same pool, 8 little-endian
    /// bytes. Note BCC sometimes stores a double-typed initializer as
    /// a 32-bit float in the pool when the value is exactly
    /// representable in single precision (`fld dword` → 80-bit FPU
    /// promotion → `fstp qword` truncation gives the same bits), but
    /// that lossless-narrowing decision lives in the codegen caller,
    /// not the pool: this method always stores the full 8 bytes.
    pub fn intern_double(&mut self, bits: u64) -> u32 {
        self.intern_inner(&bits.to_le_bytes(), false)
    }

    fn intern_inner(&mut self, bytes: &[u8], nul: bool) -> u32 {
        // String-literal entries (`nul = true`) only dedupe when
        // `merge_strings` is set (BCC's `-d` flag). Blob entries
        // (`nul = false`) always dedupe — they're typically large
        // const initializers where duplication wastes space.
        let allow_dedup = !nul || self.merge_strings;
        let mut offset: u32 = 0;
        for existing in &self.entries {
            if allow_dedup
                && existing.bytes.as_slice() == bytes
                && existing.nul == nul
            {
                return offset;
            }
            offset += u32::try_from(existing.bytes.len() + usize::from(existing.nul))
                .expect("pool offset fits in u32");
        }
        self.entries.push(PoolEntry { bytes: bytes.to_vec(), nul });
        offset
    }

    /// True when no entries have been interned. Tail emission can
    /// skip the `db` lines entirely in that case (matching the
    /// "empty s@ block" we used to always emit).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The interned entries in insertion order. Tail emission writes
    /// each as `db '<chars>'` (strings, plus an explicit `db 0`) or as
    /// a raw `db <byte>` sequence (blobs).
    #[must_use]
    pub fn entries(&self) -> &[PoolEntry] {
        &self.entries
    }
}

/// Format a bp-relative address: negative offsets are written
/// `[bp-N]`, positives `[bp+N]`. Used by every `word ptr` / `byte ptr`
/// memory operand a local/param produces.
/// Look up a struct field's full metadata (including bitfield
/// info, which `Type::field` discards). Returns `None` if `ty`
/// isn't a struct or the field name isn't present.
fn struct_field_info<'a>(ty: &'a Type, name: &str) -> Option<&'a crate::ast::StructField> {
    if let Type::Struct { fields, .. } = ty {
        fields.iter().find(|f| f.name == name)
    } else {
        None
    }
}

/// Information needed to read or write a bitfield: the storage
/// address (byte or word ptr operand), the access width, and the
/// bit-level placement. Returned by
/// [`FunctionEmitter::resolve_bitfield`] when the expression
/// matches a supported lvalue shape (currently: dotted member of a
/// stack-resident struct local).
struct BitfieldRef {
    addr: String,
    /// Width of the memory access — `byte` when the field fits in
    /// one byte (`bit_offset + bit_width <= 8`), `word` otherwise.
    /// BCC uses 16-bit memory ops for cross-byte bitfields rather
    /// than two byte ops (fixture 1880).
    access: BitfieldAccess,
    /// Bit offset relative to the LSB of `addr` (in the byte for
    /// `Byte` access, in the word for `Word` access).
    bit_offset: u8,
    bit_width: u8,
    /// True when the declared field type is signed (`int b : 4`).
    /// Signed bitfields sign-extend on read — SHL/SAR rather than
    /// SHR/AND. Writes are width-equivalent to unsigned.
    signed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitfieldAccess {
    Byte,
    Word,
}

impl BitfieldAccess {
    fn ptr(self) -> &'static str {
        match self {
            Self::Byte => "byte",
            Self::Word => "word",
        }
    }
}

/// True iff `e` is a float/double literal whose value is exactly
/// 1.0. BCC uses the FPU built-in `fld1` for these instead of
/// pooling the IEEE bytes.
fn expr_is_float_one(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::FloatLit(bits) => f32::from_bits(*bits) == 1.0_f32,
        ExprKind::DoubleLit(bits) => f64::from_bits(*bits) == 1.0_f64,
        _ => false,
    }
}

fn bp_addr(off: i16) -> String {
    if off < 0 {
        format!("[bp-{}]", -i32::from(off))
    } else {
        format!("[bp+{off}]")
    }
}

/// Two-register addressing `[base+index{+disp}]`, e.g. `[bx+si+4]`.
/// Used for struct-pointer array-field access where the scaled index
/// is in `base` and the enregistered pointer is in `index`.
fn two_reg_addr(base: Reg, index: Reg, disp: i32) -> String {
    let (b, i) = (base.name(), index.name());
    if disp == 0 {
        format!("[{b}+{i}]")
    } else if disp > 0 {
        format!("[{b}+{i}+{disp}]")
    } else {
        format!("[{b}+{i}-{}]", -disp)
    }
}

/// Flatten an aggregate initializer to its little-endian byte image,
/// matching what BCC would place in the `s@` block / `_DATA` segment
/// for a stack array init.
///
/// Returns `None` if any element isn't constant-evaluatable (the
/// caller falls back to a more general lowering path or panics).
///
/// Handles:
///   - `InitList { items }` against an array — recurses element-wise
///     and zero-fills any trailing un-specified slots.
///   - `InitList { items }` against a struct — pairs items with fields
///     in declaration order; zero-fills missing fields and any trailing
///     end-padding the struct carries.
///   - `StringLit(bytes)` against a char array — writes bytes plus a
///     NUL, then zero-fills to the declared length (fixture 1476: `char
///     s[6] = "hi"` → `'h' 'i' 0 0 0 0`).
///   - Any scalar that `try_const_eval` accepts — written little-endian
///     as `ty.size_bytes()` bytes.
fn flatten_init_to_bytes(ty: &Type, init: &Expr) -> Option<Vec<u8>> {
    let total = usize::from(ty.size_bytes());
    let mut buf = vec![0u8; total];
    if write_init_bytes(&mut buf[..], ty, init)? {
        Some(buf)
    } else {
        None
    }
}

/// Write a single initializer into `dst` (already pre-zeroed). The
/// returned `bool` is always `true` on success; the `Option` shape
/// is reserved for "this initializer shape isn't constant" failures.
fn write_init_bytes(dst: &mut [u8], ty: &Type, init: &Expr) -> Option<bool> {
    debug_assert_eq!(dst.len(), usize::from(ty.size_bytes()));
    // String literal initializing a char array: copy bytes, append a
    // NUL, leave the rest zero (the pre-zeroed buffer already covers
    // any padding).
    if let (ExprKind::StringLit(bytes), Type::Array { elem, .. }) = (&init.kind, ty)
        && elem.is_char_like()
    {
        let take = bytes.len().min(dst.len());
        dst[..take].copy_from_slice(&bytes[..take]);
        // The NUL terminator slots in at `bytes.len()` if there's room.
        // For exactly-sized arrays where `bytes.len() == dst.len()` the
        // parser would have already widened the array; if we're called
        // with a too-tight array we just truncate the NUL away (mirrors
        // BCC behavior).
        return Some(true);
    }
    // Aggregate initializer list — recurse element-wise (arrays) or
    // field-wise (structs).
    if let ExprKind::InitList { items } = &init.kind {
        match ty {
            Type::Array { elem, len } => {
                let elem_size = usize::from(elem.size_bytes());
                for (i, item) in items.iter().enumerate() {
                    if i >= *len as usize {
                        break;
                    }
                    let start = i * elem_size;
                    let end = start + elem_size;
                    write_init_bytes(&mut dst[start..end], elem, item)?;
                }
                return Some(true);
            }
            Type::Struct { fields, .. } => {
                for (item, field) in items.iter().zip(fields.iter()) {
                    let start = usize::from(field.offset);
                    let end = start + usize::from(field.ty.size_bytes());
                    write_init_bytes(&mut dst[start..end], &field.ty, item)?;
                }
                return Some(true);
            }
            _ => return None,
        }
    }
    // Scalar: must fold to a constant. Long-like types get all 4 bytes;
    // ints/pointers get 2; chars get 1.
    let v = try_const_eval(init)?;
    for (i, slot) in dst.iter_mut().enumerate() {
        *slot = ((v >> (i * 8)) & 0xFF) as u8;
    }
    Some(true)
}

/// `DGROUP:_<sym>` or `DGROUP:_<sym>+<off>` — the asm-text form BCC
/// uses when addressing into a global's body at a known offset (long
/// halves, struct fields, array element bases). `off == 0` collapses
/// to the bare symbol; otherwise `+<off>` is appended.
fn global_offset_addr(sym: &str, off: i32) -> String {
    if off == 0 {
        format!("DGROUP:_{sym}")
    } else {
        format!("DGROUP:_{sym}+{off}")
    }
}

/// Look at the end of `buf` for the 4-instruction LHS-mem-clobber tail
/// Returns true if the emitted buffer ends with a byte-store form
/// `\tmov\tbyte ptr <X>,al\r\n` — the last meaningful write was AL,
/// so a subsequent zero test should use `or al, al` (matches BCC).
fn last_emit_ends_with_byte_store_al(buf: &[u8]) -> bool {
    // Find the last `\r\n` (end of last line) and the one before
    // that (start of last line).
    if !buf.ends_with(b"\r\n") {
        return false;
    }
    let end = buf.len() - 2;
    let line_start = buf[..end]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    let line = &buf[line_start..end];
    line.starts_with(b"\tmov\tbyte ptr ") && line.ends_with(b",al")
}

/// Post-emission peephole: when the LHS materialized as a single
/// `\tmov\tax,word ptr <X>\r\n` and the RHS begins with
/// `\tmov\tbx,word ptr <Y>\r\n` followed by an `<op> ax, word ptr
/// [bx]` line, swap the AX and BX load lines so BX is staged first.
/// BCC's order delays the AX load until immediately before the op,
/// matching how the architecture wants AX live across the smallest
/// window. Conservatively bails on any other shape.
fn hoist_bx_load_above_ax_load(buf: &mut Vec<u8>, pre_pos: usize, mid_pos: usize) {
    // LHS line: `\tmov\tax,word ptr <...>\r\n` (a memory load) or
    // `\tmov\tax,<reg>\r\n` (a register-resident local in SI/DI). The
    // register form must not name BX — that would be a true dependency
    // on the BX load we want to hoist above it. Fixture 4227
    // (`sum + **(pp+2)` with `sum` in DI: BX gets set up first).
    let lhs_line = &buf[pre_pos..mid_pos];
    if !lhs_line.ends_with(b"\r\n") {
        return;
    }
    let lhs_mem = lhs_line.starts_with(b"\tmov\tax,word ptr ");
    let lhs_reg = matches!(
        lhs_line,
        b"\tmov\tax,si\r\n" | b"\tmov\tax,di\r\n",
    );
    if !lhs_mem && !lhs_reg {
        return;
    }
    // RHS must start with `\tmov\tbx,word ptr <...>\r\n`.
    let rhs = &buf[mid_pos..];
    if !rhs.starts_with(b"\tmov\tbx,word ptr ") {
        return;
    }
    let Some(bx_end_rel) = rhs.windows(2).position(|w| w == b"\r\n") else {
        return;
    };
    let bx_end = mid_pos + bx_end_rel + 2;
    // Next line must be the `<op>\tax,word ptr [bx]\r\n` consumption,
    // for one of the commutative-or-RHS-on-bx-safe ops.
    let after_bx = &buf[bx_end..];
    const OPS: &[&[u8]] = &[
        b"\tadd\tax,word ptr [bx]\r\n",
        b"\tsub\tax,word ptr [bx]\r\n",
        b"\tand\tax,word ptr [bx]\r\n",
        b"\tor\tax,word ptr [bx]\r\n",
        b"\txor\tax,word ptr [bx]\r\n",
    ];
    if !OPS.iter().any(|op| after_bx.starts_with(op)) {
        return;
    }
    // Swap the two lines: pull the BX line and reinsert it before AX.
    let lhs_bytes: Vec<u8> = buf[pre_pos..mid_pos].to_vec();
    let bx_bytes: Vec<u8> = buf[mid_pos..bx_end].to_vec();
    buf.drain(pre_pos..bx_end);
    let mut insert_at = pre_pos;
    for &b in &bx_bytes {
        buf.insert(insert_at, b);
        insert_at += 1;
    }
    for &b in &lhs_bytes {
        buf.insert(insert_at, b);
        insert_at += 1;
    }
}

/// Post-emission peephole used by the LHS-first push/pop binop path.
/// If the first line after `\tpush\tax\r\n` is a single
/// `\tmov\tbx,<src>\r\n` (a pointer load that doesn't touch AX),
/// swap it with the push so BCC's "delay the push" emission shape
/// is matched. Walks one line; conservatively bails on anything
/// else.
/// True when the buffer's last emitted line is `<op> ax,<imm>` (a
/// purely-immediate bitwise / additive op against AX). `<op> ax,
/// word ptr [...]` and similar memory operands return false. Used
/// by the compound-assign emitter to pick BCC's AX-route shape only
/// for the `n += x & K` style where the RHS evaluation ends in a
/// bare immediate op. Fixture 2271 (`n += x & 1`).
fn tail_is_ax_imm_op(buf: &[u8]) -> bool {
    if !buf.ends_with(b"\r\n") {
        return false;
    }
    let stripped = &buf[..buf.len() - 2];
    let line_start = stripped
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let line = &stripped[line_start..];
    const PREFIXES: &[&[u8]] = &[
        b"\tand\tax,",
        b"\tor\tax,",
        b"\txor\tax,",
        b"\tadd\tax,",
        b"\tsub\tax,",
    ];
    let Some(prefix) = PREFIXES.iter().find(|p| line.starts_with(*p)) else {
        return false;
    };
    let rhs = &line[prefix.len()..];
    // Only accept a bare decimal/hex immediate (digits, optional
    // leading `-`). Reject `word ptr [...]`, register names like
    // `di`/`si`, etc. — those keep the direct-add shape because
    // BCC does too.
    if rhs.is_empty() {
        return false;
    }
    let after_sign = if rhs[0] == b'-' { &rhs[1..] } else { rhs };
    after_sign.first().is_some_and(|c| c.is_ascii_digit())
}

fn hoist_first_setup_above_push(buf: &mut Vec<u8>, push_pos: usize) {
    const PUSH_LINE: &[u8] = b"\tpush\tax\r\n";
    if buf.len() < push_pos + PUSH_LINE.len() {
        return;
    }
    if &buf[push_pos..push_pos + PUSH_LINE.len()] != PUSH_LINE {
        return;
    }
    let after_push = push_pos + PUSH_LINE.len();
    // Look for the next `\r\n` terminator. The line content is
    // `buf[after_push..line_end]` and ends just before `\r\n`.
    let Some(rel) = buf[after_push..].windows(2).position(|w| w == b"\r\n") else {
        return;
    };
    let line_end = after_push + rel + 2; // include `\r\n`
    let line = &buf[after_push..line_end];
    // Recognize a single `\tmov\tbx,<...>\r\n` line. Anything else
    // (string-literal AX setup, ALU op, byte-load) bails — the
    // line could touch AX or could be content the caller depends
    // on being adjacent to the push.
    if !line.starts_with(b"\tmov\tbx,") {
        return;
    }
    // Swap by moving the line to before the push.
    let line_bytes: Vec<u8> = line.to_vec();
    buf.drain(after_push..line_end);
    // The line has been removed; insert it at push_pos. The push
    // line shifts down by line_bytes.len().
    for (i, &b) in line_bytes.iter().enumerate() {
        buf.insert(push_pos + i, b);
    }
}

/// Try to rewind the `emit_array_addr_to_bx` AX-route 3-instruction
/// shape (`lea ax, word ptr [bp-N]\r\nmov bx, <reg-word>\r\nadd bx,
/// ax\r\n`) at the end of `buf`. Returns the truncate offset if the
/// pattern matches AND the BX source register matches the low-byte
/// register the caller is about to use for the byte value (so the
/// caller can swap to a DX-routed emission). Conservative: bails on
/// any other shape.
fn try_rewind_array_addr_ax_to_dx(
    buf: &[u8],
    base_off: i16,
    low_name: &str,
) -> Option<usize> {
    let word_reg = match low_name {
        "dl" => "dx",
        "bl" => "bx",
        "cl" => "cx",
        _ => return None,
    };
    let lea_line = format!("\tlea\tax,word ptr {}\r\n", bp_addr(base_off));
    let mov_line = format!("\tmov\tbx,{}\r\n", word_reg);
    let add_line = "\tadd\tbx,ax\r\n";
    let total_len = lea_line.len() + mov_line.len() + add_line.len();
    if buf.len() < total_len {
        return None;
    }
    let start = buf.len() - total_len;
    if &buf[start..start + lea_line.len()] != lea_line.as_bytes() {
        return None;
    }
    let mid = start + lea_line.len();
    if &buf[mid..mid + mov_line.len()] != mov_line.as_bytes() {
        return None;
    }
    let tail = mid + mov_line.len();
    if &buf[tail..tail + add_line.len()] != add_line.as_bytes() {
        return None;
    }
    Some(start)
}

/// Same shape as `split_lhs_mem_clobber_tail` but with a register
/// (reg-resident LHS) instead of a memory operand. Detects the
/// 4-instruction tail
///   push ax
///   mov ax, <reg>
///   pop dx
///   <op> ax, dx
/// where `<reg>` is a 16-bit GP register name (si, di, bx, cx,
/// dx, ax, bp). Returns the truncate offset, the reg-name slice
/// bounds, and the op mnemonic. Used by `try_collapse_lhs_clobber
/// _to_dx` to rewrite as `mov dx, <reg>; <op> dx, ax`.
fn split_lhs_reg_clobber_tail(buf: &[u8]) -> Option<(usize, usize, usize, &'static [u8])> {
    const OPS: &[(&[u8], &[u8])] = &[
        (b"\tadd\tax,dx\r\n", b"add"),
        (b"\tsub\tax,dx\r\n", b"sub"),
        (b"\tand\tax,dx\r\n", b"and"),
        (b"\tor\tax,dx\r\n", b"or"),
        (b"\txor\tax,dx\r\n", b"xor"),
    ];
    let (op_mnem, tail_len) = OPS
        .iter()
        .find_map(|(tail, mn)| buf.ends_with(tail).then_some((*mn, tail.len())))?;
    let pop_tail = b"\tpop\tdx\r\n";
    let tail_end = buf.len() - tail_len;
    if !buf[..tail_end].ends_with(pop_tail) {
        return None;
    }
    let mov_end = tail_end - pop_tail.len();
    // Look back for `\tmov\tax,<reg>\r\n` — the reg is some short
    // name of letters (e.g. `si`, `di`, `bx`, `cx`). Locate the
    // `\r\n` preceding the move line.
    let line_search_end = mov_end - 2; // exclude trailing \r\n
    let prev_nl = buf[..line_search_end].iter().rposition(|&b| b == b'\n')?;
    let mov_line_start = prev_nl + 1;
    let mov_line = &buf[mov_line_start..mov_end];
    let prefix = b"\tmov\tax,";
    if !mov_line.starts_with(prefix) || !mov_line.ends_with(b"\r\n") {
        return None;
    }
    let reg_start = mov_line_start + prefix.len();
    let reg_end = mov_end - 2; // before \r\n
    // Reject if the source is a memory operand or immediate — only
    // a bare register identifier (lowercase letters) qualifies.
    if !buf[reg_start..reg_end].iter().all(|&b| b.is_ascii_lowercase()) {
        return None;
    }
    // Verify the prior line is `push ax`.
    let push_line = b"\tpush\tax\r\n";
    if !buf[..mov_line_start].ends_with(push_line) {
        return None;
    }
    let truncate_at = mov_line_start - push_line.len();
    Some((truncate_at, reg_start, reg_end, op_mnem))
}

/// emitted by the rhs_clobbers_ax binop path when the LHS is a simple
/// memory lvalue:
///   push ax
///   mov ax, word ptr <src>
///   pop dx
///   <op> ax, dx
/// Returns `(truncate_at, src_start, src_end, op_mnem)` where:
///   - `truncate_at` is the byte offset of `push ax` (caller truncates here)
///   - `src_start..src_end` slices the `<src>` operand text
///   - `op_mnem` is one of `add|sub|and|or|xor`
fn split_lhs_mem_clobber_tail(buf: &[u8]) -> Option<(usize, usize, usize, &'static [u8])> {
    const OPS: &[(&[u8], &[u8])] = &[
        (b"\tadd\tax,dx\r\n", b"add"),
        (b"\tsub\tax,dx\r\n", b"sub"),
        (b"\tand\tax,dx\r\n", b"and"),
        (b"\tor\tax,dx\r\n",  b"or"),
        (b"\txor\tax,dx\r\n", b"xor"),
    ];
    for (op_tail, op_mnem) in OPS {
        if !buf.ends_with(op_tail) {
            continue;
        }
        let after_pop = buf.len() - op_tail.len();
        const POP: &[u8] = b"\tpop\tdx\r\n";
        if after_pop < POP.len() || &buf[after_pop - POP.len()..after_pop] != POP {
            continue;
        }
        let mov_line_end_with_crlf = after_pop - POP.len();
        const MOV_PREFIX: &[u8] = b"\tmov\tax,word ptr ";
        // mov_line_end_with_crlf is the byte index right after the
        // mov line's trailing `\n`. Walk back to find the previous
        // line break to anchor the start of the mov line.
        let content_end = mov_line_end_with_crlf.checked_sub(2)?; // before \r\n
        let line_start = buf[..content_end]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        if !buf[line_start..].starts_with(MOV_PREFIX) {
            continue;
        }
        let src_start = line_start + MOV_PREFIX.len();
        let src_end = content_end;
        const PUSH: &[u8] = b"\tpush\tax\r\n";
        if line_start < PUSH.len()
            || &buf[line_start - PUSH.len()..line_start] != PUSH
        {
            continue;
        }
        let truncate_at = line_start - PUSH.len();
        return Some((truncate_at, src_start, src_end, op_mnem));
    }
    None
}

/// Given an asm address operand (one of: `DGROUP:_<sym>`,
/// `DGROUP:_<sym>+N`, `[bp-N]`, `[bp+N]`, `[<reg>]`, `[<reg>+N]`),
/// return the same operand shifted by +2 bytes. Used by the long-
/// field member-assign path to derive the high-half address from
/// the low-half address.
fn shift_dest_by_two(dest: &str) -> String {
    // `DGROUP:_<sym>` → `DGROUP:_<sym>+2`
    // `DGROUP:_<sym>+N` → `DGROUP:_<sym>+(N+2)`
    if let Some(rest) = dest.strip_prefix("DGROUP:_") {
        if let Some((sym, off)) = rest.split_once('+') {
            let n: i32 = off.parse().expect("global offset is integer");
            return format!("DGROUP:_{sym}+{}", n + 2);
        }
        return format!("DGROUP:_{rest}+2");
    }
    // `[bp-N]` → `[bp-(N-2)]` (less negative); `[bp+N]` → `[bp+(N+2)]`.
    if let Some(rest) = dest.strip_prefix("[bp") {
        let body = rest.strip_suffix(']').expect("malformed bp-relative dest");
        let n: i32 = body.parse().expect("bp offset is integer");
        let shifted = n + 2;
        return if shifted < 0 {
            format!("[bp{shifted}]")
        } else if shifted == 0 {
            "[bp]".to_owned()
        } else {
            format!("[bp+{shifted}]")
        };
    }
    // `[<reg>]` → `[<reg>+2]`; `[<reg>+N]` → `[<reg>+(N+2)]`.
    if let Some(inside) = dest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if let Some((reg, off)) = inside.split_once('+') {
            let n: i32 = off.parse().expect("reg-indirect offset is integer");
            return format!("[{reg}+{}]", n + 2);
        }
        return format!("[{inside}+2]");
    }
    panic!("shift_dest_by_two: unsupported dest form `{dest}`");
}
use plan::{LabelPlan, SwitchStrategy};

/// Emit the per-function chunk of an `-S` file for one function.
pub fn emit_function(
    out: &mut Vec<u8>,
    source: &str,
    function: &Function,
    func_idx: u32,
    signatures: &Signatures,
    globals: &GlobalTable,
    strings: &mut StringPool,
    helpers: &mut std::collections::HashSet<String>,
    target_186: bool,
    stack_check: bool,
    no_reg_vars: bool,
    model_has_far_code: bool,
    model_is_huge: bool,
) {
    let mut emitter = FunctionEmitter::new_with_opts(
        out, source, function, func_idx, signatures, globals, strings, helpers,
        no_reg_vars,
    );
    emitter.target_186 = target_186;
    emitter.stack_check = stack_check;
    emitter.model_has_far_code = model_has_far_code;
    emitter.model_is_huge = model_is_huge;
    emitter.run();
}

/// What BCC prepends to a C symbol when emitting it in the small memory
/// model.
pub fn function_symbol(name: &str) -> String {
    format!("_{name}")
}

/// Pascal-convention function symbol: uppercase, no leading
/// underscore. Fixture 1653 (`int pascal add(...)` → public `ADD`).
pub fn function_symbol_pascal(name: &str) -> String {
    name.to_uppercase()
}

struct FunctionEmitter<'a> {
    out: &'a mut Vec<u8>,
    source: &'a str,
    function: &'a Function,
    func_idx: u32,
    lines: LineMap,
    /// 1-based source line of the last comment we emitted, or 0 if we
    /// haven't emitted any comment yet for this function.
    current_line: u32,
    locals: Locals,
    label_plan: LabelPlan,
    signatures: &'a Signatures,
    globals: &'a GlobalTable,
    strings: &'a mut StringPool,
    /// Stack of enclosing loop targets so `break;` / `continue;`
    /// statements can look up their jump destination. The innermost
    /// loop sits at the top (index `len()-1`).
    loop_stack: Vec<LoopTargets>,
    /// Data labels emitted between `_main endp` and `?debug C E9`,
    /// staged here while the function body is being emitted. Used by
    /// the jump-table and linear-search switch strategies, both of
    /// which need a `@<func>@C<num> label word / dw / db` block after
    /// the function ends. Empty for most functions.
    post_function_data: Vec<u8>,
    /// Runtime-helper symbols this function references (e.g.
    /// `N_LXLSH@` for long left-shift). Shared across all functions
    /// in the TU so the tail-emitter can declare each one once and
    /// merge them into the publics ordering. Fixture 228.
    helpers: &'a mut std::collections::HashSet<String>,
    /// When true, `emit_widen_al` is a no-op. Set during char-return
    /// value emission since the ABI leaves AH garbage — the caller
    /// widens with `cbw` after the call.
    skip_widen: bool,
    /// When true, the int-Mod emission in `emit_op_with_source` skips
    /// the trailing `mov ax, dx`. The caller then reads the remainder
    /// directly from DX (e.g. to store via `mov [mem], dx`).
    skip_mod_to_ax: bool,
    /// True when an `fstp` to memory has been emitted earlier in the
    /// function and we haven't yet emitted the matching synchronizing
    /// `fwait` before a CPU memory access. The int→FPU widening path
    /// in `emit_float_load_to_fpu` (mov ax / mov scratch / fild)
    /// prepends a bare `fwait` when this is set so the prior fstp's
    /// memory write completes before the CPU mov runs. Fixture 1752.
    pending_fpu_store_fwait: bool,
    /// True while evaluating a function-call argument expression.
    /// BCC's char `*p++` deferred-inc peephole fires only in non-arg
    /// contexts; in arg eval, the save-to-BX pattern is kept (the
    /// consumer is a `push`/`call`, not a reg-to-reg move).
    in_arg_expr: bool,
    /// Deferred post-update from a `*p++` / `*p--` of a char pointer.
    /// BCC emits the read first, then the consumer of the loaded
    /// byte, and finally the inc/dec. We capture the register name,
    /// stride, and mnemonic here; the next statement boundary flushes
    /// the pending update (so the inc lands AFTER the consumer
    /// instruction). Fixture 2000.
    pending_post_update: Option<(String, u8, &'static str)>,
    /// `-1` / `-2` flag: target the 80186 (or 80286) instruction set.
    /// Enables `enter`/`leave` for the prologue/epilogue and the
    /// `shl r16, imm8` form for multi-bit shifts. Fixtures 2134,
    /// 2276, 2277.
    target_186: bool,
    /// `-N` flag: emit stack-overflow check after the prologue.
    /// Compares `___brklvl` against `sp`; calls `N_OVERFLOW@` if
    /// sp has dropped at or below the break level. Fixture 2129.
    stack_check: bool,
    /// True under medium / large / huge memory models — code is
    /// far, so calls to *unknown* names (externs) must use the
    /// far-call form (`call far ptr` → `9A` opcode + 4-byte
    /// segment:offset). Same-TU calls already pick up the
    /// `push cs; call near` shape via per-function `is_far`.
    /// Fixture 2210.
    model_has_far_code: bool,
    /// True under the huge memory model. Each module has its own
    /// data segment (no DGROUP group), so functions reload DS from
    /// the segment of `HELLO_DATA` on entry and restore it on exit;
    /// globals are referenced without the `DGROUP:` prefix.
    /// Fixtures 1770, 2057.
    model_is_huge: bool,
    /// Set by `emit_compare` when it emits an operand-swapped cmp
    /// (e.g. `cmp ax, <reg>` when BCC wants `cmp <reg>, <ax-side>`
    /// reordered). The outer Jcc selector inverts the mnemonic
    /// pair (`jb`/`jae` → `ja`/`jbe`, etc.) so the branch
    /// semantics match. Fixture 1814.
    cmp_swapped: bool,
    /// Bytes reserved below the regular locals frame for the hidden
    /// temporary buffer that an N_SCOPY@-routed struct-returning call
    /// (size ∉ {1, 2, 4}) needs as its caller-supplied result slot.
    /// One slot per function sized to the largest such return type;
    /// the slot's bp-relative top sits at
    /// `-(locals.stack_bytes() + struct_call_tmp_bytes)`. Zero when
    /// the function makes no qualifying calls. Fixtures 1685, 1877,
    /// 2207, 2352.
    struct_call_tmp_bytes: u16,
    /// When `Some(off)`, the next `emit_call` pushes the hidden far
    /// pointer to `[bp + off]` as the final pre-call push (so the
    /// callee sees it at `[bp+4..7]` after its own `push bp; mov bp,
    /// sp`), and the cleanup includes the extra 4 bytes. Used to
    /// route struct-returning calls (size ∉ {1, 2, 4}) through the
    /// SCOPY@ pattern.
    pending_hidden_ret_ptr_tmp_off: Option<i16>,
}

/// Innermost enclosing construct that catches `break;` (and maybe
/// `continue;`). Pushed for `while` / `do-while` / `for` / `switch`.
/// For switches, `continue_target_slot` is `None` — a `continue;` in
/// a switch body threads past the switch to the enclosing loop.
#[derive(Clone, Copy)]
struct LoopTargets {
    break_target_slot: u32,
    continue_target_slot: Option<u32>,
}

// FunctionEmitter methods are split across concern modules;
// each holds an `impl<'a> FunctionEmitter<'a>` block.
// See scripts/codegen_split/ for the mechanical split.
mod emitter_arrays_1;
mod emitter_arrays_2;
mod emitter_assign_1;
mod emitter_assign_2;
mod emitter_assign_3;
mod emitter_bitfields;
mod emitter_classify;
mod emitter_conditions;
mod emitter_emit_core_1;
mod emitter_emit_core_2;
mod emitter_emit_core_3;
mod emitter_expressions;
mod emitter_lvalues;
mod emitter_members;
mod emitter_statements;

/// Does `body` contain a `break;` that targets the enclosing loop?
/// Stops at nested loops — a `break;` inside an inner `while`/`for`
/// targets the inner loop, not the outer one.
/// Largest hidden-tmp size needed for any N_SCOPY@-routed struct-
/// returning call (size ∉ {1, 2, 4}) reachable from `function`'s
/// body. One slot per function covers all such calls. Rounded up to
/// an even byte count so the tmp's top stays word-aligned.
fn compute_struct_call_tmp_bytes(
    function: &Function,
    signatures: &Signatures,
) -> u16 {
    fn visit_expr(e: &Expr, sigs: &Signatures, max: &mut u16) {
        if let ExprKind::Call { name, args } = &e.kind {
            if let Some(ret_ty) = sigs.ret_ty_of(name)
                && matches!(ret_ty, Type::Struct { .. })
            {
                let sz = ret_ty.size_bytes();
                if sz != 1 && sz != 2 && sz != 4 {
                    let aligned = (sz + 1) & !1;
                    if aligned > *max {
                        *max = aligned;
                    }
                }
            }
            for arg in args {
                visit_expr(arg, sigs, max);
            }
            return;
        }
        match &e.kind {
            ExprKind::BinOp { left, right, .. }
            | ExprKind::Logical { left, right, .. }
            | ExprKind::Comma { left, right } => {
                visit_expr(left, sigs, max);
                visit_expr(right, sigs, max);
            }
            ExprKind::Unary { operand, .. }
            | ExprKind::Deref(operand)
            | ExprKind::Cast { operand, .. } => visit_expr(operand, sigs, max),
            ExprKind::Member { base, .. } => visit_expr(base, sigs, max),
            ExprKind::ArrayIndex { array, index } => {
                visit_expr(array, sigs, max);
                visit_expr(index, sigs, max);
            }
            ExprKind::Ternary { cond, then_value, else_value } => {
                visit_expr(cond, sigs, max);
                visit_expr(then_value, sigs, max);
                visit_expr(else_value, sigs, max);
            }
            ExprKind::AssignExpr { value, .. }
            | ExprKind::CompoundAssignExpr { value, .. } => {
                visit_expr(value, sigs, max);
            }
            ExprKind::AssignLvalueExpr { target, value } => {
                visit_expr(target, sigs, max);
                visit_expr(value, sigs, max);
            }
            ExprKind::UpdateLvalue { target, .. } => visit_expr(target, sigs, max),
            ExprKind::InitList { items } => {
                for it in items {
                    visit_expr(it, sigs, max);
                }
            }
            ExprKind::CallVia { addr, args } => {
                visit_expr(addr, sigs, max);
                for arg in args {
                    visit_expr(arg, sigs, max);
                }
            }
            _ => {}
        }
    }
    fn visit_stmt(s: &Stmt, sigs: &Signatures, max: &mut u16) {
        match &s.kind {
            StmtKind::Return(Some(e))
            | StmtKind::ExprStmt(e)
            | StmtKind::Assign { value: e, .. }
            | StmtKind::CompoundAssign { value: e, .. }
            | StmtKind::DerefAssign { value: e, .. }
            | StmtKind::DerefCompoundAssign { value: e, .. }
            | StmtKind::MemberAssign { value: e, .. }
            | StmtKind::MemberCompoundAssign { value: e, .. } => visit_expr(e, sigs, max),
            StmtKind::Return(None) | StmtKind::Empty | StmtKind::Break
            | StmtKind::Continue | StmtKind::Goto { .. } | StmtKind::Label { .. }
            | StmtKind::Asm { .. } => {}
            StmtKind::Declare { init, .. } => {
                if let Some(i) = init {
                    visit_expr(i, sigs, max);
                }
            }
            StmtKind::If { cond, then_branch, else_branch } => {
                visit_expr(cond, sigs, max);
                for s in then_branch { visit_stmt(s, sigs, max); }
                if let Some(eb) = else_branch {
                    for s in eb { visit_stmt(s, sigs, max); }
                }
            }
            StmtKind::While { cond, body } | StmtKind::DoWhile { body, cond } => {
                visit_expr(cond, sigs, max);
                for s in body { visit_stmt(s, sigs, max); }
            }
            StmtKind::For { init, cond, step, body } => {
                if let Some(es) = init { for e in es { visit_expr(e, sigs, max); } }
                if let Some(c) = cond { visit_expr(c, sigs, max); }
                if let Some(es) = step { for e in es { visit_expr(e, sigs, max); } }
                for s in body { visit_stmt(s, sigs, max); }
            }
            StmtKind::Switch { scrutinee, cases } => {
                visit_expr(scrutinee, sigs, max);
                for c in cases {
                    for s in &c.body { visit_stmt(s, sigs, max); }
                }
            }
            StmtKind::ArrayAssign { indices, value, .. }
            | StmtKind::ArrayCompoundAssign { indices, value, .. }
            | StmtKind::MemberArrayAssign { indices, value, .. } => {
                for i in indices { visit_expr(i, sigs, max); }
                visit_expr(value, sigs, max);
            }
            StmtKind::Block(body) => {
                for s in body { visit_stmt(s, sigs, max); }
            }
        }
    }
    let mut max: u16 = 0;
    for s in function.body.as_deref().unwrap_or(&[]) {
        visit_stmt(s, signatures, &mut max);
    }
    max
}

/// Peel any number of explicit `(T) ...` casts off the outside of an
/// expression. Returns the inner expression (or the original if no
/// cast was present). Used by far-pointer init / assign codegen
/// where `(int far *)&x` semantically *is* `&x` once the FarPointer
/// route is selected.
fn strip_cast(e: &Expr) -> Option<&Expr> {
    let mut cur = e;
    let mut stripped = false;
    while let ExprKind::Cast { operand, .. } = &cur.kind {
        cur = operand;
        stripped = true;
    }
    if stripped { Some(cur) } else { Some(e) }
}

/// Match the canonical "combine two int halves into a long" idiom:
/// `((long)<hi> << 16) | (long)(unsigned int)<lo>`. Returns
/// `(hi_name, lo_name, lo_was_unsigned_cast)` or `None`. The
/// outer-OR's operands may appear in either order. Peels any
/// chain of explicit Cast wrappers — `((long)hi << 16)` against
/// `(long)(unsigned int)lo` (the typical signed-hi / unsigned-lo
/// shape, fixture 1946) and the symmetric all-unsigned variant.
fn match_combine_long_idiom(e: &Expr) -> Option<(String, String, bool)> {
    let ExprKind::BinOp { op: BinOp::BitOr, left, right } = &e.kind else {
        return None;
    };
    let try_shape = |hi_side: &Expr, lo_side: &Expr| -> Option<(String, String, bool)> {
        let hi_inner = strip_cast(hi_side)?;
        let ExprKind::BinOp { op: BinOp::Shl, left: shl_l, right: shl_r } = &hi_inner.kind
            else { return None };
        if !matches!(&shl_r.kind, ExprKind::IntLit(16)) { return None; }
        let hi_operand = strip_cast(shl_l)?;
        let ExprKind::Ident(hi_name) = &hi_operand.kind else { return None };
        let lo_inner = strip_cast(lo_side)?;
        // The lo side might be `(long)(unsigned int)<lo>` or just
        // `(long)<lo>` — peel any remaining cast layers.
        let (lo_operand, lo_unsigned) = match &lo_inner.kind {
            ExprKind::Cast { ty: Type::UInt, operand } => (strip_cast(operand)?, true),
            _ => (lo_inner, false),
        };
        let ExprKind::Ident(lo_name) = &lo_operand.kind else { return None };
        Some((hi_name.clone(), lo_name.clone(), lo_unsigned))
    };
    try_shape(left, right).or_else(|| try_shape(right, left))
}

/// True when `scrutinee` is a bare Ident whose static type is
/// long-like (signed or unsigned). Routes long-scrutinee switches
/// through the dedicated `LongLinearSearch` dispatch. Fixture 1913.
fn scrutinee_is_long_typed(
    scrutinee: &Expr,
    locals: &crate::codegen::locals::Locals,
    globals: &GlobalTable,
) -> bool {
    let ExprKind::Ident(name) = &scrutinee.kind else { return false };
    if locals.has(name) {
        return locals.type_of(name).is_long_like();
    }
    if let Some(t) = globals.type_of(name) {
        return t.is_long_like();
    }
    false
}

/// Tighten asm whitespace to BCC's tab-and-no-comma-space style.
/// The first whitespace run after the mnemonic becomes a tab; a
/// space immediately following a comma is dropped (so `mov ax, 42`
/// becomes `mov\tax,42`). Spaces *within* an operand — e.g. between
/// `word` and `ptr`, or `ptr` and `[` — are preserved verbatim.
/// Recognize the asm-side pseudo-register names `_AX` / `_BX` /
/// `_CX` / `_DX` / `_SI` / `_DI` / `_BP` / `_SP` (and their byte
/// halves). These resolve to the live CPU register inside an
/// inline-asm block; outside of asm they appear in `return _AX;`
/// and similar patterns where the asm body left a value live in
/// the named register. Fixture 2122.
pub(crate) fn is_asm_pseudo_register(name: &str) -> bool {
    matches!(
        name,
        "_AX" | "_BX" | "_CX" | "_DX"
            | "_AL" | "_AH" | "_BL" | "_BH" | "_CL" | "_CH" | "_DL" | "_DH"
            | "_SI" | "_DI" | "_BP" | "_SP"
            | "_ES" | "_CS" | "_SS" | "_DS"
            | "_FLAGS"
    )
}

/// Lowercase asm operand for a pseudo-register identifier. Returns
/// `None` if `name` is not a pseudo-register, or if it's `_FLAGS`
/// (which has no direct mov-target form — callers must route flag
/// reads through `pushf`).
fn pseudo_register_operand(name: &str) -> Option<&'static str> {
    Some(match name {
        "_AX" => "ax", "_BX" => "bx", "_CX" => "cx", "_DX" => "dx",
        "_AL" => "al", "_AH" => "ah",
        "_BL" => "bl", "_BH" => "bh",
        "_CL" => "cl", "_CH" => "ch",
        "_DL" => "dl", "_DH" => "dh",
        "_SI" => "si", "_DI" => "di", "_BP" => "bp", "_SP" => "sp",
        "_ES" => "es", "_CS" => "cs", "_SS" => "ss", "_DS" => "ds",
        _ => return None,
    })
}

/// True for the 8-bit pseudo-registers (`_AL`, `_AH`, ..., `_DH`).
/// Used when narrowing immediate values for byte-width stores.
fn is_byte_pseudo_register(name: &str) -> bool {
    matches!(name, "_AL" | "_AH" | "_BL" | "_BH" | "_CL" | "_CH" | "_DL" | "_DH")
}

/// Recognize `_FLAGS & <flag-bit>` (and the singly-negated form
/// `!(_FLAGS & <flag-bit>)`) as the skip-then / take-then mnemonic
/// pair for a direct conditional jump. Returns `None` if the shape
/// doesn't match or the bit is one of the non-jumpable flags
/// (AF / TF / IF / DF). Fixtures 4055, 4057–4061.
fn flags_bit_test_mnemonics(cond: &Expr) -> Option<(&'static str, &'static str)> {
    let (inner, negated) = match &cond.kind {
        ExprKind::Unary { op: UnaryOp::Not, operand } => (operand.as_ref(), true),
        _ => (cond, false),
    };
    let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &inner.kind else {
        return None;
    };
    // Accept either operand order: `_FLAGS & K` or `K & _FLAGS`.
    let bit = match (&left.kind, &right.kind) {
        (ExprKind::PseudoReg(n), _) if n == "_FLAGS" => try_const_eval(right)?,
        (_, ExprKind::PseudoReg(n)) if n == "_FLAGS" => try_const_eval(left)?,
        _ => return None,
    };
    let (skip, take) = match bit & 0xFFFF {
        0x0001 => ("jnc", "jc"),   // CF (carry)
        0x0004 => ("jnp", "jp"),   // PF (parity)
        0x0040 => ("jne", "je"),   // ZF (zero) — BCC's listing uses jne/je
        0x0080 => ("jns", "js"),   // SF (sign)
        0x0800 => ("jno", "jo"),   // OF (overflow)
        _ => return None,
    };
    Some(if negated { (take, skip) } else { (skip, take) })
}

fn normalize_asm_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut saw_mnemonic_end = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if (c == ' ' || c == '\t') && !saw_mnemonic_end {
            // First whitespace run after the mnemonic — emit a
            // single tab, then skip any trailing whitespace.
            out.push('\t');
            saw_mnemonic_end = true;
            i += 1;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
        // After emitting a comma, drop any immediately-following
        // whitespace.
        if c == ',' {
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
        }
    }
    out
}

/// Format a disp8 as a signed string suffix — `+5` / `-3` / `+0`.
/// Used for the `[bp+si{}]` addressing mode in the BP+SI char-
/// array peephole. Fixture 2488.
fn signed_disp_suffix(disp: i8) -> String {
    if disp >= 0 {
        format!("+{disp}")
    } else {
        format!("{disp}")
    }
}

fn body_has_return(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_return)
}

fn stmt_has_return(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(_) => true,
        StmtKind::If { then_branch, else_branch, .. } => {
            body_has_return(then_branch)
                || else_branch.as_ref().is_some_and(|b| body_has_return(b))
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
            body_has_return(body)
        }
        StmtKind::For { body, .. } => body_has_return(body),
        StmtKind::Switch { cases, .. } => {
            cases.iter().any(|c| body_has_return(&c.body))
        }
        _ => false,
    }
}

fn body_has_break(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_break)
}

fn stmt_has_break(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Break => true,
        StmtKind::If { then_branch, else_branch, .. } => {
            body_has_break(then_branch)
                || else_branch.as_ref().is_some_and(|b| body_has_break(b))
        }
        // Nested loops AND nested switches shadow `break;` — they
        // consume any break in their body, so the enclosing loop
        // doesn't see it.
        _ => false,
    }
}

fn body_has_continue(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_continue)
}

fn stmt_has_continue(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Continue => true,
        StmtKind::If { then_branch, else_branch, .. } => {
            body_has_continue(then_branch)
                || else_branch.as_ref().is_some_and(|b| body_has_continue(b))
        }
        // A switch does NOT consume `continue;` — the inner continue
        // threads past it to the enclosing loop, so we have to look
        // inside the case bodies.
        StmtKind::Switch { cases, .. } => {
            cases.iter().any(|c| body_has_continue(&c.body))
        }
        _ => false,
    }
}

/// Compute the `C<num>` suffix of the data-table label BCC uses for
/// a jump-table or linear-search switch. The formulas below are
/// **empirical fits** through our captured fixtures — they pin the
/// labels for 073 (jump-table, 8 cases), 076 (jump-table, 4 cases)
/// and 074 (linear-search, 4 cases), but we don't yet understand
/// what determines the constants `508` and `442`, or whether they
/// vary with anything other than `case_count` (e.g. function
/// position, function size, surrounding constants). _Fingerprint
/// open question; see `specs/FINGERPRINTS.md`._
fn switch_c_num(strategy: SwitchStrategy, case_count: u32) -> u32 {
    match strategy {
        SwitchStrategy::JumpTable => 92 * case_count + 508,
        SwitchStrategy::LinearSearch => 74 * case_count + 442,
        // Long-linear-search has a longer per-iteration loop body
        // (compare-both-halves) and a 3-N-word data table; the
        // exact constants are observation-pending — for now use a
        // placeholder formula. The label name doesn't reach the
        // OBJ (it's resolved at assembly time), so any unique
        // value works for byte-exactness. Fixture 1913.
        SwitchStrategy::LongLinearSearch => 100 * case_count + 500,
        SwitchStrategy::Chained => unreachable!(
            "chained-compare switch has no data label and no `C<num>` to compute"
        ),
    }
}

/// Width keyword for a `mov ptr [bp-N], K` store of the given type:
/// `"byte"` for `char` (and char arrays), `"word"` for `int`,
/// pointers, and int arrays. Currently used only by initialization
/// of stack-resident locals.
/// The (low-half, high-half) mnemonic pair for long-to-long arithmetic
/// or bitwise ops. Add/Sub propagate carry/borrow into the high half
/// (`adc`/`sbb`); bitwise ops act independently per half so high uses
/// the same mnemonic as low.
fn long_pair_op(op: BinOp) -> Option<(&'static str, &'static str)> {
    match op {
        BinOp::Add => Some(("add", "adc")),
        BinOp::Sub => Some(("sub", "sbb")),
        BinOp::BitAnd => Some(("and", "and")),
        BinOp::BitOr => Some(("or", "or")),
        BinOp::BitXor => Some(("xor", "xor")),
        _ => None,
    }
}

fn ptr_width(ty: &Type) -> &'static str {
    if ty.size_bytes() == 1 { "byte" } else { "word" }
}

fn is_reg16_name(s: &str) -> bool {
    matches!(s, "ax" | "bx" | "cx" | "dx" | "si" | "di" | "bp" | "sp")
}

/// Walk a Member chain rooted at an Ident pointer (multiple arrow
/// derefs allowed), producing the sequence of intermediate deref
/// offsets (each one becomes a `mov bx, [bx+disp]`) and the final
/// field offset + leaf type for the trailing read. Returns `None`
/// if the chain doesn't bottom out at an Ident-rooted Arrow.
/// Fixture 3448 (`o->m.p->c`).
fn multi_arrow_chain(
    base: &Expr,
    field: &str,
    ident_pointee: impl Fn(&str) -> Option<Type>,
) -> Option<(String, Vec<i32>, i32, Type)> {
    // Group the path into "deref groups". The outer Arrow's field
    // (and any Dots that follow the same arrow's deref) belongs to
    // the leaf read. Every Dot encountered before the next inner
    // Arrow accumulates into THAT arrow's step (because those Dots
    // apply to the inner Arrow's deref result before the outer
    // Arrow's deref happens).
    //
    // Example: `o->m.p->c`
    //   Outer Arrow field = "c" → leaf group = ["c"]
    //   Walk inward through Dot.p → push "p" into the NEXT (deeper)
    //   group. Walk into Arrow{o, "m"}: that arrow's own field "m"
    //   joins the current pending group → [["p","m"]] (root step).
    //
    // groups_outer_first = [["c"], ["p","m"]], root = o.
    let mut groups_outer_first: Vec<Vec<&str>> = vec![vec![field]];
    let mut pending: Vec<&str> = Vec::new();
    let mut cur = base;
    let root_name: String;
    loop {
        match &cur.kind {
            ExprKind::Member { base: inner, field: f, kind } => match kind {
                crate::ast::MemberKind::Dot => {
                    // Dot fields encountered between arrows belong
                    // to the NEXT (inner) arrow's deref-step.
                    pending.push(f.as_str());
                    cur = inner;
                }
                crate::ast::MemberKind::Arrow => {
                    // This arrow closes off a deref step. Its own
                    // field joins the pending dots.
                    let mut step = std::mem::take(&mut pending);
                    step.push(f.as_str());
                    groups_outer_first.push(step);
                    if let ExprKind::Ident(name) = &inner.kind {
                        root_name = name.clone();
                        break;
                    }
                    cur = inner;
                }
            },
            _ => return None,
        }
    }
    let root_pointee = ident_pointee(&root_name)?;
    // Process groups in reverse (innermost first → that's the one
    // closest to the root).
    let mut step_offsets: Vec<i32> = Vec::new();
    let mut ty: Type = root_pointee;
    let mut leaf_off: i32 = 0;
    let mut leaf_ty: Type = Type::Int;
    for (idx, group) in groups_outer_first.iter().enumerate().rev() {
        // Within a group, the inner Dot/Arrow field is at the end
        // of the vec (we pushed outer-first). Walk in reverse (inner
        // first) to apply the field accesses to `ty`.
        let mut off: i32 = 0;
        for df in group.iter().rev() {
            let (f_off, f_ty) = ty.field(df)?;
            off = off.checked_add(i32::from(f_off))?;
            ty = f_ty;
        }
        if idx == 0 {
            // Outermost group — the trailing field read.
            leaf_off = off;
            leaf_ty = ty.clone();
        } else {
            // Inner group — its accumulated offset becomes the
            // deref step. The pointer-typed result is the base of
            // the next outer group.
            step_offsets.push(off);
            let pointee = ty.pointee()?.clone();
            ty = pointee;
        }
    }
    Some((root_name, step_offsets, leaf_off, leaf_ty))
}

/// Walk a deref expression chain (`*p` → `(p, 0)`, `**p` → `(p, 1)`,
/// `***p` → `(p, 2)`) and return the base ident name + the count of
/// visible `*`s. The caller's implicit outer `*` (the one applied
/// when reading/writing) is not counted. Used by both
/// `emit_deref_to_ax` (read) and `emit_deref_assign` (write) so the
/// chain prefix is shared.
fn deref_chain_root(ptr: &Expr) -> (&str, u32) {
    let mut depth = 0u32;
    let mut cur = ptr;
    let name = loop {
        match &cur.kind {
            ExprKind::Deref(inner) => {
                depth += 1;
                cur = inner;
            }
            // Pointer cast `(T *)p` is a pure type-level operation
            // — bytes are identical to p. Skip through to the
            // underlying ident so `*(char *)p` resolves like `*p`.
            // Fixture 3163.
            ExprKind::Cast { ty, operand }
                if matches!(ty, crate::ast::Type::Pointer(_)) =>
            {
                let _ = ty;
                cur = operand;
            }
            ExprKind::Ident(name) => break name.as_str(),
            _ => panic!(
                "non-ident base in deref chain not yet supported (no fixture for {:?})",
                cur.kind
            ),
        }
    };
    (name, depth)
}

/// Walk an array-type chain against an index list, summing
/// `stride * k` for each subscript when every index is a compile-time
/// constant. Returns `(byte_offset, leaf_type)`. `None` if any index
/// is non-constant or the type chain stops being `Type::Array` before
/// all indices are consumed. Used by both array-read and array-assign
/// codegen to fold `a[1][2]` into a single `[bp-N]` operand.
fn try_const_array_offset<'a, I>(array_ty: &Type, indices: I) -> Option<(i32, Type)>
where
    I: IntoIterator<Item = &'a Expr>,
{
    let mut ty = array_ty.clone();
    let mut off: i32 = 0;
    for ix in indices {
        let k = try_const_eval(ix)? as i32;
        let Type::Array { elem, .. } = &ty else { return None };
        let stride = i32::from(elem.size_bytes());
        off = off.checked_add(k.checked_mul(stride)?)?;
        ty = (**elem).clone();
    }
    Some((off, ty))
}

/// One step in a long-pair-op chain. The first entry holds the
/// root operand (with empty ops); subsequent entries hold each
/// RHS to op-against. Used by `collect_long_lvalue_chain`.
struct LongChainStep {
    lo_op: &'static str,
    hi_op: &'static str,
    hi: String,
    lo: String,
}

/// Classification of a variable-indexed array's base name, used by
/// the var-idx-array RHS peephole to dispatch the address-into-BX
/// computation.
#[derive(Debug, Clone, Copy)]
enum VarIdxKind {
    /// Stack array at bp-offset `base_off`, element size `elem_sz`.
    StackArr(i16, u16),
    /// Int* local (reg or stack — codegen picks the variant).
    PtrInt,
    /// Global array — uses `<sym>[bx]` indexed memory.
    GlobalArr,
}

/// A resolved right-hand operand.
enum OperandSource {
    Immediate(u32),
    /// Stack-resident local or param at a (signed) bp offset.
    Local(i16),
    Reg(Reg),
    /// File-scope variable — addressed as `<width> ptr DGROUP:_<name>`.
    /// Fixture 087: `add ax, word ptr DGROUP:_b`.
    Global(String),
    /// File-scope array element at a compile-time offset:
    /// `<width> ptr DGROUP:_<name>+<offset>`. Fixture 189 uses
    /// `add ax, word ptr DGROUP:_a+2` for `a[1]`.
    GlobalOffset { name: String, offset: i32 },
    /// The link-time ADDRESS of a file-scope array element, used as a
    /// symbolic immediate: `offset DGROUP:_<name>+<offset>`. Distinct
    /// from `GlobalOffset` (which reads memory `word ptr ...`): this is
    /// the pointer value `<arr> + K`, not its contents. Fixture 4226
    /// (`cmp si,offset DGROUP:_a+12` for `p < a + 6`).
    GlobalAddr { name: String, offset: i32 },
    /// `*p` where `p` is a register-resident local pointer —
    /// addressed as `<width> ptr [<reg>]`. Fixture 201:
    /// `sub ax,word ptr [si]` for `10 - *p` with `p` in SI.
    DerefReg(Reg),
    /// `p[K]` where `p` is a register-resident local pointer and
    /// `K` is constant — addressed as `<width> ptr [<reg>+<off>]`.
    /// Fixture 1472 (`sum`: `add ax, [si+2]` for `p[1]`).
    DerefRegOffset { reg: Reg, offset: i16 },
    /// Two-register `<width> ptr [<base>+<index>+<off>]` — used for
    /// `<reg-ptr>-><array-field>[i]` where the scaled index sits in
    /// `base` (BX) and the enregistered pointer in `index` (SI). The
    /// caller emits the index scaling before reading the operand.
    /// Probe fixture 9001 (`s->a[i] + s->a[i]`).
    TwoRegOffset { base: Reg, index: Reg, offset: i16 },
    /// Operand value has already been emitted into AX. Used when the
    /// resolver had to compute a non-trivial address (e.g. `lea ax,
    /// [bp+N]` for `<local_arr> + <const>`). Fixture 1814.
    Ax,
}

impl OperandSource {
    /// Format as a 16-bit source operand.
    fn word(&self) -> String {
        match self {
            // try_const_eval returns u32 with negative values
            // sign-extended (e.g. -2 → 0xFFFFFFFE). Mask to 16 bits
            // so the emitted immediate fits the instruction.
            Self::Immediate(v) => (v & 0xFFFF).to_string(),
            Self::Local(off) => format!("word ptr {}", bp_addr(*off)),
            Self::Reg(r) => r.name().to_owned(),
            Self::Global(name) => format!("word ptr DGROUP:_{name}"),
            Self::GlobalOffset { name, offset } => {
                if *offset == 0 {
                    format!("word ptr DGROUP:_{name}")
                } else {
                    format!("word ptr DGROUP:_{name}+{offset}")
                }
            }
            Self::GlobalAddr { name, offset } => {
                if *offset == 0 {
                    format!("offset DGROUP:_{name}")
                } else {
                    format!("offset DGROUP:_{name}+{offset}")
                }
            }
            Self::DerefReg(r) => format!("word ptr [{}]", r.name()),
            Self::DerefRegOffset { reg, offset } => {
                if *offset == 0 {
                    format!("word ptr [{}]", reg.name())
                } else if *offset > 0 {
                    format!("word ptr [{}+{}]", reg.name(), offset)
                } else {
                    format!("word ptr [{}-{}]", reg.name(), -offset)
                }
            }
            Self::TwoRegOffset { base, index, offset } => {
                format!("word ptr {}", two_reg_addr(*base, *index, i32::from(*offset)))
            }
            Self::Ax => "ax".to_owned(),
        }
    }

    /// Byte form, used for shift counts (`mov cl, byte ptr ...`).
    fn byte(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("byte ptr {}", bp_addr(*off)),
            Self::Global(name) => format!("byte ptr DGROUP:_{name}"),
            Self::GlobalOffset { name, offset } => {
                if *offset == 0 {
                    format!("byte ptr DGROUP:_{name}")
                } else {
                    format!("byte ptr DGROUP:_{name}+{offset}")
                }
            }
            // A register holding an int provides the low byte via
            // its `*L` half (BX → BL, CX → CL, DX → DL). SI and DI
            // don't have addressable byte halves; route those
            // through CX with an explicit `mov cx, si; mov cl,
            // cl`... actually BCC just panics for those — only
            // BX/CX/DX work directly. Fixture 2399 (`1 << i` with
            // i in DX → `mov cl, dl; shl ax, cl`).
            Self::Reg(r) => match r.name() {
                "bx" => "bl".to_owned(),
                "cx" => "cl".to_owned(),
                "dx" => "dl".to_owned(),
                _ => panic!(
                    "shift count from register `{}` not yet supported (no byte half)",
                    r.name()
                ),
            },
            Self::GlobalAddr { .. } => {
                panic!("symbolic-address operand has no byte form (pointer is 2 bytes)")
            }
            Self::DerefReg(r) => format!("byte ptr [{}]", r.name()),
            Self::DerefRegOffset { reg, offset } => {
                if *offset == 0 {
                    format!("byte ptr [{}]", reg.name())
                } else if *offset > 0 {
                    format!("byte ptr [{}+{}]", reg.name(), offset)
                } else {
                    format!("byte ptr [{}-{}]", reg.name(), -offset)
                }
            }
            Self::TwoRegOffset { base, index, offset } => {
                format!("byte ptr {}", two_reg_addr(*base, *index, i32::from(*offset)))
            }
            Self::Ax => "al".to_owned(),
        }
    }
}

/// Emit the operator-specific instruction(s) given an already-loaded AX
/// (left operand) and a source string for the right operand. `unsigned`
/// selects `shr` over `sar` for `Shr` — the left operand's static type
/// drives the choice (right is always the shift count).
fn emit_op_with_source(out: &mut Vec<u8>, op: BinOp, src: &OperandSource, unsigned: bool) {
    emit_op_with_source_opts(out, op, src, unsigned, false);
}

/// Swap a Jcc mnemonic to the equivalent for an operand-reversed
/// cmp: `cmp a, b; jl` ↔ `cmp b, a; jg`, etc. Used when emit_compare
/// chooses to emit `cmp ax, <reg>` instead of `cmp <reg>, ax`.
fn swap_jcc(m: &'static str) -> &'static str {
    match m {
        "je" => "je",
        "jne" => "jne",
        "jl" => "jg",
        "jg" => "jl",
        "jle" => "jge",
        "jge" => "jle",
        "jb" => "ja",
        "ja" => "jb",
        "jbe" => "jae",
        "jae" => "jbe",
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_op_with_source_opts(
    out: &mut Vec<u8>,
    op: BinOp,
    src: &OperandSource,
    unsigned: bool,
    skip_mod_to_ax: bool,
) {
    // Identity folds: `a + 0`, `a - 0`, `a | 0`, `a ^ 0`, `a << 0`,
    // `a >> 0` are all just `a`. Skip the emission entirely. BCC
    // collapses these at compile time (fixtures 3370 `x + 0`, 2735
    // `x - 0`).
    if let OperandSource::Immediate(0) = src {
        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr) {
            return;
        }
    }
    // ±1 / +2 peephole: `inc ax` for +1 (1 byte vs 3), `dec ax` for -1,
    // and two `inc ax` for +2 (2 bytes vs 3). Notably -2 is NOT folded
    // to `dec ax; dec ax` — BCC keeps `add ax, -2` (3 bytes, AX-accum
    // imm16 form). Fixture 2074 (`x - 2`), 1277 (`fib(n - 2)`).
    if let OperandSource::Immediate(v) = src
        && ((matches!(op, BinOp::Add) && (*v == 1 || *v == 2))
            || (matches!(op, BinOp::Sub) && *v == 1))
    {
        let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
        for _ in 0..*v {
            let _ = write!(out, "\t{mnem}\tax\r\n");
        }
        return;
    }
    match op {
        BinOp::Add => {
            let _ = write!(out, "\tadd\tax,{}\r\n", src.word());
        }
        BinOp::Sub => {
            // BCC canonicalizes signed `ax - K` (immediate RHS, K !=
            // ±1, ±2 — those go through the inc/dec peephole
            // upstream) as `add ax, -K` (the AX-accumulator imm16
            // form `05 lo hi`) rather than `sub ax, K`. For unsigned
            // operands BCC keeps `sub` (fixture 3578: `unsigned x -
            // 5` → `2d 05 00`). Fixture 630 (signed `x - 5` → `05
            // fb ff`).
            if let OperandSource::Immediate(k) = src
                && !unsigned
            {
                let neg = (0u32.wrapping_sub(*k)) & 0xFFFF;
                let neg_i16 = neg as i16;
                let _ = write!(out, "\tadd\tax,{neg_i16}\r\n");
            } else {
                let _ = write!(out, "\tsub\tax,{}\r\n", src.word());
            }
        }
        BinOp::BitAnd => {
            let _ = write!(out, "\tand\tax,{}\r\n", src.word());
        }
        BinOp::BitOr => {
            let _ = write!(out, "\tor\tax,{}\r\n", src.word());
        }
        BinOp::BitXor => {
            let _ = write!(out, "\txor\tax,{}\r\n", src.word());
        }
        BinOp::Mul => {
            if let OperandSource::Immediate(v) = src {
                // `ax *= K` with K a small power of two unrolls into
                // `shl ax, 1` repeated. Fixture 592 (`return x * 2`
                // → `shl ax, 1`). For non-power-of-2 immediates BCC
                // materializes K in DX and uses the single-operand
                // `imul dx`. Fixture 615 (`return x * 3` → `mov dx,
                // 3; imul dx`).
                let k = *v;
                if k > 0 && k.is_power_of_two() && k.trailing_zeros() <= 15 {
                    let shifts = k.trailing_zeros();
                    if shifts <= 3 {
                        for _ in 0..shifts {
                            out.extend_from_slice(b"\tshl\tax,1\r\n");
                        }
                    } else {
                        let _ = write!(out, "\tmov\tcl,{shifts}\r\n");
                        out.extend_from_slice(b"\tshl\tax,cl\r\n");
                    }
                } else {
                    let v16 = k & 0xFFFF;
                    let _ = write!(out, "\tmov\tdx,{v16}\r\n");
                    out.extend_from_slice(b"\timul\tdx\r\n");
                }
            } else {
                let _ = write!(out, "\timul\t{}\r\n", src.word());
            }
        }
        BinOp::Div => {
            // For unsigned operands BCC uses `xor dx, dx; div <r/m>` —
            // zeros the upper half rather than sign-extending via
            // `cwd`, and uses the unsigned `div` instruction. Fixture
            // 946 (`unsigned a, b; return a / b;`).
            //
            // Unsigned div by power-of-2 constant: strength-reduce to
            // `shr ax, log2(K)` (signed div by pow2 is NOT reducible —
            // signed shift rounds toward -∞ while signed div rounds
            // toward 0). For K ≤ 8, unroll `shr ax, 1` (each 2 bytes);
            // larger K uses `mov cl, log2(K); shr ax, cl`. Fixtures
            // 3369, 2084, 1725.
            if unsigned
                && let OperandSource::Immediate(v) = src
                && *v > 0
                && v.is_power_of_two()
                && v.trailing_zeros() <= 15
            {
                let shifts = v.trailing_zeros();
                if shifts <= 3 {
                    for _ in 0..shifts {
                        out.extend_from_slice(b"\tshr\tax,1\r\n");
                    }
                } else {
                    let _ = write!(out, "\tmov\tcl,{shifts}\r\n");
                    out.extend_from_slice(b"\tshr\tax,cl\r\n");
                }
                return;
            }
            let (widen, mnem) = if unsigned {
                (&b"\txor\tdx,dx\r\n"[..], "div")
            } else {
                (&b"\tcwd\t\r\n"[..], "idiv")
            };
            if let OperandSource::Immediate(v) = src {
                // `idiv`/`div` has no immediate form. BCC materializes
                // the divisor in BX, then `<widen>; <mnem> bx`.
                // Fixture 584 (signed compound `/=`), 946 (unsigned).
                let v16 = v & 0xFFFF;
                let _ = write!(out, "\tmov\tbx,{v16}\r\n");
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\tbx\r\n");
            } else {
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\t{}\r\n", src.word());
            }
        }
        BinOp::Mod => {
            // Unsigned mod by power-of-2 K: `x % K = x & (K-1)`. Single
            // `and ax, K-1` (3 bytes) vs the full `mov bx; xor dx,dx;
            // div bx; mov ax,dx` (9 bytes).
            if unsigned
                && let OperandSource::Immediate(v) = src
                && *v > 0
                && v.is_power_of_two()
            {
                let mask = (v - 1) & 0xFFFF;
                let _ = write!(out, "\tand\tax,{mask}\r\n");
                return;
            }
            let (widen, mnem) = if unsigned {
                (&b"\txor\tdx,dx\r\n"[..], "div")
            } else {
                (&b"\tcwd\t\r\n"[..], "idiv")
            };
            if let OperandSource::Immediate(v) = src {
                let v16 = v & 0xFFFF;
                let _ = write!(out, "\tmov\tbx,{v16}\r\n");
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\tbx\r\n");
            } else {
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\t{}\r\n", src.word());
            }
            // `mov ax, dx` materializes the remainder in AX. Skipped
            // when the caller has signaled it'll read the remainder
            // from DX directly (e.g. an immediate `mov [mem], dx`
            // store). Saves 2 bytes for `int r = x % K` shapes.
            if !skip_mod_to_ax {
                out.extend_from_slice(b"\tmov\tax,dx\r\n");
            }
        }
        BinOp::Shl | BinOp::Shr => {
            let mnemonic = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if unsigned => "shr",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            // BCC unrolls expression-context shifts by 1, 2, or 3
            // into repeated `<mn> ax, 1` rather than the `mov cl, K;
            // <mn> ax, cl` pair — even when K=3 (which would be
            // shorter as a CL load). Fixture 627 (`x >> 3` → three
            // `sar ax, 1`). For K >= 4 BCC switches to the CL form.
            if let OperandSource::Immediate(k) = src
                && *k >= 1
                && *k <= 3
            {
                for _ in 0..*k {
                    let _ = write!(out, "\t{mnemonic}\tax,1\r\n");
                }
            } else {
                // SI/DI don't have addressable low bytes. Bridge by
                // copying the full 16-bit register to CX first, then
                // shift with CL. Fixture 2399 (`1 << i` with i in SI).
                if let OperandSource::Reg(r) = src
                    && matches!(r.name(), "si" | "di")
                {
                    let _ = write!(out, "\tmov\tcx,{}\r\n", r.name());
                    let _ = write!(out, "\t{mnemonic}\tax,cl\r\n");
                } else {
                    let _ = write!(out, "\tmov\tcl,{}\r\n", src.byte());
                    let _ = write!(out, "\t{mnemonic}\tax,cl\r\n");
                }
            }
        }
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            unreachable!("comparison op should take the cmp-as-value path");
        }
    }
}
