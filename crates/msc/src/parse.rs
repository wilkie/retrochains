use crate::*;

/// Parse Phase 1's source-shape envelope: a sequence of function
/// definitions, each `<ret-type> <name>(void) { <body> }`. `ret-type`
/// is `int` or `void`; bodies follow the existing per-statement
/// grammar.
pub(crate) fn parse_unit(source: &str) -> Result<Unit, EmitError> {
    let mut toks = tokenize(source)?;
    apply_typedef_substitutions(&mut toks);
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        local_names: Vec::new(),
        local_specs: Vec::new(),
        param_names: Vec::new(),
        param_struct_idxs: Vec::new(),
        param_is_char: Vec::new(),
        param_is_long: Vec::new(),
        param_is_unsigned: Vec::new(),
        global_names: Vec::new(),
        globals: Vec::new(),
        structs: Vec::new(),
        strings: Vec::new(),
        enum_consts: std::collections::HashMap::new(),
        typedefs: std::collections::HashMap::new(),
    };
    let mut functions = Vec::new();
    let mut decl_order: Vec<TopDecl> = Vec::new();
    while p.peek().is_some() {
        // Skip any preprocessor directives at file scope.
        if matches!(p.peek(), Some(Tok::PreprocLine)) {
            p.bump();
            continue;
        }
        // `struct <Name> { ... };` — record the struct definition.
        // `struct <Name> <var>;` and `struct <Name> *<var>;` fall
        // into the global-decl path further down.
        if matches!(p.peek(), Some(Tok::Kw("struct")))
            && matches!(p.toks.get(p.pos + 1), Some(Tok::Ident(_)))
            && matches!(p.toks.get(p.pos + 2), Some(Tok::LBrace))
        {
            parse_struct_def(&mut p)?;
            continue;
        }
        // `enum [<tag>] { NAME [= K], ... };` — register the listed
        // names as compile-time constants. Phase 1: only the anonymous
        // top-level enum (fixture 1004).
        if matches!(p.peek(), Some(Tok::Kw("enum"))) {
            parse_enum_def(&mut p)?;
            continue;
        }
        // `typedef <type> <name>;` — record the alias and continue.
        if matches!(p.peek(), Some(Tok::Kw("typedef"))) {
            parse_typedef(&mut p)?;
            continue;
        }
        // Disambiguate file-scope `int <name>...;` (global) from
        // `int <name>(...) { ... }` (function) by looking ahead
        // across any leading modifier keywords. A `(` after the
        // identifier means function; everything else means decl.
        let mut k = p.pos;
        while matches!(
            p.toks.get(k),
            Some(Tok::Kw("unsigned")) | Some(Tok::Kw("signed"))
                | Some(Tok::Kw("static")) | Some(Tok::Kw("extern"))
                | Some(Tok::Kw("register")) | Some(Tok::Kw("auto"))
                | Some(Tok::Kw("volatile")) | Some(Tok::Kw("const"))
                | Some(Tok::Kw("short"))
        ) {
            k += 1;
        }
        let is_type_prefix = matches!(
            p.toks.get(k),
            Some(Tok::Kw("int")) | Some(Tok::Kw("char")) | Some(Tok::Kw("long"))
                | Some(Tok::Kw("float")) | Some(Tok::Kw("double"))
        ) || (k > p.pos && matches!(p.toks.get(k), Some(Tok::Ident(_))))
            || matches!(p.toks.get(k), Some(Tok::Kw("struct")));
        if is_type_prefix {
            // Walk past the type kw (plus the struct's name token if
            // it's a `struct <Name>` prefix) + optional `*` to look
            // at the declarator's first token after the name.
            let mut after = k + 1;
            if matches!(p.toks.get(k), Some(Tok::Kw("struct"))) {
                after += 1; // consume the struct's name
            }
            // Skip calling-convention / pointer-distance modifiers
            // (`int far helper(...)`).
            while matches!(p.toks.get(after),
                Some(Tok::Kw("cdecl")) | Some(Tok::Kw("pascal"))
                | Some(Tok::Kw("far")) | Some(Tok::Kw("near"))
                | Some(Tok::Kw("huge")) | Some(Tok::Kw("interrupt"))
            ) { after += 1; }
            if matches!(p.toks.get(after), Some(Tok::Star)) { after += 1; }
            // Now expect an ident or the `main` keyword. The token
            // after the name decides function (`(`) vs global decl.
            let name_ok = matches!(
                p.toks.get(after),
                Some(Tok::Ident(_)) | Some(Tok::Kw("main"))
            );
            let is_function = name_ok
                && matches!(p.toks.get(after + 1), Some(Tok::LParen));
            if !is_function {
                let before = p.globals.len();
                parse_global_decl(&mut p)?;
                for i in before..p.globals.len() {
                    decl_order.push(TopDecl::Global(i));
                }
                continue;
            }
            // Prototype-only declaration: `int f(int);` ends in `;`
            // rather than `{`. Walk to the matching `)` and check.
            let lparen_idx = after + 1;
            let mut depth: i32 = 0;
            let mut close_idx = lparen_idx;
            for j in lparen_idx..p.toks.len() {
                match p.toks.get(j) {
                    Some(Tok::LParen) => depth += 1,
                    Some(Tok::RParen) => {
                        depth -= 1;
                        if depth == 0 { close_idx = j; break; }
                    }
                    _ => {}
                }
            }
            if matches!(p.toks.get(close_idx + 1), Some(Tok::Semi)) {
                // Skip the prototype. Advance the parser past `;`.
                p.pos = close_idx + 2;
                continue;
            }
        }
        let fn_idx = functions.len();
        functions.push(parse_function(&mut p)?);
        decl_order.push(TopDecl::Function(fn_idx));
    }
    if functions.is_empty() {
        return Err(EmitError::Unsupported(
            "translation unit has no functions".to_owned(),
        ));
    }
    Ok(Unit { globals: p.globals, structs: p.structs, functions, decl_order, strings: p.strings })
}
/// Parse a file-scope `struct <Name> <var> [= { ... }];` declaration.
/// Stores the struct global as if it were a `char` array sized to
/// the struct's total_bytes — that gives correct storage layout
/// without needing a separate Global::struct_idx field. Initializer
/// values are mapped to per-field byte slots.
pub(crate) fn parse_struct_global_decl(p: &mut Parser<'_>, is_static: bool) -> Result<(), EmitError> {
    p.eat(&Tok::Kw("struct"))?;
    let sname = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected struct name in global decl, got {other:?}"
            )));
        }
    };
    let sidx = p.structs.iter().position(|s| s.name == sname).ok_or_else(|| {
        EmitError::Unsupported(format!("unknown struct `{sname}` in global decl"))
    })?;
    let stotal = p.structs[sidx].total_bytes;
    let is_pointer = matches!(p.peek(), Some(Tok::Star));
    if is_pointer { p.bump(); }
    let name = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected global name, got {other:?}"
            )));
        }
    };
    let init = if matches!(p.peek(), Some(Tok::Assign)) {
        p.bump();
        if !is_pointer && matches!(p.peek(), Some(Tok::LBrace)) {
            p.bump();
            let mut slots: Vec<GlobalInit> = Vec::new();
            let mut field_idx = 0usize;
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                let field = &p.structs[sidx].fields[field_idx];
                let field_size = field.size;
                // Pad to the field's byte offset by BYTES (an Int slot is 2
                // bytes), not slot count, so word fields align correctly.
                while slots.iter().map(GlobalInit::size_bytes).sum::<usize>() < field.byte_off as usize {
                    slots.push(GlobalInit::Byte(0));
                }
                match p.peek() {
                    // Nested struct field `{...}` — flatten its scalar members
                    // into consecutive 2-byte slots (fixture 2102).
                    Some(Tok::LBrace) => {
                        p.bump();
                        while !matches!(p.peek(), Some(Tok::RBrace)) {
                            let v = parse_signed_int(p)?;
                            slots.push(GlobalInit::Int(v));
                            if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
                        }
                        p.eat(&Tok::RBrace)?;
                    }
                    // String-literal pointer field — intern + StrAddr (2100).
                    Some(Tok::StrLit(_)) => {
                        let bytes = match p.bump().cloned() {
                            Some(Tok::StrLit(b)) => b,
                            _ => unreachable!(),
                        };
                        let mut with_nul = bytes.clone();
                        with_nul.push(0);
                        let str_idx = p.strings.len();
                        p.strings.push(with_nul);
                        slots.push(GlobalInit::StrAddr(str_idx));
                    }
                    _ => {
                        let v = parse_signed_int(p)?;
                        if field_size == 1 {
                            slots.push(GlobalInit::Byte((v as u32 & 0xFF) as u8));
                        } else {
                            slots.push(GlobalInit::Int(v));
                        }
                    }
                }
                field_idx += 1;
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
            }
            p.eat(&Tok::RBrace)?;
            while slots.iter().map(GlobalInit::size_bytes).sum::<usize>() < stotal {
                slots.push(GlobalInit::Byte(0));
            }
            Some(slots)
        } else if is_pointer && matches!(p.peek(), Some(Tok::Amp)) {
            p.bump();
            let target_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `&` in init, got {other:?}"
                    )));
                }
            };
            let target_idx = p.global_names.iter().position(|n| *n == target_name)
                .ok_or_else(|| EmitError::Unsupported(format!(
                    "address-of unknown global `{target_name}`"
                )))?;
            Some(vec![GlobalInit::GlobalAddr(target_idx)])
        } else {
            return Err(EmitError::Unsupported(format!(
                "unsupported struct global init: {:?}", p.peek()
            )));
        }
    } else {
        None
    };
    p.eat(&Tok::Semi)?;
    let array_len = if is_pointer { 1 } else { stotal };
    let element_size = 1; // byte-oriented storage; fields by offset
    p.global_names.push(name.clone());
    p.globals.push(Global {
        name,
        init,
        array_len,
        element_size,
        is_pointer,
        struct_idx: Some(sidx),
        is_long: false,
        is_static,
        is_extern: false,
        is_unsigned: false,
        is_float: false,
    });
    Ok(())
}
/// Parse `struct Name { <field-decl>; ... };` — record the struct's
/// fields and their byte offsets. C89 padding rule: each field
/// starts at its natural alignment (even for `int`/pointer, byte
/// for `char`). MSC's small-model size is the sum of field sizes
/// without trailing pad until the next int boundary; we use the
/// same rule. Anonymous structs and bitfields aren't supported.
/// `enum [<tag>] { NAME [= K], ... };` — record each enum constant
/// in `enum_consts` so subsequent Ident lookups fold to the literal.
/// The optional tag is consumed but unused (no type tracking yet).
/// Default value is 0 for the first entry, then increment by 1.
pub(crate) fn parse_enum_def(p: &mut Parser<'_>) -> Result<(), EmitError> {
    p.eat(&Tok::Kw("enum"))?;
    if matches!(p.peek(), Some(Tok::Ident(_))) { p.bump(); }
    p.eat(&Tok::LBrace)?;
    let mut next_val: i32 = 0;
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        let name = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => return Err(EmitError::Unsupported(format!(
                "expected enum constant name, got {other:?}"
            ))),
        };
        if matches!(p.peek(), Some(Tok::Assign)) {
            p.bump();
            next_val = parse_signed_int(p)?;
        }
        p.enum_consts.insert(name, next_val);
        next_val += 1;
        if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
    }
    p.eat(&Tok::RBrace)?;
    p.eat(&Tok::Semi)?;
    Ok(())
}
/// `typedef <type-tokens> <name>;` — record the name as an alias for
/// the source-level type. Phase 1: just consume the tokens (we don't
/// model type aliases; downstream identifiers using the alias would
/// need type resolution). Skip-only path matches MSC's `typedef
/// long` fixture which is unused beyond the declaration.
pub(crate) fn parse_typedef(p: &mut Parser<'_>) -> Result<(), EmitError> {
    p.eat(&Tok::Kw("typedef"))?;
    // Walk through the type tokens — accept primitive keywords +
    // modifiers, ignore signedness — and remember the last primitive
    // type name encountered. Skip pointer markers.
    let mut base: Option<&'static str> = None;
    while !matches!(p.peek(), Some(Tok::Semi) | None) {
        match p.peek().cloned() {
            Some(Tok::Kw("int")) => { base = Some("int"); p.bump(); }
            Some(Tok::Kw("char")) => { base = Some("char"); p.bump(); }
            Some(Tok::Kw("long")) => { base = Some("long"); p.bump(); }
            Some(Tok::Kw("unsigned"))
            | Some(Tok::Kw("signed"))
            | Some(Tok::Kw("short")) => { p.bump(); }
            Some(Tok::Star) => { p.bump(); }
            Some(Tok::Ident(name)) => {
                p.bump();
                // The last identifier is the alias name. Record it
                // before potentially consuming more.
                if matches!(p.peek(), Some(Tok::Semi) | None) {
                    if let Some(b) = base {
                        p.typedefs.insert(name, b);
                    }
                }
            }
            _ => { p.bump(); }
        }
    }
    p.eat(&Tok::Semi)?;
    Ok(())
}
pub(crate) fn parse_struct_def(p: &mut Parser<'_>) -> Result<(), EmitError> {
    p.eat(&Tok::Kw("struct"))?;
    let sname = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected struct name, got {other:?}"
            )));
        }
    };
    p.eat(&Tok::LBrace)?;
    let mut fields: Vec<StructField> = Vec::new();
    let mut cursor: usize = 0;
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        skip_decl_modifiers(p);
        // A field may be a nested `struct <Name>` (value or pointer).
        let mut field_struct_idx: Option<usize> = None;
        let size: u8 = if matches!(p.peek(), Some(Tok::Kw("struct"))) {
            p.bump();
            let inner_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => return Err(EmitError::Unsupported(format!(
                    "expected struct name for nested field, got {other:?}"))),
            };
            let inner = p.structs.iter().position(|s| s.name == inner_name).ok_or_else(|| {
                EmitError::Unsupported(format!("unknown nested struct `{inner_name}`"))
            })?;
            field_struct_idx = Some(inner);
            u8::try_from(p.structs[inner].total_bytes).expect("nested struct fits in u8")
        } else {
            match p.bump().cloned() {
                Some(Tok::Kw("int")) => 2,
                Some(Tok::Kw("char")) => 1,
                Some(Tok::Kw("long")) => 4,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "struct field type not yet supported: {other:?}"
                    )));
                }
            }
        };
        let is_ptr = if matches!(p.peek(), Some(Tok::Star)) {
            p.bump();
            true
        } else {
            false
        };
        // A pointer-to-struct field is a 2-byte near pointer, not an inline
        // struct, so it carries no struct_idx for inline member access.
        if is_ptr { field_struct_idx = None; }
        let fname = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected struct field name, got {other:?}"
                )));
            }
        };
        let field_size = if is_ptr { 2 } else { size };
        // Word-align int / pointer / struct fields. Char fields take the
        // next byte at any offset.
        if field_size >= 2 && cursor % 2 != 0 {
            cursor += 1;
        }
        let byte_off = u16::try_from(cursor).expect("field offset fits in u16");
        fields.push(StructField {
            name: fname,
            byte_off,
            size: field_size,
            struct_idx: field_struct_idx,
        });
        cursor += field_size as usize;
        p.eat(&Tok::Semi)?;
    }
    p.eat(&Tok::RBrace)?;
    p.eat(&Tok::Semi)?;
    // Round total up to the natural alignment (2 bytes for any
    // struct containing an int/pointer field; 1 byte otherwise).
    let needs_word_align = fields.iter().any(|f| f.size >= 2);
    let total_bytes = if needs_word_align { (cursor + 1) & !1 } else { cursor };
    p.structs.push(StructDef { name: sname, fields, total_bytes });
    Ok(())
}
/// Parse one file-scope `<type> <name> [= <init>];` declaration and
/// register it in the parser's globals list. Phase 1 covers
/// `int <name>`, `int <name>[N]`, and `char *<name>` with optional
/// initializer. Caller has confirmed the next tokens form a
/// declaration, not a function.
pub(crate) fn parse_global_decl(p: &mut Parser<'_>) -> Result<(), EmitError> {
    // Walk forward to see if `static` or `extern` is part of the
    // modifier prefix — affects symbol visibility (no PUBDEF for
    // static; EXTDEF rather than COMDEF for extern).
    let mut i = p.pos;
    let mut is_static = false;
    let mut is_extern = false;
    let mut is_unsigned = false;
    while let Some(t) = p.toks.get(i) {
        match t {
            Tok::Kw("static") => { is_static = true; i += 1; }
            Tok::Kw("extern") => { is_extern = true; i += 1; }
            Tok::Kw("unsigned") => { is_unsigned = true; i += 1; }
            Tok::Kw("signed")
                | Tok::Kw("register") | Tok::Kw("auto")
                | Tok::Kw("volatile") | Tok::Kw("const")
                | Tok::Kw("short") => { i += 1; }
            _ => break,
        }
    }
    // Skip any leading storage/qualifier modifiers (unsigned, static,
    // ...) — we treat them all as no-ops at the codegen level.
    let mods_consumed = skip_decl_modifiers(p);
    // `struct <Name> name [= {...}] ;` and `struct <Name> *name [= ...] ;`
    // routed through a separate parse path because the size + element
    // model differ from primitive types.
    if matches!(p.peek(), Some(Tok::Kw("struct"))) {
        return parse_struct_global_decl(p, is_static);
    }
    // Type prefix. Phase 1 globals: `int [*]`, `char *`, `char [N]`,
    // and minimal `long` support (storage only; arithmetic not yet).
    // Bare `unsigned x;` (no following int/char) implies int.
    let mut is_pointer = false;
    let mut is_char = false;
    let mut is_long = false;
    let mut is_float = false;
    let mut float_width = 0usize;
    match p.peek() {
        Some(Tok::Kw("int")) => {
            p.bump();
            while matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                is_pointer = true;
            }
        }
        Some(Tok::Kw("float")) | Some(Tok::Kw("double")) => {
            float_width = if matches!(p.peek(), Some(Tok::Kw("double"))) { 8 } else { 4 };
            is_float = true;
            p.bump();
            while matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                is_pointer = true;
                is_float = false; // pointer-to-float is a 2-byte near pointer
            }
        }
        Some(Tok::Kw("char")) => {
            p.bump();
            is_char = true;
            while matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                is_pointer = true;
            }
        }
        Some(Tok::Kw("long")) => {
            p.bump();
            // `long int` is just `long`.
            if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
            is_long = true;
            while matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                is_pointer = true;
            }
        }
        // Bare modifier (`unsigned x;`) → implicit int.
        Some(Tok::Ident(_)) if mods_consumed > 0 => {}
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected `int`, `int *`, `long`, `char *`, or `char [...]` for global, got {other:?}"
            )));
        }
    };
    // Comma-separated declarators share the base type but each gets its own
    // `*` (pointer-ness), array suffix, and initializer: `int g1, g2;`,
    // `int *p, q;`. Loop over them; `is_pointer` resets per declarator.
    // Fixtures 3944-3947.
    loop {
    // Pointer-ness is per-declarator; floatness follows from the base type
    // unless this declarator is a pointer (pointer-to-float is a near word).
    is_float = float_width != 0 && !is_pointer;
    let name = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected global name, got {other:?}"
            )));
        }
    };
    // Optional `[N]` (or `[]` with init) for an array declaration.
    // The element count determines the COMDEF or _DATA byte length.
    let mut implicit_array_len = false;
    let array_len = if matches!(p.peek(), Some(Tok::LBrack)) {
        p.bump();
        if matches!(p.peek(), Some(Tok::RBrack)) {
            // `int a[] = {...};` — size from init list count.
            p.bump();
            implicit_array_len = true;
            0 // placeholder; we'll overwrite after parsing init below
        } else {
        let k = parse_signed_int(p)?;
        if k <= 0 {
            return Err(EmitError::Unsupported(format!(
                "array length must be positive, got {k}"
            )));
        }
        let n = k as usize;
        p.eat(&Tok::RBrack)?;
        n
        }
    } else {
        1
    };
    let init = if matches!(p.peek(), Some(Tok::Assign)) {
        p.bump();
        if matches!(p.peek(), Some(Tok::LBrace)) {
            p.bump();
            let mut values = Vec::new();
            loop {
                if matches!(p.peek(), Some(Tok::RBrace)) {
                    // Trailing comma in init: `{1, 2, 3,}`.
                    p.bump();
                    break;
                }
                if is_float && !is_pointer {
                    // `double arr[] = {1.0, 2.0, ...}` — IEEE bytes per element.
                    let bits = match p.bump().cloned() {
                        Some(Tok::Float(b, _)) => b,
                        Some(Tok::Int(n)) => f64::from(n).to_bits(),
                        other => return Err(EmitError::Unsupported(format!(
                            "expected float literal in initializer, got {other:?}"))),
                    };
                    values.push(GlobalInit::FloatBits(bits, float_width));
                    match p.peek() {
                        Some(Tok::Comma) => { p.bump(); }
                        Some(Tok::RBrace) => { p.bump(); break; }
                        other => return Err(EmitError::Unsupported(format!(
                            "expected `,` or `}}` in initializer, got {other:?}"))),
                    }
                    continue;
                }
                let v = parse_signed_int(p)?;
                if is_char && !is_pointer {
                    values.push(GlobalInit::Byte((v as u32 & 0xFF) as u8));
                } else {
                    values.push(GlobalInit::Int(v));
                }
                match p.peek() {
                    Some(Tok::Comma) => { p.bump(); }
                    Some(Tok::RBrace) => { p.bump(); break; }
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected `,` or `}}` in initializer, got {other:?}"
                        )));
                    }
                }
            }
            Some(values)
        } else if is_pointer && matches!(p.peek(), Some(Tok::StrLit(_))) {
            let bytes = match p.bump().cloned() {
                Some(Tok::StrLit(b)) => b,
                _ => unreachable!(),
            };
            let mut with_nul = bytes.clone();
            with_nul.push(0);
            let str_idx = p.strings.len();
            p.strings.push(with_nul);
            Some(vec![GlobalInit::StrAddr(str_idx)])
        } else if is_char && matches!(p.peek(), Some(Tok::StrLit(_))) {
            // `char a[N] = "...";` — bytes land directly in _DATA.
            // Trailing NUL is included; if the literal is shorter than
            // N, the remainder stays zero-filled by the linker.
            let bytes = match p.bump().cloned() {
                Some(Tok::StrLit(b)) => b,
                _ => unreachable!(),
            };
            let mut slots: Vec<GlobalInit> =
                bytes.iter().map(|b| GlobalInit::Byte(*b)).collect();
            // C semantics: include the implicit NUL if it fits.
            if slots.len() < array_len {
                slots.push(GlobalInit::Byte(0));
            }
            Some(slots)
        } else if is_pointer && matches!(p.peek(), Some(Tok::Amp)) {
            p.bump();
            let target_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `&` in initializer, got {other:?}"
                    )));
                }
            };
            let target_idx = p.global_names.iter().position(|n| *n == target_name)
                .ok_or_else(|| EmitError::Unsupported(format!(
                    "address-of unknown global `{target_name}`"
                )))?;
            Some(vec![GlobalInit::GlobalAddr(target_idx)])
        } else if is_float && !is_pointer {
            // `double g = 3.14;` / `float g = 3.5f;` — IEEE bytes in _DATA.
            let bits = match p.bump().cloned() {
                Some(Tok::Float(bits, _)) => bits,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected float literal initializer, got {other:?}"
                    )));
                }
            };
            Some(vec![GlobalInit::FloatBits(bits, float_width)])
        } else if is_long {
            let k = parse_signed_int(p)?;
            let low = (k as u32 & 0xFFFF) as i32;
            let high = (((k as u32) >> 16) & 0xFFFF) as i32;
            Some(vec![GlobalInit::Int(low), GlobalInit::Int(high)])
        } else if is_char && !is_pointer && array_len == 1 {
            // `char g = K;` — single byte in _DATA.
            let k = parse_signed_int(p)?;
            Some(vec![GlobalInit::Byte((k as u32 & 0xFF) as u8)])
        } else {
            Some(vec![GlobalInit::Int(parse_signed_int(p)?)])
        }
    } else {
        None
    };
    // `element_size` describes the pointed-to or array-element type
    // (1 for `char` family, 2 otherwise). `is_pointer` is set when
    // the declarator carries a `*`. Storage size is independent:
    // pointers are always 2 bytes; arrays scale by `array_len`.
    let element_size = if is_char { 1 } else if is_float && !is_pointer { float_width } else { 2 };
    // Long storage is 4 bytes. A scalar long is modeled as a 2-slot word array
    // (array_len=2, element_size=2) so `(int)g` reads the low word at the base.
    // A long ARRAY keeps its element count with a 4-byte element so `a[K]` lands
    // at K*4 and storage is N*4 bytes.
    let (mut array_len, element_size) = if is_long && !is_pointer {
        if array_len <= 1 { (2usize, 2usize) } else { (array_len, 4usize) }
    } else {
        (array_len, element_size)
    };
    // Implicit array size from initializer count (`int a[] = {1,2,3};`).
    if implicit_array_len {
        array_len = init.as_ref().map(|v| v.len()).unwrap_or(0).max(1);
    }
    p.global_names.push(name.clone());
    // A long POINTER (`long *p`) is just a near pointer; its long-ness belongs
    // to the pointee, so it must not be flagged is_long (else `p = a` would be
    // treated as a 4-byte long store).
    p.globals.push(Global { name, init, array_len, element_size, is_pointer, struct_idx: None, is_long: is_long && !is_pointer, is_static, is_extern, is_unsigned, is_float: is_float && !is_pointer });
    // Another declarator after a comma, or end of statement.
    match p.peek() {
        Some(Tok::Comma) => {
            p.bump();
            // Reset pointer-ness and re-read this declarator's `*` markers.
            is_pointer = false;
            while matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                is_pointer = true;
            }
        }
        _ => break,
    }
    }
    p.eat(&Tok::Semi)?;
    Ok(())
}
/// Returns true when `e` has a direct `Local` leaf whose `init_is_literal`
/// flag is set, whose known init value equals `fold_val`, AND whose storage
/// size matches `target_size`.
///
/// This detects "identity-operation" initialisers: `int a = x << 0` where
/// x=5 folds to 5 (same as x's init), so MSC's middle-end effectively treats
/// `a` as an alias of `x`. Contrast `int a = x & 127` (x=0x1234 → 52 ≠ 0x1234)
/// or `int a = x + y` (x=5, y=10 → 15 ≠ 5 and 15 ≠ 10), which MSC does NOT
/// fold into later uses.  The size check prevents cross-type-size detection
/// like `int n = c` where c is a char (fixture 1043).
pub(crate) fn init_expr_has_matching_literal_leaf(
    e: &Expr,
    fold_val: i32,
    target_size: usize,
    locals: &[LocalSpec],
) -> bool {
    match e {
        Expr::Local(i) => locals
            .get(*i)
            .map(|l| l.init_is_literal && l.init == Some(fold_val) && l.size == target_size)
            .unwrap_or(false),
        Expr::BinOp { left, right, .. } => {
            init_expr_has_matching_literal_leaf(left, fold_val, target_size, locals)
                || init_expr_has_matching_literal_leaf(right, fold_val, target_size, locals)
        }
        _ => false,
    }
}
/// Const-fold a float/double initializer to its f64 value using the locals
/// declared so far. Handles literals, int/float local references, and `+-*/`
/// arithmetic — enough for `(float)i`, `double d = f`, and `a + b` inits.
pub(crate) fn float_fold_value(e: &Expr, specs: &[LocalSpec]) -> Option<f64> {
    match e {
        Expr::FloatLit(bits, _) => Some(f64::from_bits(*bits)),
        Expr::IntLit(k) => Some(*k as f64),
        Expr::Local(idx) => {
            let s = specs.get(*idx)?;
            if s.is_float { s.float_bits.map(f64::from_bits) } else { s.init.map(|k| k as f64) }
        }
        Expr::BinOp { op, left, right } => {
            let l = float_fold_value(left, specs)?;
            let r = float_fold_value(right, specs)?;
            match op {
                BinOp::Add => Some(l + r),
                BinOp::Sub => Some(l - r),
                BinOp::Mul => Some(l * r),
                BinOp::Div => Some(l / r),
                _ => None,
            }
        }
        _ => None,
    }
}
/// Like `float_fold_value` but only folds float locals whose init was a direct
/// literal (`init_is_literal`); a non-literal float local (cast/arith init)
/// returns `None` so its consumer lowers to runtime FP, not a fold.
pub(crate) fn float_fold_literal(e: &Expr, specs: &[LocalSpec]) -> Option<f64> {
    match e {
        Expr::FloatLit(bits, _) => Some(f64::from_bits(*bits)),
        Expr::IntLit(k) => Some(*k as f64),
        Expr::Local(idx) => {
            let s = specs.get(*idx)?;
            if s.is_float {
                if s.init_is_literal { s.float_bits.map(f64::from_bits) } else { None }
            } else {
                s.init.map(|k| k as f64)
            }
        }
        Expr::BinOp { op, left, right } => {
            let l = float_fold_literal(left, specs)?;
            let r = float_fold_literal(right, specs)?;
            match op {
                BinOp::Add => Some(l + r),
                BinOp::Sub => Some(l - r),
                BinOp::Mul => Some(l * r),
                BinOp::Div => Some(l / r),
                _ => None,
            }
        }
        _ => None,
    }
}
/// True when the expression references a `float`/`double` operand. Gates the
/// int-context float fold so it never alters pure-integer arithmetic.
pub(crate) fn expr_involves_float(e: &Expr, specs: &[LocalSpec]) -> bool {
    match e {
        Expr::FloatLit(..) => true,
        Expr::Local(idx) => specs.get(*idx).map(|s| s.is_float).unwrap_or(false),
        Expr::BinOp { left, right, .. } => {
            expr_involves_float(left, specs) || expr_involves_float(right, specs)
        }
        _ => false,
    }
}
pub(crate) fn parse_function(p: &mut Parser<'_>) -> Result<Function, EmitError> {
    // `<modifiers>* <ret-type> <name>(...)` — skip any leading
    // storage-class / sign keywords (static, extern, unsigned, etc.),
    // then expect `int` / `char` / `void`. `char` returns are widened
    // to int via cbw at the consume site; treat as int here.
    skip_decl_modifiers(p);
    let mut return_char = false;
    let mut return_long = false;
    let mut return_float_width = 0usize;
    let return_int = match p.bump().cloned() {
        Some(Tok::Kw("int")) => true,
        Some(Tok::Kw("char")) => { return_char = true; true }
        Some(Tok::Kw("long")) => {
            if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
            return_long = true;
            true
        }
        // `float`/`double` returns go through the __fac floating accumulator,
        // not AX — `return_int` is false.
        Some(Tok::Kw("float")) => { return_float_width = 4; false }
        Some(Tok::Kw("double")) => { return_float_width = 8; false }
        Some(Tok::Kw("void")) => false,
        Some(Tok::Kw("struct")) => {
            // Skip the struct's name. Phase 1 only models functions
            // that return a struct *pointer* (`struct S *f()`) — full
            // struct-by-value returns need a hidden-pointer ABI we
            // don't support yet.
            if matches!(p.peek(), Some(Tok::Ident(_))) { p.bump(); }
            true
        }
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected return type, got {other:?}"
            )));
        }
    };
    // Skip post-return-type calling-convention / pointer-distance
    // modifiers (`int far helper(...)`).
    skip_decl_modifiers(p);
    // Pointer return types (`char *fn(...)`, `int *fn(...)`): consume
    // the `*` markers. We model the return as int (a pointer fits in
    // AX) — sufficient for `fn()[K]` shapes (fixture 1227).
    while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
    let name = match p.bump().cloned() {
        Some(Tok::Kw("main")) => "main".to_owned(),
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected function name, got {other:?}"
            )));
        }
    };
    p.eat(&Tok::LParen)?;
    // Parameter list: either `void` (no params) or one or more
    // `int <name>` separated by `,`. Phase 1 only handles int
    // parameters; other types come with later fixtures.
    let params = if matches!(p.peek(), Some(Tok::Kw("void"))) {
        p.bump();
        (Vec::<String>::new(), Vec::<Option<usize>>::new(), Vec::<bool>::new(), Vec::<bool>::new(), Vec::<bool>::new(), Vec::<usize>::new(), Vec::<usize>::new())
    } else if matches!(p.peek(), Some(Tok::RParen)) {
        // K&R-style empty param list (`int main()`). Treat as no
        // params. Fixture 888.
        (Vec::<String>::new(), Vec::<Option<usize>>::new(), Vec::<bool>::new(), Vec::<bool>::new(), Vec::<bool>::new(), Vec::<usize>::new(), Vec::<usize>::new())
    } else {
        let mut names = Vec::new();
        let mut struct_idxs: Vec<Option<usize>> = Vec::new();
        let mut is_chars: Vec<bool> = Vec::new();
        let mut is_longs: Vec<bool> = Vec::new();
        let mut is_unsigned_ints: Vec<bool> = Vec::new();
        let mut float_widths: Vec<usize> = Vec::new();
        let mut pointee_sizes: Vec<usize> = Vec::new();
        loop {
            // Optional sign/qualifier modifiers, then `int` / `char` /
            // `struct Name`. Pointers (`<type> *<name>`) consume one
            // stack slot regardless of pointee type.
            let mod_start = p.pos;
            skip_decl_modifiers(p);
            let has_unsigned_mod = (mod_start..p.pos).any(|i| {
                matches!(p.toks.get(i), Some(Tok::Kw("unsigned")))
            });
            let mut struct_idx: Option<usize> = None;
            let mut is_char = false;
            let mut is_long = false;
            let mut float_width = 0usize;
            match p.peek() {
                Some(Tok::Kw("char")) => { is_char = true; p.bump(); }
                Some(Tok::Kw("int")) => { p.bump(); }
                Some(Tok::Kw("float")) => { float_width = 4; p.bump(); }
                Some(Tok::Kw("double")) => { float_width = 8; p.bump(); }
                Some(Tok::Kw("long")) => {
                    p.bump();
                    if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
                    is_long = true;
                }
                Some(Tok::Kw("struct")) => {
                    p.bump();
                    let sname = match p.bump().cloned() {
                        Some(Tok::Ident(s)) => s,
                        other => {
                            return Err(EmitError::Unsupported(format!(
                                "expected struct name in param, got {other:?}"
                            )));
                        }
                    };
                    struct_idx = p.structs.iter().position(|s| s.name == sname);
                }
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected `int`, `char`, or `struct` in parameter type, got {other:?}"
                    )));
                }
            }
            let has_ptr = matches!(p.peek(), Some(Tok::Star));
            // Pointee byte size, captured before `is_char` is cleared below:
            // char*→1, long*→4, float*→width, struct*→struct size, int*→2.
            // 0 = not a pointer. Drives pointer-arithmetic element scaling.
            let mut pointee_size = if has_ptr {
                if is_char { 1 } else if is_long { 4 }
                else if float_width != 0 { float_width }
                else if let Some(si) = struct_idx {
                    p.structs.get(si).map(|s| s.total_bytes).unwrap_or(2)
                } else { 2 }
            } else { 0 };
            if has_ptr {
                p.bump();
                is_char = false; // pointer: always word-sized
            }
            let pname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected parameter name, got {other:?}"
                    )));
                }
            };
            // `int a[]` and `int a[N]` decay to `int *a`. Eat the
            // optional bracket pair.
            if matches!(p.peek(), Some(Tok::LBrack)) {
                // array decays to pointer: pointee = the element type size.
                if pointee_size == 0 {
                    pointee_size = if is_char { 1 } else if is_long { 4 }
                        else if float_width != 0 { float_width } else { 2 };
                }
                is_char = false; // array decays to pointer: word-sized
                p.bump();
                while !matches!(p.peek(), Some(Tok::RBrack)) {
                    p.bump();
                }
                p.eat(&Tok::RBrack)?;
            }
            pointee_sizes.push(pointee_size);
            names.push(pname);
            struct_idxs.push(struct_idx);
            is_chars.push(is_char);
            is_longs.push(is_long && !has_ptr); // pointer-to-long is word-sized
            // `unsigned int x` (not pointer, not char) → track for /2 optimization
            is_unsigned_ints.push(has_unsigned_mod && !is_char && !has_ptr);
            float_widths.push(if has_ptr { 0 } else { float_width }); // pointer-to-float is word-sized
            if matches!(p.peek(), Some(Tok::Comma)) {
                p.bump();
                continue;
            }
            break;
        }
        (names, struct_idxs, is_chars, is_longs, is_unsigned_ints, float_widths, pointee_sizes)
    };
    let (params, param_struct_idxs, param_is_char, param_is_long, param_is_unsigned, param_float_width, param_pointee_size) = params;
    p.eat(&Tok::RParen)?;
    p.eat(&Tok::LBrace)?;

    // Reset per-function name lists, then populate with this
    // function's params before parsing the body.
    p.local_names.clear();
    p.local_specs.clear();
    p.param_names = params.clone();
    p.param_struct_idxs = param_struct_idxs;
    p.param_is_char = param_is_char.clone();
    p.param_is_long = param_is_long.clone();
    p.param_is_unsigned = param_is_unsigned.clone();

    // `[storage-class]+ int|char <name> [= <init>] (, <name> [= <init>])* ;`
    //
    // A non-constant init becomes a synthetic assignment statement
    // prepended to the body.
    let mut locals: Vec<LocalSpec> = Vec::new();
    let mut prelude: Vec<Stmt> = Vec::new();
    loop {
        // A function-local `static` declaration is really a TU-private global:
        // route it through parse_global_decl, which appends it to p.globals
        // (no PUBDEF, is_static) and p.global_names so body references resolve
        // to `Expr::Global`. MSC names them `$S<n>_<name>`, but that symbol is
        // cosmetic — the OBJ references them by _DATA offset.
        {
            let mut j = p.pos;
            let mut has_static = false;
            while let Some(Tok::Kw(k)) = p.toks.get(j) {
                if !matches!(*k, "static" | "unsigned" | "signed" | "register"
                    | "auto" | "volatile" | "const" | "short" | "extern") {
                    break;
                }
                if *k == "static" { has_static = true; }
                j += 1;
            }
            if has_static {
                parse_global_decl(p)?;
                continue;
            }
        }
        // Peek across leading modifier keywords. The decl is a local
        // when the next token is int/char OR when *any* modifier was
        // present (bare `unsigned x;` means `unsigned int x;`).
        let mut peek_pos = p.pos;
        let start_pos = peek_pos;
        while matches!(
            p.toks.get(peek_pos),
            Some(Tok::Kw("unsigned")) | Some(Tok::Kw("signed"))
                | Some(Tok::Kw("static")) | Some(Tok::Kw("extern"))
                | Some(Tok::Kw("register")) | Some(Tok::Kw("auto"))
                | Some(Tok::Kw("volatile")) | Some(Tok::Kw("const"))
                | Some(Tok::Kw("short"))
        ) {
            peek_pos += 1;
        }
        // `enum [<tag>]` — treat as int (no separate enum type yet).
        // Consume the optional tag identifier and let the standard
        // int-decl path handle the declarator(s).
        let enum_consumed = matches!(p.toks.get(peek_pos), Some(Tok::Kw("enum")));
        if enum_consumed {
            skip_decl_modifiers(p);
            p.bump(); // enum
            if matches!(p.peek(), Some(Tok::Ident(_))) { p.bump(); }
        }
        // `struct <Name>` form is a separate path because the size is
        // looked up from the struct registry rather than a primitive
        // type token. Each declarator can still be `s` (struct value)
        // or `*s` (struct pointer).
        if matches!(p.toks.get(peek_pos), Some(Tok::Kw("struct"))) {
            skip_decl_modifiers(p);
            p.bump(); // struct
            let sname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected struct name in local decl, got {other:?}"
                    )));
                }
            };
            let sidx = p.structs.iter().position(|s| s.name == sname).ok_or_else(|| {
                EmitError::Unsupported(format!("unknown struct `{sname}` in local decl"))
            })?;
            let stotal = p.structs[sidx].total_bytes;
            loop {
                let is_ptr = if matches!(p.peek(), Some(Tok::Star)) {
                    p.bump();
                    true
                } else {
                    false
                };
                let lname = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected identifier in struct decl, got {other:?}"
                        )));
                    }
                };
                let spec = if is_ptr {
                    LocalSpec { size: 2, array_len: 1, init: None, struct_idx: Some(sidx), is_long: false, init_is_literal: false, is_far_ptr: false, pointee_size: stotal, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None }
                } else {
                    LocalSpec { size: 1, array_len: stotal, init: None, struct_idx: Some(sidx), is_long: false, init_is_literal: false, is_far_ptr: false, pointee_size: 0, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None }
                };
                let local_idx = locals.len();
                p.local_names.push(lname);
                p.local_specs.push(spec.clone());
                locals.push(spec);
                // `struct s *p = &<global>;` — synthesize as runtime
                // assign. Only handle the address-of-global init form.
                if is_ptr && matches!(p.peek(), Some(Tok::Assign)) {
                    p.bump();
                    let init_expr = parse_expr(p)?;
                    prelude.push(Stmt::Assign {
                        target: AssignTarget::Local(local_idx),
                        value: init_expr,
                    });
                }
                if matches!(p.peek(), Some(Tok::Comma)) {
                    p.bump();
                    continue;
                }
                break;
            }
            p.eat(&Tok::Semi)?;
            continue;
        }
        // Detect `unsigned` in the peek range so we can mark char locals.
        let has_unsigned = (start_pos..peek_pos).any(|i| {
            matches!(p.toks.get(i), Some(Tok::Kw("unsigned")))
        });
        let (size, has_explicit_type, is_long_decl, is_float_decl) = if enum_consumed {
            // The `enum [<tag>]` prefix has already been consumed; the
            // next token is the declarator. Treat as int.
            (2usize, false, false, false)
        } else { match p.toks.get(peek_pos) {
            Some(Tok::Kw("int")) => (2usize, true, false, false),
            Some(Tok::Kw("char")) => (1usize, true, false, false),
            Some(Tok::Kw("long")) => (2usize, true, true, false),
            Some(Tok::Kw("float")) => (4usize, true, false, true),
            Some(Tok::Kw("double")) => (8usize, true, false, true),
            // `unsigned x;` / `signed x;` → implicit int.
            _ if peek_pos > start_pos
                && matches!(p.toks.get(peek_pos), Some(Tok::Ident(_))) =>
            {
                (2usize, false, false, false)
            }
            _ => break,
        }};
        skip_decl_modifiers(p);
        if has_explicit_type {
            p.bump(); // type kw
            // Consume optional `int` after `long` (i.e. `long int`).
            if is_long_decl && matches!(p.peek(), Some(Tok::Kw("int"))) {
                p.bump();
            }
        }
        // Float/double locals: `float|double name [= <literal>] [, ...] ;`.
        // The init literal's f64 bits are stored for the x87 const pool;
        // a non-literal float init isn't supported yet.
        if is_float_decl {
            loop {
                let lname = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => return Err(EmitError::Unsupported(format!(
                        "expected float local name, got {other:?}"))),
                };
                // `float a[N];` — a stack float array. Elements are set by later
                // `a[K] = <float>` statements (no scalar init here).
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let n = parse_signed_int(p)?;
                    if n <= 0 {
                        return Err(EmitError::Unsupported(format!(
                            "float array length must be positive, got {n}")));
                    }
                    p.eat(&Tok::RBrack)?;
                    let mut s = LocalSpec::float_(size, None);
                    s.array_len = n as usize;
                    p.local_names.push(lname);
                    p.local_specs.push(s.clone());
                    locals.push(s);
                    if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
                    break;
                }
                let spec = if matches!(p.peek(), Some(Tok::Assign)) {
                    p.bump();
                    // A direct literal init (`float f = 3.0f;`) folds: the local
                    // gets `init = (int)value` so `(int)f` lowers to `mov ax,K`
                    // (fixture 1670). A cast/arith init (`(float)i`, `double d =
                    // f`, `a + b`) is const-foldable but kept non-literal: the
                    // CONST temp is materialized and the store keeps st(0) live
                    // (`fst`) so the coupled `(int)<local>` is `call __ftol`.
                    let direct_literal = matches!(p.peek(), Some(Tok::Float(..)) | Some(Tok::Int(..)))
                        && matches!(p.toks.get(p.pos + 1), Some(Tok::Comma) | Some(Tok::Semi));
                    if direct_literal {
                        let bits = match p.bump().cloned() {
                            Some(Tok::Float(b, _)) => b,
                            Some(Tok::Int(n)) => f64::from(n).to_bits(),
                            _ => unreachable!(),
                        };
                        LocalSpec::float_(size, Some(bits))
                    } else {
                        let rhs = parse_expr(p)?;
                        if let Some(v) = float_fold_value(&rhs, &p.local_specs) {
                            LocalSpec::float_nonliteral(size, v.to_bits())
                        } else {
                            // Non-foldable init (e.g. `double d = pi();`, a
                            // float-returning call): emit a synthetic assign;
                            // the local has no CONST temp. The assign lowers to
                            // the __fac receive-copy.
                            let local_idx = locals.len();
                            prelude.push(Stmt::Assign {
                                target: AssignTarget::Local(local_idx),
                                value: rhs,
                            });
                            LocalSpec::float_(size, None)
                        }
                    }
                } else {
                    LocalSpec::float_(size, None)
                };
                p.local_names.push(lname);
                p.local_specs.push(spec.clone());
                locals.push(spec);
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
                break;
            }
            p.eat(&Tok::Semi)?;
            continue;
        }
        loop {
            // Per-declarator `*` prefix for pointer locals, with
            // optional pointer-distance qualifiers (`far`/`near`/`huge`)
            // between the type and `*` (e.g. `int far *p`).
            // Peek ahead to detect far/huge before consuming modifiers.
            let is_far_or_huge = {
                let mut i = p.pos;
                let mut found = false;
                while i < p.toks.len() {
                    match &p.toks[i] {
                        Tok::Kw("far") | Tok::Kw("huge") => { found = true; break; }
                        Tok::Kw("near") | Tok::Kw("unsigned") | Tok::Kw("signed")
                        | Tok::Kw("static") | Tok::Kw("extern") | Tok::Kw("register")
                        | Tok::Kw("auto") | Tok::Kw("volatile") | Tok::Kw("const")
                        | Tok::Kw("short") | Tok::Kw("cdecl") | Tok::Kw("pascal")
                        | Tok::Kw("interrupt") => { i += 1; }
                        _ => break,
                    }
                }
                found
            };
            skip_decl_modifiers(p);
            let star_count = {
                let mut n = 0usize;
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); n += 1; }
                n
            };
            let is_far_ptr_decl = is_far_or_huge && star_count > 0;
            let lname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier in declaration, got {other:?}"
                    )));
                }
            };
            // Optional `[N]` for an array decl.
            let array_len = if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let k = parse_signed_int(p)?;
                if k <= 0 {
                    return Err(EmitError::Unsupported(format!(
                        "local array length must be positive, got {k}"
                    )));
                }
                p.eat(&Tok::RBrack)?;
                k as usize
            } else {
                1
            };
            let local_idx = locals.len();
            p.local_names.push(lname);
            // Long: 4-byte slot modeled as a 2-word "array". Reads via
            // `Expr::Local(idx)` pick up the low word at [bp-disp].
            let (slot_size, slot_len, is_long_slot) = if is_long_decl && array_len == 1 {
                (2usize, 2usize, true)
            } else if star_count > 0 {
                // A near pointer is a 2-byte slot regardless of pointee type
                // (so `char *p` stores its address as a word, not a byte).
                (2usize, array_len, false)
            } else {
                (size, array_len, false)
            };
            let spec = LocalSpec { size: slot_size, array_len: slot_len, init: None, struct_idx: None, is_long: is_long_slot, init_is_literal: false, is_far_ptr: is_far_ptr_decl, pointee_size: if star_count > 0 { size } else { 0 }, is_unsigned: has_unsigned && size == 1 && star_count == 0, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None };
            p.local_specs.push(spec.clone());
            locals.push(spec);
            if matches!(p.peek(), Some(Tok::Assign)) {
                p.bump();
                if array_len > 1 && matches!(p.peek(), Some(Tok::LBrace)) {
                    // `int a[N] = { v0, v1, ... };` — synthesize an
                    // a[i] = vi store for each element.
                    p.bump();
                    let mut i = 0usize;
                    while !matches!(p.peek(), Some(Tok::RBrace)) {
                        let value = parse_expr(p)?;
                        let byte_off = u16::try_from(i * size)
                            .expect("brace-init byte_off fits");
                        prelude.push(Stmt::Assign {
                            target: if size == 1 {
                                AssignTarget::IndexedLocalByte { local: local_idx, byte_off }
                            } else {
                                AssignTarget::IndexedLocal { local: local_idx, byte_off }
                            },
                            value,
                        });
                        i += 1;
                        if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
                    }
                    p.eat(&Tok::RBrace)?;
                } else if array_len > 1 && size == 1 && matches!(p.peek(), Some(Tok::StrLit(_))) {
                    // `char s[N] = "literal";` — break into per-byte
                    // stores so const-prop can see each element.
                    let bytes = match p.bump().cloned() {
                        Some(Tok::StrLit(b)) => b,
                        _ => unreachable!(),
                    };
                    let mut with_nul = bytes.clone();
                    with_nul.push(0);
                    for (i, b) in with_nul.iter().enumerate().take(array_len) {
                        prelude.push(Stmt::Assign {
                            target: AssignTarget::IndexedLocalByte {
                                local: local_idx,
                                byte_off: u16::try_from(i).expect("byte_off fits"),
                            },
                            value: Expr::IntLit(*b as i32),
                        });
                    }
                } else {
                    // Detect `(char) <expr>` explicit cast before
                    // parse_expr consumes the tokens. MSC generates
                    // `b0 imm8; 88 46 disp` (via AL) for these in the
                    // prologue vs `c6 46 disp imm8` for implicit/direct
                    // assigns. Fixture 1039 vs 1045.
                    let init_via_cast = matches!(p.toks.get(p.pos), Some(Tok::LParen))
                        && matches!(p.toks.get(p.pos + 1), Some(Tok::Kw("char")))
                        && matches!(p.toks.get(p.pos + 2), Some(Tok::RParen));
                    // Detect any `( [qualifier*] type-kw [*...] )` cast prefix.
                    // When the init is a type-cast of another local, the cast is
                    // erased in the AST (parse_atom returns the inner Local),
                    // making it look like a direct-alias init and triggering
                    // chained_literal. But MSC does NOT propagate const values
                    // through casts: `unsigned int u = (unsigned int)x` still
                    // emits a runtime load of u. Fixture 1855.
                    let init_via_type_cast = {
                        let mut i = p.pos;
                        let mut found = false;
                        if matches!(p.toks.get(i), Some(Tok::LParen)) {
                            i += 1;
                            while matches!(p.toks.get(i), Some(Tok::Kw("unsigned" | "signed" | "short" | "long" | "far" | "near" | "huge"))) { i += 1; }
                            if matches!(p.toks.get(i), Some(Tok::Kw("int" | "char" | "long"))) {
                                i += 1;
                                while matches!(p.toks.get(i), Some(Tok::Kw("int" | "unsigned" | "signed" | "short" | "long" | "far" | "near" | "huge"))) { i += 1; }
                                while matches!(p.toks.get(i), Some(Tok::Star)) { i += 1; }
                                if matches!(p.toks.get(i), Some(Tok::RParen)) { found = true; }
                            }
                        }
                        found
                    };
                    let init_expr = parse_expr(p)?;
                    // Postfix `++`/`--` on the init expression — yields
                    // the *current* value (which is what we already
                    // have in init_expr), then increments the target.
                    // Supported only when init_expr is a bare lvalue
                    // (Local or Global). Fixtures 1154, 1265.
                    if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
                        let inc = matches!(p.peek(), Some(Tok::PlusPlus));
                        let op = if inc { BinOp::Add } else { BinOp::Sub };
                        let post_target = match &init_expr {
                            Expr::Local(i) => Some(AssignTarget::Local(*i)),
                            Expr::Global(g) => Some(AssignTarget::Global(*g)),
                            _ => None,
                        };
                        if let Some(target) = post_target {
                            p.bump(); // consume `++`/`--`
                            let lvalue_expr = match &target {
                                AssignTarget::Local(i) => Expr::Local(*i),
                                AssignTarget::Global(g) => Expr::Global(*g),
                                _ => unreachable!(),
                            };
                            prelude.push(Stmt::Assign {
                                target,
                                value: Expr::BinOp {
                                    op,
                                    left: Box::new(lvalue_expr),
                                    right: Box::new(Expr::IntLit(1)),
                                },
                            });
                        }
                    }
                    let init_view: Vec<Option<i32>> = locals
                        .iter()
                        .take(local_idx)
                        .map(|l| l.init)
                        .collect();
                    // MSC never const-folds || / && into a compile-time
                    // literal (fixture 1466). For ternary: only skip fold
                    // when the condition is a non-comparison truthy check
                    // (e.g. `a ? b : c` with a a local). When the condition
                    // is a comparison operator, fold normally (fixture 1156).
                    let skip_fold = matches!(&init_expr,
                        Expr::BinOp { op: BinOp::LogOr | BinOp::LogAnd, .. })
                        || matches!(&init_expr, Expr::Ternary { cond, .. }
                            if !matches!(cond.as_ref(), Expr::BinOp {
                                op: BinOp::Eq | BinOp::Ne | BinOp::Lt
                                    | BinOp::Le | BinOp::Gt | BinOp::Ge, ..
                            }));
                    let fold_k = if skip_fold { None } else { init_expr.fold(&init_view) };
                    if let Some(k) = fold_k {
                        locals[local_idx].init = Some(k);
                        // Mirror into the parser's snapshot so later
                        // stmt-level lookups (a[i] with i known) see
                        // the init value.
                        if let Some(spec) = p.local_specs.get_mut(local_idx) {
                            spec.init = Some(k);
                        }
                        let pure_literal = init_expr.fold(&[]).is_some();
                        let chained_literal = !init_via_type_cast && matches!(
                            &init_expr,
                            Expr::Local(li) if locals.get(*li)
                                .map(|l| l.init_is_literal && l.size == size)
                                .unwrap_or(false)
                        );
                        // Detect identity-operation inits: `int a = x << 0`
                        // where x's known-literal value equals the fold result.
                        // MSC simplifies these to `a = x` at the IR level, so
                        // `a` can be folded in later uses just like `x`.
                        // See `init_expr_has_matching_literal_leaf` for the rule.
                        let identity_literal = !init_via_type_cast
                            && init_expr_has_matching_literal_leaf(&init_expr, k, size, &locals);
                        locals[local_idx].init_is_literal = pure_literal || chained_literal || identity_literal;
                        locals[local_idx].init_via_cast = init_via_cast && size == 1;
                        locals[local_idx].init_via_type_cast = init_via_type_cast;
                        if let Some(spec) = p.local_specs.get_mut(local_idx) {
                            spec.init_is_literal = pure_literal || chained_literal || identity_literal;
                            spec.init_via_cast = init_via_cast && size == 1;
                            spec.init_via_type_cast = init_via_type_cast;
                        }
                    } else {
                        prelude.push(Stmt::Assign {
                            target: AssignTarget::Local(local_idx),
                            value: init_expr,
                        });
                    }
                }
            }
            if matches!(p.peek(), Some(Tok::Comma)) {
                p.bump();
                continue;
            }
            break;
        }
        p.eat(&Tok::Semi)?;
    }

    // Body statements until the closing `}`. Any synthetic assigns
    // from non-constant local inits run first.
    let mut body = prelude;
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        body.push(parse_stmt(p)?);
    }
    p.eat(&Tok::RBrace)?;

    // Float/double-returning functions: const-fold each `return <expr>` to a
    // float literal so the value materializes as a CONST temp for the __fac
    // return sequence (and survives the int-oriented const-prop pass, which
    // would otherwise truncate the operands).
    if return_float_width != 0 {
        let is_double = return_float_width == 8;
        for s in &mut body {
            if let Stmt::Return(e) = s
                && let Some(v) = float_fold_value(e, &p.local_specs)
            {
                *e = Expr::FloatLit(v.to_bits(), is_double);
            }
        }
    }
    // In an int-returning function, `return (int)(<float arith>)` — e.g.
    // `(int)(a * b)` with literal-double `a`/`b` — folds via float semantics to
    // an int literal. Gated to arithmetic that references a float operand so it
    // never alters pure-integer expressions; non-literal float operands return
    // None (they need runtime FP, not yet handled). Fixture 1751.
    if return_int {
        for s in &mut body {
            if let Stmt::Return(e) = s
                && matches!(e, Expr::BinOp { .. })
                && expr_involves_float(e, &p.local_specs)
                && let Some(v) = float_fold_literal(e, &p.local_specs)
            {
                *e = Expr::IntLit(v as i32);
            }
        }
    }

    let local_names = p.local_names.clone();
    Ok(Function { name, return_int, return_long, return_char, return_float_width, params, param_is_char, param_is_long, param_is_unsigned, param_float_width, param_pointee_size, locals, local_names, body })
}
pub(crate) fn parse_signed_int(p: &mut Parser<'_>) -> Result<i32, EmitError> {
    // Accept any compile-time constant expression — integer literal,
    // negated literal, or any binop chain that folds to a constant.
    // Used for global inits, brace inits, case labels, array sizes.
    let expr = parse_expr(p)?;
    expr.fold(&[]).ok_or_else(|| EmitError::Unsupported(
        format!("expected constant expression in init, got {expr:?}")
    ))
}
pub(crate) fn parse_stmt(p: &mut Parser<'_>) -> Result<Stmt, EmitError> {
    match p.peek() {
        Some(Tok::Kw("return")) => {
            p.bump();
            // `return;` (void) — no value. Codegen with IntLit(0) and
            // the return-emit path will ignore it for void functions.
            if matches!(p.peek(), Some(Tok::Semi)) {
                p.bump();
                return Ok(Stmt::Return(Expr::IntLit(0)));
            }
            let expr = parse_expr(p)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Return(expr))
        }
        Some(Tok::Kw("if")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::RParen)?;
            let then_branch = Box::new(parse_stmt(p)?);
            let else_branch = if matches!(p.peek(), Some(Tok::Kw("else"))) {
                p.bump();
                Some(Box::new(parse_stmt(p)?))
            } else {
                None
            };
            Ok(Stmt::If { cond, then_branch, else_branch })
        }
        Some(Tok::Kw("while")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::RParen)?;
            let body = Box::new(parse_stmt(p)?);
            Ok(Stmt::While { cond, body })
        }
        Some(Tok::Kw("do")) => {
            p.bump();
            let body = Box::new(parse_stmt(p)?);
            p.eat(&Tok::Kw("while"))?;
            p.eat(&Tok::LParen)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::RParen)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::DoWhile { body, cond })
        }
        Some(Tok::Kw("for")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            // Init is an assignment expression-statement without a
            // trailing semi (the semi is the for-syntax separator).
            // Allow comma-separated assigns in for-init / for-step
            // (`for (i = 0, s = 0; ...; i++, s += 1)`). Each gets
            // collected into a Stmt::Block for emit. Fixtures 1240,
            // 1827, 172, 1523.
            let init = {
                let first = parse_assign_no_semi(p)?;
                if matches!(p.peek(), Some(Tok::Comma)) {
                    let mut stmts = vec![first];
                    while matches!(p.peek(), Some(Tok::Comma)) {
                        p.bump();
                        stmts.push(parse_assign_no_semi(p)?);
                    }
                    Box::new(Stmt::Block(stmts))
                } else {
                    Box::new(first)
                }
            };
            p.eat(&Tok::Semi)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::Semi)?;
            let step = {
                let first = parse_assign_no_semi(p)?;
                if matches!(p.peek(), Some(Tok::Comma)) {
                    let mut stmts = vec![first];
                    while matches!(p.peek(), Some(Tok::Comma)) {
                        p.bump();
                        stmts.push(parse_assign_no_semi(p)?);
                    }
                    Box::new(Stmt::Block(stmts))
                } else {
                    Box::new(first)
                }
            };
            p.eat(&Tok::RParen)?;
            let body = Box::new(parse_stmt(p)?);
            Ok(Stmt::For { init, cond, step, body })
        }
        Some(Tok::Kw("switch")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            let scrutinee = parse_expr(p)?;
            p.eat(&Tok::RParen)?;
            p.eat(&Tok::LBrace)?;
            let mut cases: Vec<SwitchArm> = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                let value = match p.peek() {
                    Some(Tok::Kw("case")) => {
                        p.bump();
                        Some(parse_signed_int(p)?)
                    }
                    Some(Tok::Kw("default")) => {
                        p.bump();
                        None
                    }
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected `case` or `default` in switch, got {other:?}"
                        )));
                    }
                };
                p.eat(&Tok::Colon)?;
                let mut body = Vec::new();
                while !matches!(
                    p.peek(),
                    Some(Tok::Kw("case")) | Some(Tok::Kw("default")) | Some(Tok::RBrace)
                ) {
                    body.push(parse_stmt(p)?);
                }
                cases.push(SwitchArm { value, body });
            }
            p.eat(&Tok::RBrace)?;
            Ok(Stmt::Switch { scrutinee, cases })
        }
        Some(Tok::Kw("break")) => {
            p.bump();
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Break)
        }
        Some(Tok::Kw("continue")) => {
            p.bump();
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Continue)
        }
        Some(Tok::Semi) => {
            p.bump();
            Ok(Stmt::Empty)
        }
        Some(Tok::LBrace) => {
            // Block statement: `{ <stmt>* }`. Lowered as a Block
            // variant so caller (if/while/etc.) can treat it as one
            // statement.
            p.bump();
            let mut stmts = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                stmts.push(parse_stmt(p)?);
            }
            p.eat(&Tok::RBrace)?;
            Ok(Stmt::Block(stmts))
        }
        Some(Tok::Star) => {
            // `*<ident> = <expr>;` or `**<ident> = <expr>;` — store
            // through a pointer (single or double deref).
            p.bump();
            // `**<ident> = <expr>;` — double-deref store.
            if matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                let name = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => return Err(EmitError::Unsupported(format!(
                        "expected identifier after `**`, got {other:?}"
                    ))),
                };
                let target = if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                    AssignTarget::DoubleDerefGlobal(idx)
                } else if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                    AssignTarget::DoubleDerefLocal(idx)
                } else {
                    return Err(EmitError::Unsupported(format!(
                        "double-deref store through `{name}` not yet supported"
                    )));
                };
                let value = if let Some(v) = parse_compound_rhs(p, &target)? {
                    v
                } else {
                    p.eat(&Tok::Assign)?;
                    parse_expr(p)?
                };
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            let target_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `*`, got {other:?}"
                    )));
                }
            };
            // `*p++ = v;` — store through old pointer then advance.
            if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
                if let Some(local_idx) = p.local_names.iter().position(|n| *n == target_name) {
                    let step_sign = if matches!(p.peek(), Some(Tok::PlusPlus)) { 1i32 } else { -1i32 };
                    p.bump();
                    let ptsz = p.local_specs[local_idx].pointee_size;
                    let step = step_sign * if ptsz > 0 { ptsz as i32 } else { 1 };
                    p.eat(&Tok::Assign)?;
                    let value = parse_expr(p)?;
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign {
                        target: AssignTarget::DerefPostMutateLocal { local_idx, step },
                        value,
                    });
                }
            }
            let target = if let Some(idx) = p.local_names.iter().position(|n| *n == target_name) {
                AssignTarget::DerefLocal(idx)
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == target_name) {
                AssignTarget::DerefGlobal(idx)
            } else if let Some(idx) = p.param_names.iter().position(|n| *n == target_name) {
                AssignTarget::DerefParam(idx)
            } else {
                return Err(EmitError::Unsupported(format!(
                    "deref-store through unknown identifier `{target_name}`"
                )));
            };
            // Compound `*p += K;` etc. — synthesize as `*p = *p op K;`.
            // Detection via parse_compound_rhs with a synthetic lvalue.
            if let Some(value) = parse_compound_rhs(p, &target)? {
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            p.eat(&Tok::Assign)?;
            let value = parse_expr(p)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Assign { target, value })
        }
        Some(Tok::Ident(_)) => {
            // Detect an expression-statement (`<ident> <relop> ... ? : ;`
            // discards its result for side effects). The lookahead at
            // position+1 distinguishes ordinary statements (= ( [ . ->
            // ++ -- += -= etc.) from full expressions.
            let next = p.toks.get(p.pos + 1);
            let is_stmt_form = matches!(next,
                Some(Tok::Assign) | Some(Tok::LParen) | Some(Tok::LBrack)
                | Some(Tok::Dot) | Some(Tok::Arrow)
                | Some(Tok::PlusPlus) | Some(Tok::MinusMinus)
                | Some(Tok::PlusEq) | Some(Tok::MinusEq) | Some(Tok::StarEq)
                | Some(Tok::SlashEq) | Some(Tok::PercentEq)
                | Some(Tok::AndEq) | Some(Tok::PipeEq) | Some(Tok::CaretEq)
                | Some(Tok::ShlEq) | Some(Tok::ShrEq));
            if !is_stmt_form {
                // Treat the whole thing as an expression — typically a
                // discarded ternary (fixture 1202) or a call-as-expr.
                let expr = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::ExprStmt(expr));
            }
            // Either `<local> = <expr>;` (assignment) or
            // `<name>();` (call as an expression statement).
            // Peek ahead one token to disambiguate.
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                _ => unreachable!(),
            };
            if matches!(p.peek(), Some(Tok::LParen)) {
                p.bump(); // (
                let args = parse_call_args(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::ExprStmt(Expr::Call { name, args }));
            }
            // `<struct-global>.<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Dot))
                && let Some(global_idx) = p.global_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.globals[global_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::GlobalField { global: global_idx, byte_off, size },
                    value,
                });
            }
            // `<struct-ptr-global>-><field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Arrow))
                && let Some(global_idx) = p.global_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.globals[global_idx].struct_idx
                && p.globals[global_idx].is_pointer
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                let target = AssignTarget::DerefGlobalField { ptr_global: global_idx, byte_off, size };
                if let Some(value) = parse_compound_rhs(p, &target)? {
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            // `<struct-local>.<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Dot))
                && let Some(local_idx) = p.local_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.local_specs[local_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::LocalField { local: local_idx, byte_off, size },
                    value,
                });
            }
            // `<struct-ptr-param>-><field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Arrow))
                && let Some(param_idx) = p.param_names.iter().position(|n| *n == name)
                && let Some(Some(sidx)) = p.param_struct_idxs.get(param_idx).cloned()
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                let target = AssignTarget::DerefParamField { ptr_param: param_idx, byte_off, size };
                if let Some(value) = parse_compound_rhs(p, &target)? {
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            // `<struct-ptr-local>-><field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Arrow))
                && let Some(local_idx) = p.local_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.local_specs[local_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::DerefLocalField { ptr_local: local_idx, byte_off, size },
                    value,
                });
            }
            // `<local-array>[K] = <expr>;` (and compound shapes:
            // `+=`, `-=`, `*=`, `++`, `--`, etc.) — indexed local
            // array store.
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(local_idx) = p.local_names.iter().position(|n| *n == name)
            {
                p.bump(); // [
                let index_expr = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                // Try folding against the local-init view so simple
                // `a[i] = ...` with `i = K` known at decl folds.
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let elem_bytes = p.local_specs[local_idx].size;
                // A POINTER local: `p[0] = v` is `*p = v` (deref store), so the
                // alias pass can redirect it to the pointee. (Non-zero indices
                // through a pointer local are deferred.)
                let ptsz = p.local_specs[local_idx].pointee_size;
                if ptsz > 0 && matches!(index_expr.fold(&init_view), Some(0)) {
                    let target = AssignTarget::DerefLocal(local_idx);
                    let value = if let Some(v) = parse_compound_rhs(p, &target)? {
                        v
                    } else {
                        p.eat(&Tok::Assign)?;
                        parse_expr(p)?
                    };
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                // Pointer local with a constant non-zero index: `p[K] = v` is a
                // store through `p + K*pointee`. The const-prop alias pass turns
                // this into a direct array-element store when p aliases a base.
                if ptsz > 0 && let Some(k) = index_expr.fold(&init_view) {
                    let byte_off = u16::try_from((k as i64) * (ptsz as i64))
                        .expect("ptr-subscript byte offset fits");
                    let target = AssignTarget::DerefLocalOffset {
                        local: local_idx, byte_off, is_byte: ptsz == 1,
                    };
                    p.eat(&Tok::Assign)?;
                    let value = parse_expr(p)?;
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                if let Some(k) = index_expr.fold(&init_view) {
                    // Constant index — use existing byte-offset forms.
                    let byte_off = u16::try_from((k as i64) * (elem_bytes as i64))
                        .expect("indexed-store byte offset fits");
                    let target = if elem_bytes == 1 {
                        AssignTarget::IndexedLocalByte { local: local_idx, byte_off }
                    } else {
                        AssignTarget::IndexedLocal { local: local_idx, byte_off }
                    };
                    let value = if let Some(v) = parse_compound_rhs_for_indexed(
                        p, local_idx, byte_off, elem_bytes == 1, false,
                    )? {
                        v
                    } else {
                        p.eat(&Tok::Assign)?;
                        parse_expr(p)?
                    };
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                // Runtime (non-constant) index — `a[i] = expr`.
                // Requires SI register; Frame::WithSlideSi is chosen
                // in emit_function when these targets appear.
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                let target = if elem_bytes == 1 {
                    AssignTarget::IndexedLocalByteVar { local: local_idx, index: Box::new(index_expr) }
                } else {
                    AssignTarget::IndexedLocalVar { local: local_idx, index: Box::new(index_expr) }
                };
                return Ok(Stmt::Assign { target, value });
            }
            // `<global>[K] = <expr>;` — indexed array store.
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(array_idx) = p.global_names.iter().position(|n| *n == name)
            {
                p.bump(); // [
                let index_expr = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let k = index_expr.fold(&[]).ok_or_else(|| EmitError::Unsupported(
                    "non-constant array index in store not yet supported".to_owned(),
                ))?;
                let g = &p.globals[array_idx];
                let target = if g.is_pointer {
                    // `<ptr>[K] = ...` — load pointer then store at
                    // offset. Phase 1 covers the `char *p` byte form.
                    let disp = i8::try_from(k).expect("ptr index fits in i8");
                    AssignTarget::PtrIndexByte { ptr: array_idx, disp }
                } else {
                    let elem_bytes = g.element_size;
                    let byte_off = u16::try_from((k as i64) * (elem_bytes as i64))
                        .expect("indexed-store byte offset fits");
                    if elem_bytes == 1 {
                        AssignTarget::IndexedGlobalByte { array: array_idx, byte_off }
                    } else {
                        AssignTarget::IndexedGlobal { array: array_idx, byte_off }
                    }
                };
                // Compound `op=` / `++`/`--` desugars to a self-referential BinOp
                // value; plain `=` falls through.
                let value = if let Some(v) = parse_compound_rhs(p, &target)? {
                    v
                } else {
                    p.eat(&Tok::Assign)?;
                    parse_expr(p)?
                };
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            let target = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                AssignTarget::Local(idx)
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                AssignTarget::Global(idx)
            } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                AssignTarget::Param(idx)
            } else {
                return Err(EmitError::Unsupported(format!(
                    "assignment to unknown identifier `{name}`"
                )));
            };
            // Compound forms: `x++`, `x--`, `x += K`, `x -= K`, ...
            // All rewrite to `Stmt::Assign { target, value: <existing target> <op> <rhs> }`.
            // The existing local/global codegen + peephole take it from there.
            if let Some(value) = parse_compound_rhs(p, &target)? {
                // `a += b++` — the compound RHS is itself a postfix
                // expression. Wrap the assign + subsequent self-inc in
                // a Block (fixture 1347).
                if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus))
                    && let Expr::BinOp { right, .. } = &value
                    && matches!(right.as_ref(), Expr::Local(_) | Expr::Global(_) | Expr::Param(_))
                {
                    let inc = matches!(p.peek(), Some(Tok::PlusPlus));
                    p.bump();
                    p.eat(&Tok::Semi)?;
                    let inner = (**right).clone();
                    let post_target = match &inner {
                        Expr::Local(i) => AssignTarget::Local(*i),
                        Expr::Param(i) => AssignTarget::Param(*i),
                        Expr::Global(g) => AssignTarget::Global(*g),
                        _ => unreachable!(),
                    };
                    let post_stmt = Stmt::Assign {
                        target: post_target,
                        value: Expr::BinOp {
                            op: if inc { BinOp::Add } else { BinOp::Sub },
                            left: Box::new(inner),
                            right: Box::new(Expr::IntLit(1)),
                        },
                    };
                    return Ok(Stmt::Block(vec![
                        Stmt::Assign { target, value },
                        post_stmt,
                    ]));
                }
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            p.eat(&Tok::Assign)?;
            let value = parse_expr(p)?;
            // `b = a++;` — post-inc/dec on the assigned value. The
            // RHS captures a's pre-update value; then a is updated.
            // We expand to `b = a; a = a ± 1;`. Fixtures 1244, 1154.
            if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus))
                && matches!(value, Expr::Local(_) | Expr::Global(_) | Expr::Param(_))
            {
                let inc = matches!(p.peek(), Some(Tok::PlusPlus));
                p.bump();
                p.eat(&Tok::Semi)?;
                let post_target = match &value {
                    Expr::Local(i) => AssignTarget::Local(*i),
                    Expr::Param(i) => AssignTarget::Param(*i),
                    Expr::Global(g) => AssignTarget::Global(*g),
                    _ => unreachable!(),
                };
                let post_lvalue = match &post_target {
                    AssignTarget::Local(i) => Expr::Local(*i),
                    AssignTarget::Param(i) => Expr::Param(*i),
                    AssignTarget::Global(g) => Expr::Global(*g),
                    _ => unreachable!(),
                };
                let post_stmt = Stmt::Assign {
                    target: post_target,
                    value: Expr::BinOp {
                        op: if inc { BinOp::Add } else { BinOp::Sub },
                        left: Box::new(post_lvalue),
                        right: Box::new(Expr::IntLit(1)),
                    },
                };
                return Ok(Stmt::Block(vec![
                    Stmt::Assign { target, value },
                    post_stmt,
                ]));
            }
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Assign { target, value })
        }
        Some(Tok::PlusPlus) | Some(Tok::MinusMinus) => {
            // `++<ident>;` / `--<ident>;` statement — equivalent to
            // `<ident>++;` / `<ident>--;` at the codegen level.
            let inc = matches!(p.peek(), Some(Tok::PlusPlus));
            p.bump();
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after prefix `++/--`, got {other:?}"
                    )));
                }
            };
            let target = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                AssignTarget::Local(idx)
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                AssignTarget::Global(idx)
            } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                AssignTarget::Param(idx)
            } else {
                return Err(EmitError::Unsupported(format!(
                    "prefix `++/--` of unknown identifier `{name}`"
                )));
            };
            p.eat(&Tok::Semi)?;
            let lvalue = match target {
                AssignTarget::Local(i) => Expr::Local(i),
                AssignTarget::Param(i) => Expr::Param(i),
                AssignTarget::Global(g) => Expr::Global(g),
                _ => unreachable!(),
            };
            Ok(Stmt::Assign {
                target,
                value: Expr::BinOp {
                    op: if inc { BinOp::Add } else { BinOp::Sub },
                    left: Box::new(lvalue),
                    right: Box::new(Expr::IntLit(1)),
                },
            })
        }
        Some(Tok::LParen) => {
            // Statement starting with `(` — typically a parenthesized
            // lvalue followed by `++/--` or `=`, e.g. `(*p)++;`.
            p.bump(); // (
            let inner = parse_expr(p)?;
            p.eat(&Tok::RParen)?;
            // Post-inc/dec on the deref expression.
            if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
                let inc = matches!(p.peek(), Some(Tok::PlusPlus));
                p.bump();
                p.eat(&Tok::Semi)?;
                let op = if inc { BinOp::Add } else { BinOp::Sub };
                if let Expr::DerefWord { ptr } | Expr::DerefByte { ptr } = &inner {
                    let target = match ptr.as_ref() {
                        Expr::Local(i) => AssignTarget::DerefLocal(*i),
                        Expr::Param(i) => AssignTarget::DerefParam(*i),
                        Expr::Global(g) => AssignTarget::DerefGlobal(*g),
                        _ => return Err(EmitError::Unsupported(format!(
                            "post-inc on (*<expr>) with non-ident inner ptr not supported"
                        ))),
                    };
                    return Ok(Stmt::Assign {
                        target,
                        value: Expr::BinOp {
                            op,
                            left: Box::new(inner.clone()),
                            right: Box::new(Expr::IntLit(1)),
                        },
                    });
                }
                return Err(EmitError::Unsupported(format!(
                    "post-inc on parenthesized non-deref: {inner:?}"
                )));
            }
            // Bare `(<expr>);` — discard, treat as expression statement.
            p.eat(&Tok::Semi)?;
            Ok(Stmt::ExprStmt(inner))
        }
        other => Err(EmitError::Unsupported(format!(
            "statement starting with {other:?} not yet supported"
        ))),
    }
}
/// Consume any leading type-qualifier / storage-class keywords that
/// our front-end currently treats as no-ops (`unsigned`, `signed`,
/// `static`, `extern`, `register`, `auto`, `volatile`, `const`,
/// `short`). Returns the count consumed so the caller can decide
/// whether a type prefix was present.
pub(crate) fn skip_decl_modifiers(p: &mut Parser<'_>) -> usize {
    let mut count = 0;
    while matches!(
        p.peek(),
        Some(Tok::Kw("unsigned"))
            | Some(Tok::Kw("signed"))
            | Some(Tok::Kw("static"))
            | Some(Tok::Kw("extern"))
            | Some(Tok::Kw("register"))
            | Some(Tok::Kw("auto"))
            | Some(Tok::Kw("volatile"))
            | Some(Tok::Kw("const"))
            | Some(Tok::Kw("short"))
            | Some(Tok::Kw("cdecl"))
            | Some(Tok::Kw("pascal"))
            | Some(Tok::Kw("far"))
            | Some(Tok::Kw("near"))
            | Some(Tok::Kw("huge"))
            | Some(Tok::Kw("interrupt"))
    ) {
        p.bump();
        count += 1;
    }
    count
}
/// Variant of `parse_compound_rhs` for indexed-array stores like
/// `a[K] += V` and `a[K]++`. The lvalue is reconstructed as
/// `Expr::Index{,Byte}` (global) or `Expr::LocalIndex{,Byte}` (local)
/// so the rewritten expression `a[K] op V` lowers through the
/// existing emit_binop path.
pub(crate) fn parse_compound_rhs_for_indexed(
    p: &mut Parser<'_>,
    container_idx: usize,
    byte_off: u16,
    is_byte: bool,
    is_global: bool,
) -> Result<Option<Expr>, EmitError> {
    let op = match p.peek() {
        Some(Tok::PlusPlus) => { p.bump(); BinOp::Add }
        Some(Tok::MinusMinus) => { p.bump(); BinOp::Sub }
        Some(Tok::PlusEq) => { p.bump(); BinOp::Add }
        Some(Tok::MinusEq) => { p.bump(); BinOp::Sub }
        Some(Tok::StarEq) => { p.bump(); BinOp::Mul }
        Some(Tok::SlashEq) => { p.bump(); BinOp::Div }
        Some(Tok::PercentEq) => { p.bump(); BinOp::Mod }
        Some(Tok::AndEq) => { p.bump(); BinOp::BitAnd }
        Some(Tok::PipeEq) => { p.bump(); BinOp::BitOr }
        Some(Tok::CaretEq) => { p.bump(); BinOp::BitXor }
        Some(Tok::ShlEq) => { p.bump(); BinOp::Shl }
        Some(Tok::ShrEq) => { p.bump(); BinOp::Shr }
        _ => return Ok(None),
    };
    let rhs = match op {
        BinOp::Add | BinOp::Sub
            if matches!(p.peek(), Some(Tok::Semi) | Some(Tok::Comma) | Some(Tok::RParen)) =>
        {
            Expr::IntLit(1)
        }
        _ => parse_expr(p)?,
    };
    let elem_size = if is_byte { 1 } else { 2 };
    let k = (byte_off as i64) / (elem_size as i64);
    let index = Box::new(Expr::IntLit(k as i32));
    let lvalue = if is_global {
        if is_byte {
            Expr::IndexByte { array: container_idx, index }
        } else {
            Expr::Index { array: container_idx, index }
        }
    } else if is_byte {
        Expr::LocalIndexByte { local: container_idx, index }
    } else {
        Expr::LocalIndex { local: container_idx, index }
    };
    Ok(Some(Expr::BinOp { op, left: Box::new(lvalue), right: Box::new(rhs) }))
}
/// Peek and parse a compound-assignment / post-(inc|dec) RHS for an
/// already-extracted target. Returns `Some(value)` for any compound
/// form, or `None` if the next token is just a plain `=` (the caller
/// then handles the normal `target = expr;` path). Each compound
/// form rewrites to an equivalent `Expr::BinOp(<lvalue>, <op>, rhs)`
/// so the existing target-store codegen + `x = x ± 1 → inc/dec`
/// peephole kick in for free.
pub(crate) fn parse_compound_rhs(p: &mut Parser<'_>, target: &AssignTarget) -> Result<Option<Expr>, EmitError> {
    let lvalue_expr = match target {
        AssignTarget::Local(i) => Expr::Local(*i),
        AssignTarget::Param(i) => Expr::Param(*i),
        AssignTarget::Global(g) => Expr::Global(*g),
        // Deref targets read via DerefWord/Byte (the pointee size
        // isn't tracked; default to word). Lets `*p += K` desugar.
        AssignTarget::DerefLocal(i) => Expr::DerefWord { ptr: Box::new(Expr::Local(*i)) },
        AssignTarget::DerefParam(i) => Expr::DerefWord { ptr: Box::new(Expr::Param(*i)) },
        AssignTarget::DerefGlobal(g) => Expr::DerefWord { ptr: Box::new(Expr::Global(*g)) },
        // Struct-field deref targets — the read uses the matching
        // DerefXField shape so `p->x++` desugars correctly.
        AssignTarget::DerefParamField { ptr_param, byte_off, size } => {
            Expr::DerefParamField { ptr_param: *ptr_param, byte_off: *byte_off, size: *size }
        }
        AssignTarget::DerefLocalField { ptr_local, byte_off, size } => {
            Expr::DerefLocalField { ptr_local: *ptr_local, byte_off: *byte_off, size: *size }
        }
        AssignTarget::DerefGlobalField { ptr_global, byte_off, size } => {
            Expr::DerefGlobalField { ptr_global: *ptr_global, byte_off: *byte_off, size: *size }
        }
        AssignTarget::GlobalField { global, byte_off, size } => {
            Expr::GlobalField { global: *global, byte_off: *byte_off, size: *size }
        }
        AssignTarget::LocalField { local, byte_off, size } => {
            Expr::LocalField { local: *local, byte_off: *byte_off, size: *size }
        }
        // Double-deref read `**pp` for `**pp op= K`.
        AssignTarget::DoubleDerefLocal(i) => {
            Expr::DerefWord { ptr: Box::new(Expr::DerefWord { ptr: Box::new(Expr::Local(*i)) }) }
        }
        AssignTarget::DoubleDerefGlobal(g) => {
            Expr::DerefWord { ptr: Box::new(Expr::DerefWord { ptr: Box::new(Expr::Global(*g)) }) }
        }
        // Global-pointer subscript `p[K] op= v` — the self-read is the matching
        // PtrIndexByte; const-prop rewrites both it and the target through the
        // p→array alias so the in-place mem-op peephole fires on the element.
        AssignTarget::PtrIndexByte { ptr, disp } => {
            Expr::PtrIndexByte { ptr: *ptr, index: Box::new(Expr::IntLit(*disp as i32)) }
        }
        // Direct global-array subscript `a[K] op= v` (the self-read is Index).
        AssignTarget::IndexedGlobal { array, byte_off } => {
            Expr::Index { array: *array, index: Box::new(Expr::IntLit((*byte_off / 2) as i32)) }
        }
        AssignTarget::IndexedGlobalByte { array, byte_off } => {
            Expr::IndexByte { array: *array, index: Box::new(Expr::IntLit(*byte_off as i32)) }
        }
        _ => return Ok(None),
    };
    let op = match p.peek() {
        Some(Tok::PlusPlus) => { p.bump(); BinOp::Add }
        Some(Tok::MinusMinus) => { p.bump(); BinOp::Sub }
        Some(Tok::PlusEq) => { p.bump(); BinOp::Add }
        Some(Tok::MinusEq) => { p.bump(); BinOp::Sub }
        Some(Tok::StarEq) => { p.bump(); BinOp::Mul }
        Some(Tok::SlashEq) => { p.bump(); BinOp::Div }
        Some(Tok::PercentEq) => { p.bump(); BinOp::Mod }
        Some(Tok::AndEq) => { p.bump(); BinOp::BitAnd }
        Some(Tok::PipeEq) => { p.bump(); BinOp::BitOr }
        Some(Tok::CaretEq) => { p.bump(); BinOp::BitXor }
        Some(Tok::ShlEq) => { p.bump(); BinOp::Shl }
        Some(Tok::ShrEq) => { p.bump(); BinOp::Shr }
        _ => return Ok(None),
    };
    let rhs = match op {
        BinOp::Add | BinOp::Sub
            if matches!(p.peek(), Some(Tok::Semi) | Some(Tok::Comma) | Some(Tok::RParen)) =>
        {
            // Post-(in|de)crement form: `x++;` / `x--;`. For far/huge
            // pointer locals the step is 2 (int element size); otherwise 1.
            let step = if let AssignTarget::Local(idx) = target {
                if p.local_specs.get(*idx).map(|s| s.is_far_ptr).unwrap_or(false) { 2 } else { 1 }
            } else { 1 };
            Expr::IntLit(step)
        }
        _ => parse_expr(p)?,
    };
    Ok(Some(Expr::BinOp {
        op,
        left: Box::new(lvalue_expr),
        right: Box::new(rhs),
    }))
}
/// Parse `<local> = <expr>` (no trailing `;`) — used inside
/// for-clauses where the semis are the for-syntax separators, not
/// statement terminators.
pub(crate) fn parse_assign_no_semi(p: &mut Parser<'_>) -> Result<Stmt, EmitError> {
    // Empty init/step in a for-loop (`for (; ...; ...)` etc.) — just
    // emit a no-op (Empty stmt).
    if matches!(p.peek(), Some(Tok::Semi) | Some(Tok::RParen)) {
        return Ok(Stmt::Empty);
    }
    // Prefix `++<ident>` / `--<ident>` in the for-step position.
    if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
        let inc = matches!(p.peek(), Some(Tok::PlusPlus));
        p.bump();
        let name = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected identifier after prefix `++/--` in for-step, got {other:?}"
                )));
            }
        };
        let (target, lvalue) = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
            (AssignTarget::Local(idx), Expr::Local(idx))
        } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
            (AssignTarget::Global(idx), Expr::Global(idx))
        } else {
            return Err(EmitError::Unsupported(format!(
                "prefix `++/--` of unknown identifier `{name}` in for-clause"
            )));
        };
        return Ok(Stmt::Assign {
            target,
            value: Expr::BinOp {
                op: if inc { BinOp::Add } else { BinOp::Sub },
                left: Box::new(lvalue),
                right: Box::new(Expr::IntLit(1)),
            },
        });
    }
    let name = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected identifier in assignment, got {other:?}"
            )));
        }
    };
    let target = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
        AssignTarget::Local(idx)
    } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
        AssignTarget::Global(idx)
    } else {
        return Err(EmitError::Unsupported(format!(
            "unknown identifier `{name}` in for-clause"
        )));
    };
    if let Some(value) = parse_compound_rhs(p, &target)? {
        return Ok(Stmt::Assign { target, value });
    }
    p.eat(&Tok::Assign)?;
    let value = parse_expr(p)?;
    Ok(Stmt::Assign { target, value })
}
pub(crate) fn parse_cond(p: &mut Parser<'_>) -> Result<Cond, EmitError> {
    // Empty cond (`for (;;)`) — model as a constant truthy.
    if matches!(p.peek(), Some(Tok::Semi) | Some(Tok::RParen)) {
        return Ok(Cond::Truthy(Expr::IntLit(1)));
    }
    let expr = parse_expr(p)?;
    Ok(cond_from_expr(expr))
}
/// Resolve `<expr>.<field>` or `<expr>-><field>` to its byte offset
/// and field size by looking up `field` in the struct definition at
/// `sidx`. Caller has already consumed `.` or `->`.
pub(crate) fn parse_field_lookup(p: &mut Parser<'_>, sidx: usize) -> Result<(u16, u8), EmitError> {
    let fname = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected field name, got {other:?}"
            )));
        }
    };
    let sdef = &p.structs[sidx];
    let field = sdef.fields.iter().find(|f| f.name == fname).ok_or_else(|| {
        EmitError::Unsupported(format!(
            "field `{fname}` not in struct `{}`", sdef.name
        ))
    })?;
    let (byte_off, size, inner) = (field.byte_off, field.size, field.struct_idx);
    // Multi-level access `o.inner.a`: recurse into the nested struct, summing
    // byte offsets; the returned size is the leaf field's.
    if let Some(inner_sidx) = inner
        && matches!(p.peek(), Some(Tok::Dot))
    {
        p.bump(); // .
        let (inner_off, inner_size) = parse_field_lookup(p, inner_sidx)?;
        return Ok((byte_off + inner_off, inner_size));
    }
    Ok((byte_off, size))
}
/// Convert a parsed expression into a Cond — recognizes `&&` / `||`
/// at the top level so emit_cond_skip can emit short-circuit
/// branches, and unwraps relational ops into `Cond::Cmp`.
pub(crate) fn cond_from_expr(expr: Expr) -> Cond {
    if let Expr::BinOp { op: BinOp::LogAnd, left, right } = expr {
        return Cond::And(
            Box::new(cond_from_expr(*left)),
            Box::new(cond_from_expr(*right)),
        );
    }
    if let Expr::BinOp { op: BinOp::LogOr, left, right } = expr {
        return Cond::Or(
            Box::new(cond_from_expr(*left)),
            Box::new(cond_from_expr(*right)),
        );
    }
    if let Expr::BinOp { op, left, right } = &expr {
        let rel = match op {
            BinOp::Eq => Some(RelOp::Eq),
            BinOp::Ne => Some(RelOp::Ne),
            BinOp::Lt => Some(RelOp::Lt),
            BinOp::Gt => Some(RelOp::Gt),
            BinOp::Le => Some(RelOp::Le),
            BinOp::Ge => Some(RelOp::Ge),
            _ => None,
        };
        if let Some(op) = rel {
            return Cond::Cmp {
                op,
                left: left.as_ref().clone(),
                right: right.as_ref().clone(),
            };
        }
    }
    Cond::Truthy(expr)
}
/// Expression parser — recognizes the Slice-4 shapes:
/// `<atom>` or `<atom> <op> <atom>` where op is `+ - *`.
pub(crate) fn parse_expr(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let cond = parse_binop_prec(p, 0)?;
    if matches!(p.peek(), Some(Tok::Quest)) {
        p.bump();
        let then_arm = parse_expr(p)?;
        p.eat(&Tok::Colon)?;
        let else_arm = parse_expr(p)?;
        return Ok(Expr::Ternary {
            cond: Box::new(cond),
            then_arm: Box::new(then_arm),
            else_arm: Box::new(else_arm),
        });
    }
    Ok(cond)
}
/// Operator-precedence climbing for the binary-operator chain. The
/// precedence table matches C's:
///
/// ```text
/// 12  *  /  %
/// 11  +  -
/// 10  << >>
///  9  <  <=  >  >=
///  8  == !=
///  7  &
///  6  ^
///  5  |
///  4  &&
///  3  ||
/// ```
pub(crate) fn parse_binop_prec(p: &mut Parser<'_>, min_prec: u8) -> Result<Expr, EmitError> {
    let mut left = parse_atom(p)?;
    loop {
        let (op, prec) = match p.peek() {
            Some(Tok::Star)    => (BinOp::Mul,    12),
            Some(Tok::Slash)   => (BinOp::Div,    12),
            Some(Tok::Percent) => (BinOp::Mod,    12),
            Some(Tok::Plus)    => (BinOp::Add,    11),
            Some(Tok::Minus)   => (BinOp::Sub,    11),
            Some(Tok::Shl)     => (BinOp::Shl,    10),
            Some(Tok::Shr)     => (BinOp::Shr,    10),
            Some(Tok::Lt)      => (BinOp::Lt,      9),
            Some(Tok::Le)      => (BinOp::Le,      9),
            Some(Tok::Gt)      => (BinOp::Gt,      9),
            Some(Tok::Ge)      => (BinOp::Ge,      9),
            Some(Tok::EqEq)    => (BinOp::Eq,      8),
            Some(Tok::NotEq)   => (BinOp::Ne,      8),
            Some(Tok::Amp)     => (BinOp::BitAnd,  7),
            Some(Tok::Caret)   => (BinOp::BitXor,  6),
            Some(Tok::Pipe)    => (BinOp::BitOr,   5),
            Some(Tok::AndAnd)  => (BinOp::LogAnd,  4),
            Some(Tok::OrOr)    => (BinOp::LogOr,   3),
            _ => break,
        };
        if prec < min_prec { break; }
        p.bump();
        let right = parse_binop_prec(p, prec + 1)?;
        left = Expr::BinOp { op, left: Box::new(left), right: Box::new(right) };
    }
    Ok(left)
}
/// Best-effort pointee-size inference for `*<expr>` lowering.
/// Returns the byte width of `*expr`. `char *` resolves to 1; `int *`
/// (and unrecognized shapes) to 2. Used by parse_atom to pick between
/// `DerefByte` and `DerefWord` variants. Parameters carry no type
/// info in Phase 1 so they default to int-pointer (word).
pub(crate) fn pointee_size_of(e: &Expr, globals: &[Global], locals: &[LocalSpec]) -> usize {
    match e {
        Expr::Global(idx) => globals[*idx].element_size,
        // A pointer local carries its pointee size (1 for `char *`, 2 for `int *`).
        Expr::Local(idx) => locals.get(*idx).map(|s| s.pointee_size).unwrap_or(0),
        // Postfix on a pointer: step magnitude = pointee element size.
        // step=±1 → char*, step=±2 → int*.
        Expr::PostMutateLocal { step, .. } | Expr::PostMutateGlobal { step, .. } => {
            step.unsigned_abs() as usize
        }
        Expr::BinOp { op: BinOp::Add, left, right } => {
            // `<ptr> + K` and `K + <ptr>` both inherit the pointer's
            // pointee size.
            match (left.as_ref(), right.as_ref()) {
                (Expr::Global(idx), _) | (_, Expr::Global(idx)) => {
                    globals[*idx].element_size
                }
                _ => 2,
            }
        }
        _ => 2,
    }
}
/// After parsing the lvalue and confirming the next token is `=`,
/// consume it and the RHS expression, returning a synthesized
/// `Stmt::Assign`. Used by comma-operator parsing.
pub(crate) fn parse_assign_tail(p: &mut Parser<'_>, lvalue: Expr) -> Result<Stmt, EmitError> {
    p.eat(&Tok::Assign)?;
    let value = parse_expr(p)?;
    let target = match lvalue {
        Expr::Local(i) => AssignTarget::Local(i),
        Expr::Param(i) => AssignTarget::Param(i),
        Expr::Global(g) => AssignTarget::Global(g),
        other => return Err(EmitError::Unsupported(format!(
            "assignment lvalue not supported: {other:?}"
        ))),
    };
    Ok(Stmt::Assign { target, value })
}
/// Identity for the comma-operator value path; future widening for
/// implicit type promotions can hook here.
pub(crate) fn expr_from_stmt_value(e: Expr) -> Expr { e }
pub(crate) fn parse_atom(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let tok = p.bump().cloned();
    match tok {
        Some(Tok::LParen) => {
            // `(type) <expr>` cast — recognize a type-keyword right
            // after `(` and treat the cast as identity (Phase 1
            // doesn't model signedness or narrowing semantics).
            skip_decl_modifiers(p);
            if matches!(p.peek(), Some(Tok::Kw("int")) | Some(Tok::Kw("char")) | Some(Tok::Kw("long"))
                | Some(Tok::Kw("float")) | Some(Tok::Kw("double"))) {
                p.bump();
                // Accept `long int`, then skip any pointer-distance
                // qualifiers (`far`/`near`/`huge`) that may appear between
                // the type and `*` (e.g. `(int far *)`), then skip `*`s.
                while matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
                skip_decl_modifiers(p);
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                p.eat(&Tok::RParen)?;
                return parse_atom(p);
            }
            let inner = parse_expr(p)?;
            // Comma operator: `(<ident> = <expr>, <expr>, ...)` or
            // `(<expr>, <expr>)`. Build a sequence of side-effect
            // statements followed by the value expression. Fixtures
            // 1057, 1114, 2234.
            if matches!(p.peek(), Some(Tok::Assign))
                && matches!(inner, Expr::Local(_) | Expr::Global(_) | Expr::Param(_))
            {
                let mut sides: Vec<Stmt> = Vec::new();
                let mut last = parse_assign_tail(p, inner)?;
                while matches!(p.peek(), Some(Tok::Comma)) {
                    p.bump();
                    sides.push(last);
                    let next = parse_expr(p)?;
                    if matches!(p.peek(), Some(Tok::Assign))
                        && matches!(next, Expr::Local(_) | Expr::Global(_) | Expr::Param(_))
                    {
                        last = parse_assign_tail(p, next)?;
                    } else {
                        // Final value expression: terminate the loop.
                        p.eat(&Tok::RParen)?;
                        let value = next;
                        return Ok(Expr::Seq { sides, value: Box::new(expr_from_stmt_value(value)) });
                    }
                }
                // Trailing `,<expr>)` case handled above; otherwise the
                // last assign IS the value.
                p.eat(&Tok::RParen)?;
                // Reduce: convert the last Assign Stmt to an Expr that
                // returns the assigned value. For simplicity we re-read
                // the target post-store. Common case: `(x = 5)` alone.
                if let Stmt::Assign { target, .. } = &last {
                    let val_expr = match target {
                        AssignTarget::Local(i) => Expr::Local(*i),
                        AssignTarget::Param(i) => Expr::Param(*i),
                        AssignTarget::Global(g) => Expr::Global(*g),
                        _ => return Err(EmitError::Unsupported(
                            "assign-tail value with unsupported target".to_owned()
                        )),
                    };
                    sides.push(last);
                    return Ok(Expr::Seq { sides, value: Box::new(val_expr) });
                }
                return Err(EmitError::Unsupported("expected comma-tail value".to_owned()));
            }
            if matches!(p.peek(), Some(Tok::Comma)) {
                let mut sides: Vec<Stmt> = Vec::new();
                let mut acc_value = inner;
                loop {
                    if !matches!(p.peek(), Some(Tok::Comma)) { break; }
                    p.bump();
                    sides.push(Stmt::ExprStmt(acc_value));
                    acc_value = parse_expr(p)?;
                }
                p.eat(&Tok::RParen)?;
                return Ok(Expr::Seq { sides, value: Box::new(acc_value) });
            }
            p.eat(&Tok::RParen)?;
            Ok(inner)
        }
        Some(Tok::Int(n)) => Ok(Expr::IntLit(n)),
        Some(Tok::Float(bits, double)) => Ok(Expr::FloatLit(bits, double)),
        Some(Tok::PlusPlus) => {
            let inner = parse_atom(p)?;
            match inner {
                Expr::Local(idx) => Ok(Expr::PreMutateLocal { local_idx: idx, step: 1 }),
                Expr::Global(idx) => Ok(Expr::PreMutateGlobal { global_idx: idx, step: 1 }),
                other => Ok(Expr::BinOp {
                    op: BinOp::Add,
                    left: Box::new(other),
                    right: Box::new(Expr::IntLit(1)),
                }),
            }
        }
        Some(Tok::MinusMinus) => {
            let inner = parse_atom(p)?;
            match inner {
                Expr::Local(idx) => Ok(Expr::PreMutateLocal { local_idx: idx, step: -1 }),
                Expr::Global(idx) => Ok(Expr::PreMutateGlobal { global_idx: idx, step: -1 }),
                other => Ok(Expr::BinOp {
                    op: BinOp::Sub,
                    left: Box::new(other),
                    right: Box::new(Expr::IntLit(1)),
                }),
            }
        }
        Some(Tok::Bang) => {
            // `!<expr>` — equivalent to `<expr> == 0`.
            let inner = parse_atom(p)?;
            Ok(Expr::BinOp {
                op: BinOp::Eq,
                left: Box::new(inner),
                right: Box::new(Expr::IntLit(0)),
            })
        }
        Some(Tok::Tilde) => {
            // `~<expr>` — bitwise complement via XOR with all-ones.
            let inner = parse_atom(p)?;
            Ok(Expr::BinOp {
                op: BinOp::BitXor,
                left: Box::new(inner),
                right: Box::new(Expr::IntLit(-1)),
            })
        }
        Some(Tok::StrLit(mut bytes)) => {
            // Intern the literal in the unit-level string pool with
            // the terminating NUL appended. Fixture 4103.
            bytes.push(0);
            let idx = p.strings.len();
            p.strings.push(bytes);
            Ok(Expr::StrLit(idx))
        }
        Some(Tok::Minus) => {
            // Unary minus: `- <atom>`. For literals, fold immediately;
            // otherwise lower to `0 - <atom>` for the existing
            // arithmetic codegen to handle.
            let inner = parse_atom(p)?;
            if let Expr::IntLit(n) = inner {
                Ok(Expr::IntLit(n.wrapping_neg()))
            } else {
                Ok(Expr::BinOp {
                    op: BinOp::Sub,
                    left: Box::new(Expr::IntLit(0)),
                    right: Box::new(inner),
                })
            }
        }
        Some(Tok::Kw("sizeof")) => {
            // `sizeof(<type>)` or `sizeof <expr>` — evaluated at
            // parse time into an int literal. We support int/char,
            // pointer-to-X (always 2 in small model), `struct Name`,
            // and a bare identifier (local / global storage size).
            let has_paren = matches!(p.peek(), Some(Tok::LParen));
            if has_paren { p.bump(); }
            let n = if let Some(Tok::Kw("struct")) = p.peek().cloned() {
                p.bump();
                let sname = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected struct name in sizeof, got {other:?}"
                        )));
                    }
                };
                p.structs.iter().find(|s| s.name == sname)
                    .map(|s| s.total_bytes as i32)
                    .ok_or_else(|| EmitError::Unsupported(format!("unknown struct `{sname}` in sizeof")))?
            } else if let Some(Tok::Kw("int")) = p.peek().cloned() {
                p.bump();
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                2 // int or any int-pointer = 2 in small model
            } else if let Some(Tok::Kw("char")) = p.peek().cloned() {
                p.bump();
                if matches!(p.peek(), Some(Tok::Star)) {
                    p.bump();
                    2
                } else {
                    1
                }
            } else if let Some(Tok::Kw("long")) = p.peek().cloned() {
                p.bump();
                if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                4 // long = 4 bytes; pointer-to-long still 2
            } else if let Some(Tok::Ident(name)) = p.peek().cloned() {
                p.bump();
                if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                    p.local_specs[idx].storage_bytes() as i32
                } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                    let g = &p.globals[idx];
                    (g.element_size * g.array_len) as i32
                } else {
                    return Err(EmitError::Unsupported(format!(
                        "sizeof unknown identifier `{name}`"
                    )));
                }
            } else {
                return Err(EmitError::Unsupported(format!(
                    "unsupported sizeof operand: {:?}", p.peek()
                )));
            };
            if has_paren { p.eat(&Tok::RParen)?; }
            Ok(Expr::IntLit(n))
        }
        Some(Tok::Amp) => {
            // Address-of `&<ident>` or `&<ident>[K]`. Phase 1 supports
            // globals and locals; locals lower to `lea ax, [bp-disp]`.
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `&`, got {other:?}"
                    )));
                }
            };
            // `&<ident>[K]` — address of an array element. Synthesize
            // `<base-addr> + K*elem_size` as a BinOp.
            if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let idx_expr = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                if let Some(local_idx) = p.local_names.iter().position(|n| *n == name) {
                    let elem_size = p.local_specs[local_idx].size as i32;
                    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                    let k = idx_expr.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
                        "non-constant index in `&<local>[K]` not yet supported".to_owned()
                    ))?;
                    let base_disp = -(elem_size * k);
                    let _ = base_disp;
                    // Synthesize: lea ax, [bp-disp_a + K*elem_size].
                    // We don't have a direct AST node for that; fall
                    // back to "address-of slot at element K".
                    // For now, use AddrOfLocal + IntLit offset binop.
                    return Ok(Expr::BinOp {
                        op: BinOp::Add,
                        left: Box::new(Expr::AddrOfLocal(local_idx)),
                        right: Box::new(Expr::IntLit(k * elem_size)),
                    });
                }
                if let Some(global_idx) = p.global_names.iter().position(|n| *n == name) {
                    let g = &p.globals[global_idx];
                    let elem_size = g.element_size as i32;
                    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                    let k = idx_expr.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
                        "non-constant index in `&<global>[K]` not yet supported".to_owned()
                    ))?;
                    return Ok(Expr::BinOp {
                        op: BinOp::Add,
                        left: Box::new(Expr::AddrOfGlobal(global_idx)),
                        right: Box::new(Expr::IntLit(k * elem_size)),
                    });
                }
                return Err(EmitError::Unsupported(format!(
                    "address-of unknown identifier `{name}`"
                )));
            }
            if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                return Ok(Expr::AddrOfLocal(idx));
            }
            let idx = p.global_names.iter().position(|n| *n == name)
                .ok_or_else(|| EmitError::Unsupported(format!(
                    "address-of unknown identifier `{name}`"
                )))?;
            Ok(Expr::AddrOfGlobal(idx))
        }
        Some(Tok::Star) => {
            // Unary deref `*<expr>`. Pick the byte- vs word-sized
            // variant from the inner expression's pointee type.
            let inner = parse_atom(p)?;
            let pointee_size = pointee_size_of(&inner, &p.globals, &p.local_specs);
            if pointee_size == 1 {
                Ok(Expr::DerefByte { ptr: Box::new(inner) })
            } else {
                Ok(Expr::DerefWord { ptr: Box::new(inner) })
            }
        }
        Some(Tok::Ident(name)) => {
            // Identifier may be a call site (`f(args)`), a local
            // reference, or a parameter reference. Disambiguate by
            // looking ahead for `(`.
            if matches!(p.peek(), Some(Tok::LParen)) {
                p.bump(); // (
                let args = parse_call_args(p)?;
                return Ok(Expr::Call { name, args });
            }
            // Enum constants substitute directly to their literal value.
            if let Some(&v) = p.enum_consts.get(&name) {
                return Ok(Expr::IntLit(v));
            }
            if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                // `<local>[<expr>]` — element access on a local
                // array. Picks the byte-load + cbw variant for char
                // arrays, word load otherwise.
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    // A POINTER local: `p[K]` is `*(p + K)` — a deref, not an
                    // array-slot read. K==0 yields a bare `*p` so the alias pass
                    // can fold it; K!=0 derefs `p + K*pointee`.
                    let ptsz = p.local_specs[idx].pointee_size;
                    if ptsz > 0 {
                        // Fold the index against the local-init view (mirrors the
                        // write side) so `p[i]` with a constant-init `i` scales by
                        // the pointee size here rather than slipping through as an
                        // unscaled runtime offset. Fixture 1339.
                        let init_view: Vec<Option<i32>> =
                            p.local_specs.iter().map(|l| l.init).collect();
                        let inner = match index.fold(&init_view) {
                            Some(0) => Expr::Local(idx),
                            Some(k) => Expr::BinOp {
                                op: BinOp::Add,
                                left: Box::new(Expr::Local(idx)),
                                right: Box::new(Expr::IntLit(k * ptsz as i32)),
                            },
                            None => Expr::BinOp {
                                op: BinOp::Add,
                                left: Box::new(Expr::Local(idx)),
                                right: Box::new(index),
                            },
                        };
                        return Ok(if ptsz == 1 {
                            Expr::DerefByte { ptr: Box::new(inner) }
                        } else {
                            Expr::DerefWord { ptr: Box::new(inner) }
                        });
                    }
                    return if p.local_specs[idx].size == 1 {
                        Ok(Expr::LocalIndexByte { local: idx, index: Box::new(index) })
                    } else {
                        Ok(Expr::LocalIndex { local: idx, index: Box::new(index) })
                    };
                }
                // `<struct-local>.<field>` member access.
                if matches!(p.peek(), Some(Tok::Dot))
                    && let Some(sidx) = p.local_specs[idx].struct_idx
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    return Ok(Expr::LocalField { local: idx, byte_off, size });
                }
                // `<struct-ptr-local>-><field>` member access.
                if matches!(p.peek(), Some(Tok::Arrow))
                    && let Some(sidx) = p.local_specs[idx].struct_idx
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    return Ok(Expr::DerefLocalField { ptr_local: idx, byte_off, size });
                }
                // Postfix `++`/`--` on a local — yields the OLD value
                // and schedules the mutation as a side effect in codegen.
                if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
                    let step_sign = if matches!(p.peek(), Some(Tok::PlusPlus)) { 1i32 } else { -1i32 };
                    p.bump();
                    let ptsz = p.local_specs[idx].pointee_size;
                    let step = step_sign * if ptsz > 0 { ptsz as i32 } else { 1 };
                    return Ok(Expr::PostMutateLocal { local_idx: idx, step });
                }
                // Array name in non-subscript/non-member position decays to its
                // base address (`int a[N]` → &a[0]). Scalars, pointers, longs,
                // and structs stay as plain values. Mirrors the global path.
                // Fixtures 1339, 2802, ...
                let s = &p.local_specs[idx];
                if s.array_len > 1 && s.pointee_size == 0 && !s.is_far_ptr
                    && s.struct_idx.is_none() && !s.is_long
                {
                    return Ok(Expr::AddrOfLocal(idx));
                }
                Ok(Expr::Local(idx))
            } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                // `<param>[K]` for `int *p` / `int p[]` parameters →
                // load ptr into BX, then word load `mov ax, [bx+K*2]`
                // (`8b 47 disp`). Phase 1 keeps the disp in disp8.
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    return Ok(Expr::ParamIndex { param: idx, index: Box::new(index) });
                }
                // `<struct-ptr-param>-><field>` member access.
                if matches!(p.peek(), Some(Tok::Arrow))
                    && let Some(Some(sidx)) = p.param_struct_idxs.get(idx).cloned()
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    return Ok(Expr::DerefParamField { ptr_param: idx, byte_off, size });
                }
                Ok(Expr::Param(idx))
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                // `<global>[<expr>]` — array index or pointer index.
                // Array (`int a[N]`): direct addressing.
                // Pointer (`char *p`): load pointer first, then offset.
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    let g = &p.globals[idx];
                    if g.is_pointer {
                        // Pointer-indexed read. Phase 1 covers the
                        // `char *p` byte form (fixture 4123).
                        Ok(Expr::PtrIndexByte { ptr: idx, index: Box::new(index) })
                    } else if g.element_size == 1 {
                        Ok(Expr::IndexByte { array: idx, index: Box::new(index) })
                    } else {
                        Ok(Expr::Index { array: idx, index: Box::new(index) })
                    }
                } else if matches!(p.peek(), Some(Tok::Dot))
                    && let Some(sidx) = p.globals[idx].struct_idx
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    Ok(Expr::GlobalField { global: idx, byte_off, size })
                } else if matches!(p.peek(), Some(Tok::Arrow))
                    && let Some(sidx) = p.globals[idx].struct_idx
                    && p.globals[idx].is_pointer
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    Ok(Expr::DerefGlobalField { ptr_global: idx, byte_off, size })
                } else {
                    // Postfix `++`/`--` on a global scalar or pointer.
                    if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
                        let step_sign = if matches!(p.peek(), Some(Tok::PlusPlus)) { 1i32 } else { -1i32 };
                        p.bump();
                        let g = &p.globals[idx];
                        let step = if g.is_pointer { step_sign * g.element_size as i32 } else { step_sign };
                        return Ok(Expr::PostMutateGlobal { global_idx: idx, step });
                    }
                    // Array name in non-subscript position decays to
                    // a pointer (the array's base address). Scalar
                    // globals stay as values.
                    let g = &p.globals[idx];
                    // A long ARRAY (element_size 4) decays like any array; a long
                    // SCALAR (the 2-word model, element_size 2) does not.
                    let is_array = g.array_len > 1 && !(g.is_long && g.element_size == 2);
                    if !g.is_pointer && is_array && g.struct_idx.is_none() {
                        Ok(Expr::AddrOfGlobal(idx))
                    } else {
                        Ok(Expr::Global(idx))
                    }
                }
            } else {
                Err(EmitError::Unsupported(format!("unknown identifier `{name}`")))
            }
        }
        other => Err(EmitError::Unsupported(format!(
            "expected atom, got {other:?}"
        ))),
    }
}
/// Parse the contents of `(<expr>, <expr>, ...)` for a call site —
/// caller has already consumed the opening `(`. Stops at and
/// consumes the closing `)`.
pub(crate) fn parse_call_args(p: &mut Parser<'_>) -> Result<Vec<Expr>, EmitError> {
    let mut args = Vec::new();
    if matches!(p.peek(), Some(Tok::RParen)) {
        p.bump();
        return Ok(args);
    }
    loop {
        args.push(parse_expr(p)?);
        match p.peek() {
            Some(Tok::Comma) => {
                p.bump();
            }
            Some(Tok::RParen) => {
                p.bump();
                return Ok(args);
            }
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected `,` or `)` in call args, got {other:?}"
                )));
            }
        }
    }
}
