use crate::*;

/// Parse Phase 1's source-shape envelope: a sequence of function
/// definitions, each `<ret-type> <name>(void) { <body> }`. `ret-type`
/// is `int` or `void`; bodies follow the existing per-statement
/// grammar.
pub(crate) fn parse_unit(source: &str) -> Result<Unit, EmitError> {
    let (preprocessed, had_include) = crate::preproc::preprocess(source)?;
    let mut toks = tokenize(&preprocessed)?;
    rewrite_void_pointers(&mut toks);
    apply_enum_substitutions(&mut toks);
    apply_typedef_substitutions(&mut toks);
    rewrite_fnptr_returns(&mut toks);
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
        param_pointee_sizes: Vec::new(),
        global_names: Vec::new(),
        globals: Vec::new(),
        global_dims: std::collections::HashMap::new(),
        local_dims: std::collections::HashMap::new(),
        param_dims: std::collections::HashMap::new(),
        structs: Vec::new(),
        last_field_bits: None,
        strings: Vec::new(),
        enum_consts: std::collections::HashMap::new(),
        typedefs: std::collections::HashMap::new(),
        int_cast_ptrs: std::collections::HashSet::new(),
        cast_ptr_pointee: None,
        fn_ptr_globals: std::collections::HashSet::new(),
        fn_ptr_params: std::collections::HashSet::new(),
        fn_ptr_locals: std::collections::HashSet::new(),
        fn_names: std::collections::HashSet::new(),
        block_scope_stack: Vec::new(),
        free_block_slots: Vec::new(),
        block_frame_max: 0,
        block_local_scopes: Vec::new(),
        fn_return_struct_idx: std::collections::HashMap::new(),
        fn_return_pointee: std::collections::HashMap::new(),
        param_dptr_elem: std::collections::HashMap::new(),
        ptr_array_stride: std::collections::HashMap::new(),
        struct_field_temp_count: 0,
    };
    let mut proto_long_params: std::collections::HashMap<String, Vec<bool>> = std::collections::HashMap::new();
    let mut proto_struct_params: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    let mut proto_struct_returns: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut proto_char_returns: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut prototyped_fns: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut variadic_fns: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Order in which function names FIRST appear in the source (prototype or
    // definition, whichever comes first). MSC lists functions in PUBDEF/EXTDEF
    // in this order — a forward-declared function (`int helper(int);` before
    // `main`) sorts ahead of its definition position. Fixtures 506/1762/3360.
    let mut fn_appearance: Vec<String> = Vec::new();
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
        if matches!(p.peek(), Some(Tok::Kw("struct")) | Some(Tok::Kw("union")))
            && matches!(p.toks.get(p.pos + 1), Some(Tok::Ident(_)))
            && matches!(p.toks.get(p.pos + 2), Some(Tok::LBrace))
        {
            let before = p.globals.len();
            parse_struct_def(&mut p)?;
            // Inline declarators (`struct X { ... } v = {...};`) create globals;
            // record them in declaration order so PUBDEF emission includes them.
            for i in before..p.globals.len() {
                decl_order.push(TopDecl::Global(i));
            }
            continue;
        }
        // `struct <Name>;` / `union <Name>;` — forward declaration. Register an
        // incomplete struct (no fields) so a following `struct <Name> *p;`
        // pointer global can resolve the tag; the full definition fills it in
        // later via parse_struct_def's update-in-place. Fixture 495.
        if matches!(p.peek(), Some(Tok::Kw("struct")) | Some(Tok::Kw("union")))
            && matches!(p.toks.get(p.pos + 1), Some(Tok::Ident(_)))
            && matches!(p.toks.get(p.pos + 2), Some(Tok::Semi))
        {
            let is_union = matches!(p.peek(), Some(Tok::Kw("union")));
            p.bump(); // struct / union
            let sname = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
            p.eat(&Tok::Semi)?;
            if !p.structs.iter().any(|s| s.name == sname) {
                p.structs.push(StructDef { name: sname, fields: Vec::new(), total_bytes: 0, is_union });
            }
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
                | Some(Tok::Kw("float")) | Some(Tok::Kw("double")) | Some(Tok::Kw("void"))
        ) || (k > p.pos && matches!(p.toks.get(k), Some(Tok::Ident(_))))
            || matches!(p.toks.get(k), Some(Tok::Kw("struct")) | Some(Tok::Kw("union")));
        if is_type_prefix {
            // Bare `unsigned`/`signed`/`short` (modifiers consumed, `k > p.pos`)
            // directly followed by an Ident whose next token starts a declarator
            // (`(`/`[`/`;`/`=`/`,`) → `k` is the NAME itself (implicit-int), not a
            // type token. e.g. `unsigned get_b(void)`. Otherwise `k` is a type
            // (keyword or typedef-name) and the name follows it.
            let bare_name_at_k = k > p.pos
                && matches!(p.toks.get(k), Some(Tok::Ident(_)))
                && matches!(p.toks.get(k + 1),
                    Some(Tok::LParen) | Some(Tok::LBrack) | Some(Tok::Semi)
                    | Some(Tok::Assign) | Some(Tok::Comma));
            // Walk past the type kw (plus the struct/union's name token if
            // it's a `struct <Name>` / `union <Name>` prefix) + optional `*`
            // to look at the declarator's first token after the name.
            let mut after = if bare_name_at_k { k } else { k + 1 };
            if !bare_name_at_k {
                if matches!(p.toks.get(k), Some(Tok::Kw("struct")) | Some(Tok::Kw("union"))) {
                    after += 1; // consume the struct/union's name
                }
                // Skip calling-convention / pointer-distance modifiers
                // (`int far helper(...)`).
                while matches!(p.toks.get(after),
                    Some(Tok::Kw("cdecl")) | Some(Tok::Kw("pascal"))
                    | Some(Tok::Kw("far")) | Some(Tok::Kw("near"))
                    | Some(Tok::Kw("huge")) | Some(Tok::Kw("interrupt"))
                ) { after += 1; }
                if matches!(p.toks.get(after), Some(Tok::Star)) { after += 1; }
            }
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
                // Record the prototype's long-param flags (so a call still pushes
                // long args as two words), then skip past `;`.
                if let Some(Tok::Ident(nm)) = p.toks.get(after) {
                    if !fn_appearance.contains(nm) {
                        fn_appearance.push(nm.clone());
                    }
                    prototyped_fns.insert(symbol_name(nm));
                    // Record the prototype's source position so the EXTDEF emitter
                    // can place a called extern before/after a tentative COMDEF
                    // global by declaration order. Fixtures 3602/3989/424.
                    decl_order.push(TopDecl::ExternProto(symbol_name(nm)));
                    // Variadic prototype: the param list ends with `...` (three
                    // `Dot` tokens). A long/float arg in a vararg position is
                    // pushed by natural width. Fixtures 2197/3983.
                    if (lparen_idx..close_idx).filter(|&j| matches!(p.toks.get(j), Some(Tok::Dot))).count() >= 3 {
                        variadic_fns.insert(symbol_name(nm));
                    }
                    let longs = proto_param_longs(p.toks, lparen_idx, close_idx);
                    if longs.iter().any(|&b| b) {
                        proto_long_params.insert(symbol_name(nm), longs);
                    }
                    let sbytes = proto_param_struct_bytes(p.toks, lparen_idx, close_idx, &p.structs);
                    if sbytes.iter().any(|&b| b > 0) {
                        proto_struct_params.insert(symbol_name(nm), sbytes);
                    }
                    // Struct-by-value return: `struct NAME f(...)` (no `*` between
                    // the struct name and the function name).
                    if matches!(p.toks.get(k), Some(Tok::Kw("struct")) | Some(Tok::Kw("union")))
                        && !((k + 2)..after).any(|j| matches!(p.toks.get(j), Some(Tok::Star)))
                        && let Some(Tok::Ident(sn)) = p.toks.get(k + 1)
                        && let Some(sidx) = p.structs.iter().position(|s| s.name == *sn)
                    {
                        proto_struct_returns.insert(symbol_name(nm), (p.structs[sidx].total_bytes + 1) & !1);
                        p.fn_return_struct_idx.insert(symbol_name(nm), sidx);
                    }
                    // `char NAME(...)` return (no `*` between the keyword and the
                    // name) — the call result widens via cbw. Fixture 3917.
                    if matches!(p.toks.get(k), Some(Tok::Kw("char")))
                        && !((k + 1)..after).any(|j| matches!(p.toks.get(j), Some(Tok::Star)))
                    {
                        proto_char_returns.insert(symbol_name(nm));
                    }
                }
                p.pos = close_idx + 2;
                continue;
            }
        }
        let fn_idx = functions.len();
        let parsed_fn = parse_function(&mut p)?;
        // Register the function name so a later function can take its address
        // by bare name (`apply(sq, 6)` → FuncAddr). Fixture 2314.
        p.fn_names.insert(parsed_fn.name.clone());
        if !fn_appearance.contains(&parsed_fn.name) {
            fn_appearance.push(parsed_fn.name.clone());
        }
        functions.push(parsed_fn);
        decl_order.push(TopDecl::Function(fn_idx));
    }
    // A function-less translation unit (only global declarations) is valid —
    // MSC emits an OBJ with just the data segments. Fixtures 3657/3659/3660/3680.
    Ok(Unit { globals: p.globals, structs: p.structs, functions, decl_order, strings: p.strings, proto_long_params, proto_struct_params, proto_struct_returns, prototyped_fns, proto_char_returns, fn_appearance, variadic_fns, had_include })
}
/// Parse a file-scope `struct <Name> <var> [= { ... }];` declaration.
/// Stores the struct global as if it were a `char` array sized to
/// the struct's total_bytes — that gives correct storage layout
/// without needing a separate Global::struct_idx field. Initializer
/// values are mapped to per-field byte slots.
/// Parse one `{ ... }` struct initializer group into flattened GlobalInit slots,
/// zero-padded to the struct's byte size. Handles nested-struct field braces,
/// string-literal pointer fields, and scalar fields.
pub(crate) fn parse_struct_brace_group(p: &mut Parser<'_>, sidx: usize, stotal: usize) -> Result<Vec<GlobalInit>, EmitError> {
    p.eat(&Tok::LBrace)?;
    let mut slots: Vec<GlobalInit> = Vec::new();
    let mut field_idx = 0usize;
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        let field = &p.structs[sidx].fields[field_idx];
        let field_size = field.size;
        while slots.iter().map(GlobalInit::size_bytes).sum::<usize>() < field.byte_off as usize {
            slots.push(GlobalInit::Byte(0));
        }
        match p.peek() {
            Some(Tok::LBrace) => {
                p.bump();
                while !matches!(p.peek(), Some(Tok::RBrace)) {
                    let v = parse_signed_int(p)?;
                    slots.push(GlobalInit::Int(v));
                    if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
                }
                p.eat(&Tok::RBrace)?;
            }
            Some(Tok::StrLit(_)) => {
                let bytes = match p.bump().cloned() { Some(Tok::StrLit(b)) => b, _ => unreachable!() };
                let mut with_nul = bytes.clone();
                with_nul.push(0);
                let str_idx = p.strings.len();
                p.strings.push(with_nul);
                slots.push(GlobalInit::StrAddr(str_idx));
            }
            // `&global` for a pointer field (`struct N a = {1, &b}`). Fixture 1419.
            Some(Tok::Amp) => {
                p.bump();
                let tn = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => return Err(EmitError::Unsupported(format!("expected `&<global>` in struct init, got {other:?}"))),
                };
                let ti = p.global_names.iter().position(|n| *n == tn)
                    .ok_or_else(|| EmitError::Unsupported(format!("address-of unknown global `{tn}`")))?;
                slots.push(GlobalInit::GlobalAddr(ti, 0));
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
    Ok(slots)
}
pub(crate) fn parse_struct_global_decl(p: &mut Parser<'_>, is_static: bool, is_const: bool, is_extern: bool) -> Result<(), EmitError> {
    // Accept either `struct <Tag> <var>;` or `union <Tag> <var>;` — both tags
    // live in the shared `p.structs` registry.
    if matches!(p.peek(), Some(Tok::Kw("union"))) { p.eat(&Tok::Kw("union"))?; }
    else { p.eat(&Tok::Kw("struct"))?; }
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
    // Comma-separated declarators share the `struct <Tag>` prefix:
    // `struct Pt a, b;` declares two globals. Fixtures 3349, 3612.
    loop {
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
    // `struct S arr[N];` — array of N structs (storage N*sizeof(S)).
    // `struct S *arr[N];` — array of N struct pointers (storage N*2). Fixture 3541.
    let mut elem_count = 1usize;
    if matches!(p.peek(), Some(Tok::LBrack)) {
        p.bump();
        let n = parse_signed_int(p)?;
        if n <= 0 {
            return Err(EmitError::Unsupported(format!("struct array length must be positive, got {n}")));
        }
        p.eat(&Tok::RBrack)?;
        elem_count = n as usize;
    }
    let is_ptr_array = is_pointer && elem_count > 1;
    let init = if matches!(p.peek(), Some(Tok::Assign)) {
        p.bump();
        if !is_pointer && elem_count > 1 && matches!(p.peek(), Some(Tok::LBrace)) {
            // `struct S arr[N] = {{...},{...}}` — one brace group per element.
            p.bump(); // outer `{`
            let mut all: Vec<GlobalInit> = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                all.extend(parse_struct_brace_group(p, sidx, stotal)?);
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
            }
            p.eat(&Tok::RBrace)?;
            while all.iter().map(GlobalInit::size_bytes).sum::<usize>() < stotal * elem_count {
                all.push(GlobalInit::Byte(0));
            }
            Some(all)
        } else if !is_pointer && matches!(p.peek(), Some(Tok::LBrace)) {
            Some(parse_struct_brace_group(p, sidx, stotal)?)
        } else if is_ptr_array && matches!(p.peek(), Some(Tok::LBrace)) {
            // `struct S *arr[N] = { &a, &b }` — one GlobalAddr per element.
            p.bump(); // `{`
            let mut vals = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                p.eat(&Tok::Amp)?;
                let tn = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => return Err(EmitError::Unsupported(format!("expected `&<global>` in struct-ptr-array init, got {other:?}"))),
                };
                let ti = p.global_names.iter().position(|n| *n == tn)
                    .ok_or_else(|| EmitError::Unsupported(format!("address-of unknown global `{tn}`")))?;
                vals.push(GlobalInit::GlobalAddr(ti, 0));
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
            }
            p.eat(&Tok::RBrace)?;
            while vals.len() < elem_count { vals.push(GlobalInit::Int(0)); }
            Some(vals)
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
            Some(vec![GlobalInit::GlobalAddr(target_idx, 0)])
        } else {
            return Err(EmitError::Unsupported(format!(
                "unsupported struct global init: {:?}", p.peek()
            )));
        }
    } else {
        None
    };
    // Pointer-array: N 2-byte pointer slots (array_len=N, element_size=2 → the
    // pointee struct is named by struct_idx). Struct array: byte storage.
    let array_len = if is_ptr_array { elem_count } else if is_pointer { 1 } else { stotal * elem_count };
    let element_size = if is_ptr_array { 2 } else { 1 }; // byte-oriented storage; fields by offset
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
        is_extern,
        is_unsigned: false,
        is_float: false,
        is_const,
    });
        // More declarators after a comma, else the terminating semicolon.
        if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
        break;
    }
    p.eat(&Tok::Semi)?;
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
/// Consume tokens up to (but not including) the `)` that closes the
/// current parenthesized group — used to discard a `sizeof` operand
/// without evaluating it. Nested parens are balanced.
fn skip_balanced_to_rparen(p: &mut Parser<'_>) {
    let mut depth = 0i32;
    loop {
        match p.peek() {
            Some(Tok::LParen) => { depth += 1; p.bump(); }
            Some(Tok::RParen) if depth == 0 => break,
            Some(Tok::RParen) => { depth -= 1; p.bump(); }
            None => break,
            _ => { p.bump(); }
        }
    }
}
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
    // `union` shares the struct path: identical field syntax, but every
    // member sits at offset 0 and the total size is the largest member.
    let is_union = matches!(p.peek(), Some(Tok::Kw("union")));
    if is_union { p.eat(&Tok::Kw("union"))?; } else { p.eat(&Tok::Kw("struct"))?; }
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
    // Bit-field packing state: `bit_unit_off` is the byte offset of the 16-bit
    // unit currently being filled (None = no open unit), `bit_pos` the bits used
    // in it. Opening a unit reserves its 2 bytes in `cursor` immediately.
    let mut bit_unit_off: Option<u16> = None;
    let mut bit_pos: u8 = 0;
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        // Scan the field's leading modifier run for `unsigned` before consuming it.
        let had_unsigned = {
            let mut j = p.pos;
            let mut found = false;
            while let Some(Tok::Kw(k)) = p.toks.get(j) {
                if !is_decl_modifier_kw(k) { break; }
                if *k == "unsigned" { found = true; }
                j += 1;
            }
            found
        };
        let had_modifiers = skip_decl_modifiers(p) > 0;
        // A field may be a nested `struct <Name>` (value or pointer).
        let mut field_struct_idx: Option<usize> = None;
        let size: u8 = if matches!(p.peek(), Some(Tok::Kw("struct"))) {
            p.bump();
            let inner_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => return Err(EmitError::Unsupported(format!(
                    "expected struct name for nested field, got {other:?}"))),
            };
            if inner_name == sname {
                // Self-referential field `struct N *next;` — `N` isn't in the
                // registry yet (it's being defined now); its index is the slot
                // it will occupy. Must be a pointer (a struct can't contain
                // itself by value), so its size is the 2-byte pointer. Fixtures
                // 1419, 1928, 2310.
                field_struct_idx = Some(p.structs.len());
                2
            } else {
                let inner = p.structs.iter().position(|s| s.name == inner_name).ok_or_else(|| {
                    EmitError::Unsupported(format!("unknown nested struct `{inner_name}`"))
                })?;
                field_struct_idx = Some(inner);
                u8::try_from(p.structs[inner].total_bytes).expect("nested struct fits in u8")
            }
        } else {
            match p.peek() {
                Some(Tok::Kw("int")) => { p.bump(); 2 }
                Some(Tok::Kw("char")) => { p.bump(); 1 }
                Some(Tok::Kw("long")) => { p.bump(); 4 }
                // Bare `unsigned`/`signed`/`short` (no explicit `int`) — e.g.
                // `unsigned a : 3;`. The modifier was already consumed; the type
                // defaults to int (2 bytes).
                _ if had_modifiers => 2,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "struct field type not yet supported: {other:?}"
                    )));
                }
            }
        };
        // Function-pointer field `<ret> (*name)(params)` — a 2-byte near pointer.
        // Consume the `(*name)(params)` declarator; the field is a plain pointer.
        // Fixtures 2378, 1812.
        let mut fnptr_field_name: Option<String> = None;
        if matches!(p.peek(), Some(Tok::LParen))
            && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
            && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
            && matches!(p.toks.get(p.pos + 3), Some(Tok::RParen))
            && matches!(p.toks.get(p.pos + 4), Some(Tok::LParen))
        {
            p.bump(); p.bump(); // `(` `*`
            let nm = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
            p.bump(); // `)`
            p.eat(&Tok::LParen)?;
            let mut depth = 1usize;
            while depth > 0 {
                match p.bump() {
                    Some(Tok::LParen) => depth += 1,
                    Some(Tok::RParen) => depth -= 1,
                    None => return Err(EmitError::Unsupported("unterminated fnptr field param list".to_owned())),
                    _ => {}
                }
            }
            fnptr_field_name = Some(nm);
        }
        let is_ptr = fnptr_field_name.is_some()
            || if matches!(p.peek(), Some(Tok::Star)) { p.bump(); true } else { false };
        // A pointer-to-struct field keeps its `struct_idx` (so `o->p->v` member
        // chains can resolve the target struct) but is flagged `is_pointer`, which
        // distinguishes it from an inline struct value at member-access sites.
        // Field name — optional for an anonymous bit-field (`unsigned : 0;`).
        let fname = if fnptr_field_name.is_some() {
            fnptr_field_name
        } else {
            match p.peek() {
                Some(Tok::Ident(s)) => { let s = s.clone(); p.bump(); Some(s) }
                _ => None,
            }
        };
        // Bit-field declarator `<name> : <width>;` (or anonymous `: <width>;`).
        if !is_ptr && field_struct_idx.is_none() && matches!(p.peek(), Some(Tok::Colon)) {
            p.bump();
            let width = parse_signed_int(p)? as u8;
            p.eat(&Tok::Semi)?;
            if width == 0 {
                // Zero-width: close the current unit (align next field to a new
                // unit). No named member.
                bit_unit_off = None;
                bit_pos = 0;
                continue;
            }
            let (unit_off, place_bit) = match bit_unit_off {
                Some(off) if bit_pos + width <= 16 => (off, bit_pos),
                _ => {
                    if cursor % 2 != 0 { cursor += 1; }
                    let off = u16::try_from(cursor).expect("bit-field unit offset fits");
                    cursor += 2;
                    bit_unit_off = Some(off);
                    (off, 0)
                }
            };
            bit_pos = place_bit + width;
            if let Some(name) = fname {
                fields.push(StructField {
                    name, byte_off: unit_off, size: 2, struct_idx: None,
                    bit_width: width, bit_off: place_bit, is_pointer: false, pointee_size: 0,
                    is_unsigned: false,
                });
            }
            continue;
        }
        // Ordinary field: close any open bit-field unit first (its 2 bytes are
        // already reserved in `cursor`).
        bit_unit_off = None;
        bit_pos = 0;
        let fname = fname.ok_or_else(|| EmitError::Unsupported(
            "expected struct field name".to_owned()))?;
        let elem_size = if is_ptr { 2 } else { size };
        // Array field `int v[N];` — element size stays `elem_size` (so `s.v[K]`
        // indexes correctly); the field spans elem_size*N bytes.
        let mut count = 1usize;
        if matches!(p.peek(), Some(Tok::LBrack)) {
            p.bump();
            let n = parse_signed_int(p)?;
            if n <= 0 {
                return Err(EmitError::Unsupported(format!("struct array field length must be positive, got {n}")));
            }
            p.eat(&Tok::RBrack)?;
            count = n as usize;
        }
        // Struct: word-align int/pointer/struct fields (char fields take the
        // next byte at any offset) and advance the cursor cumulatively. Union:
        // every field sits at offset 0; the total is the largest member span.
        let span = elem_size as usize * count;
        let byte_off = if is_union {
            cursor = cursor.max(span);
            0
        } else {
            if elem_size >= 2 && cursor % 2 != 0 { cursor += 1; }
            let off = u16::try_from(cursor).expect("field offset fits in u16");
            cursor += span;
            off
        };
        fields.push(StructField {
            name: fname,
            byte_off,
            size: elem_size,
            struct_idx: field_struct_idx,
            bit_width: 0,
            bit_off: 0,
            is_pointer: is_ptr,
            pointee_size: if is_ptr { size } else { 0 },
            is_unsigned: had_unsigned,
        });
        // Comma-separated declarators of the same base type: `int a, b, c;`.
        // Each may have its own `*` and `[N]`. Fixture 3612. (Bit-field commas
        // are not handled — those declare one member per `;`.)
        while matches!(p.peek(), Some(Tok::Comma)) {
            p.bump();
            let is_ptr2 = matches!(p.peek(), Some(Tok::Star));
            if is_ptr2 { p.bump(); }
            let fname2 = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => return Err(EmitError::Unsupported(format!(
                    "expected struct field name after comma, got {other:?}"))),
            };
            let elem_size2 = if is_ptr2 { 2 } else { size };
            let mut count2 = 1usize;
            if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let n = parse_signed_int(p)?;
                if n <= 0 { return Err(EmitError::Unsupported(format!("struct array field length must be positive, got {n}"))); }
                p.eat(&Tok::RBrack)?;
                count2 = n as usize;
            }
            let span2 = elem_size2 as usize * count2;
            let byte_off2 = if is_union {
                cursor = cursor.max(span2);
                0
            } else {
                if elem_size2 >= 2 && cursor % 2 != 0 { cursor += 1; }
                let off = u16::try_from(cursor).expect("field offset fits in u16");
                cursor += span2;
                off
            };
            fields.push(StructField {
                name: fname2, byte_off: byte_off2, size: elem_size2,
                struct_idx: field_struct_idx, bit_width: 0, bit_off: 0,
                is_pointer: is_ptr2, pointee_size: if is_ptr2 { size } else { 0 },
                is_unsigned: had_unsigned,
            });
        }
        p.eat(&Tok::Semi)?;
    }
    p.eat(&Tok::RBrace)?;
    // Round total up to the natural alignment (2 bytes for any
    // struct containing an int/pointer field; 1 byte otherwise).
    let needs_word_align = fields.iter().any(|f| f.size >= 2);
    let total_bytes = if needs_word_align { (cursor + 1) & !1 } else { cursor };
    // If a forward declaration (incomplete, no fields) of this tag already
    // exists, fill it in place so its registry index — already referenced by
    // any earlier `struct <Name> *p;` pointer global — stays valid. Fixture 495.
    let sidx = if let Some(i) = p.structs.iter().position(|s| s.name == sname && s.fields.is_empty()) {
        p.structs[i] = StructDef { name: sname, fields, total_bytes, is_union };
        i
    } else {
        let i = p.structs.len();
        p.structs.push(StructDef { name: sname, fields, total_bytes, is_union });
        i
    };
    // Inline declarator(s): `struct X { ... } v, *p, a[N];` — the `}` is
    // followed by variable declarators rather than `;`. Register each as a
    // file-scope global of this struct type. Fixtures 3322, 3419, 3420, 3446.
    if !matches!(p.peek(), Some(Tok::Semi)) {
        loop {
            let is_pointer = matches!(p.peek(), Some(Tok::Star));
            if is_pointer { p.bump(); }
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => return Err(EmitError::Unsupported(format!(
                    "expected declarator after struct definition, got {other:?}"))),
            };
            let mut elem_count = 1usize;
            if !is_pointer && matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let n = parse_signed_int(p)?;
                if n <= 0 {
                    return Err(EmitError::Unsupported(format!("struct array length must be positive, got {n}")));
                }
                p.eat(&Tok::RBrack)?;
                elem_count = n as usize;
            }
            let array_len = if is_pointer { 1 } else { total_bytes * elem_count };
            // Optional `= { ... }` initializer on the inline declarator
            // (`struct Pt { ... } p = {3,4};`). Reuse the struct brace-group
            // parser. Fixture 3341.
            let init = if !is_pointer && matches!(p.peek(), Some(Tok::Assign)) {
                p.bump();
                if matches!(p.peek(), Some(Tok::LBrace)) {
                    Some(parse_struct_brace_group(p, sidx, total_bytes)?)
                } else {
                    return Err(EmitError::Unsupported(
                        "non-brace initializer on inline struct declarator not yet supported".to_owned()));
                }
            } else {
                None
            };
            p.global_names.push(name.clone());
            p.globals.push(Global {
                name, init, array_len, element_size: 1, is_pointer,
                struct_idx: Some(sidx), is_long: false, is_static: false,
                is_extern: false, is_unsigned: false, is_float: false, is_const: false,
            });
            match p.peek() {
                Some(Tok::Comma) => { p.bump(); }
                _ => break,
            }
        }
    }
    p.eat(&Tok::Semi)?;
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
    let mut is_const = false;
    while let Some(t) = p.toks.get(i) {
        match t {
            Tok::Kw("static") => { is_static = true; i += 1; }
            Tok::Kw("extern") => { is_extern = true; i += 1; }
            Tok::Kw("unsigned") => { is_unsigned = true; i += 1; }
            Tok::Kw("const") => { is_const = true; i += 1; }
            Tok::Kw("signed")
                | Tok::Kw("register") | Tok::Kw("auto")
                | Tok::Kw("volatile")
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
    if matches!(p.peek(), Some(Tok::Kw("struct")) | Some(Tok::Kw("union"))) {
        return parse_struct_global_decl(p, is_static, is_const, is_extern);
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
    // Function-pointer ARRAY global: `<ret> (*name[N])(params) [= {f,...}];` —
    // an N-element array of 2-byte near pointers. Elements are called indirectly
    // (`name[i](args)`). Fixtures 2944, 3696.
    if matches!(p.peek(), Some(Tok::LParen))
        && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
        && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
        && matches!(p.toks.get(p.pos + 3), Some(Tok::LBrack))
    {
        p.bump(); p.bump(); // `(` `*`
        let name = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
        p.eat(&Tok::LBrack)?;
        let n = parse_signed_int(p)?;
        if n <= 0 {
            return Err(EmitError::Unsupported(format!("fnptr-array length must be positive, got {n}")));
        }
        p.eat(&Tok::RBrack)?;
        p.eat(&Tok::RParen)?; // close `(*name[N])`
        p.eat(&Tok::LParen)?; // parameter list
        let mut depth = 1usize;
        while depth > 0 {
            match p.bump() {
                Some(Tok::LParen) => depth += 1,
                Some(Tok::RParen) => depth -= 1,
                None => return Err(EmitError::Unsupported("unterminated fnptr-array parameter list".to_owned())),
                _ => {}
            }
        }
        // Optional `= { f0, f1, ... }` initializer of function addresses.
        let init = if matches!(p.peek(), Some(Tok::Assign)) {
            p.bump();
            p.eat(&Tok::LBrace)?;
            let mut vals = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                match p.bump().cloned() {
                    Some(Tok::Ident(s)) => vals.push(GlobalInit::FuncAddr(symbol_name(&s))),
                    Some(Tok::Int(0)) => vals.push(GlobalInit::Int(0)),
                    other => return Err(EmitError::Unsupported(format!(
                        "expected function name in fnptr-array initializer, got {other:?}"))),
                }
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
            }
            p.eat(&Tok::RBrace)?;
            while vals.len() < n as usize { vals.push(GlobalInit::Int(0)); }
            Some(vals)
        } else {
            None
        };
        p.eat(&Tok::Semi)?;
        p.global_names.push(name.clone());
        p.globals.push(Global {
            name, init, array_len: n as usize, element_size: 2, is_pointer: true,
            struct_idx: None, is_long: false, is_static, is_extern, is_unsigned: false, is_float: false, is_const,
        });
        return Ok(());
    }
    // Function-pointer global: `<ret> (*name)(params);` — a 2-byte near
    // pointer slot. The signature isn't modeled; the call site resolves it as
    // an indirect call. (Initializer `= func` deferred to a later slice.)
    if matches!(p.peek(), Some(Tok::LParen))
        && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
        && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
        && matches!(p.toks.get(p.pos + 3), Some(Tok::RParen))
        && matches!(p.toks.get(p.pos + 4), Some(Tok::LParen))
    {
        p.bump(); p.bump(); // `(` `*`
        let name = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
        p.bump(); // `)`
        // Skip the parameter-list parens (balanced).
        p.eat(&Tok::LParen)?;
        let mut depth = 1usize;
        while depth > 0 {
            match p.bump() {
                Some(Tok::LParen) => depth += 1,
                Some(Tok::RParen) => depth -= 1,
                None => return Err(EmitError::Unsupported("unterminated fnptr parameter list".to_owned())),
                _ => {}
            }
        }
        // Optional `= func` initializer → a _DATA word holding OFFSET _func.
        let init = if matches!(p.peek(), Some(Tok::Assign)) {
            p.bump();
            let fname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => return Err(EmitError::Unsupported(format!(
                    "expected function name initializing a function pointer, got {other:?}"))),
            };
            Some(vec![GlobalInit::FuncAddr(symbol_name(&fname))])
        } else {
            None
        };
        p.eat(&Tok::Semi)?;
        p.global_names.push(name.clone());
        p.fn_ptr_globals.insert(name.clone());
        p.globals.push(Global {
            name, init, array_len: 1, element_size: 2, is_pointer: true,
            struct_idx: None, is_long: false, is_static, is_extern, is_unsigned: false, is_float: false, is_const,
        });
        return Ok(());
    }
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
    // Optional `[N]` (or `[]` with init) for an array declaration, plus any
    // further dimensions for a multidimensional array (`int a[N][M]`). The total
    // element count (product of dims) determines the COMDEF / _DATA byte length;
    // `dims` is recorded so `a[i][j]` with constant indices folds to a flat
    // row-major offset.
    let mut implicit_array_len = false;
    let mut dims: Vec<usize> = Vec::new();
    let array_len = if matches!(p.peek(), Some(Tok::LBrack)) {
        p.bump();
        if matches!(p.peek(), Some(Tok::RBrack)) {
            // `int a[] = {...};` — size from init list count. Also
            // `int m[][N] = {{...},...}` — outer dim implicit, inner dims given.
            p.bump();
            implicit_array_len = true;
            while matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let m = parse_signed_int(p)?;
                if m <= 0 {
                    return Err(EmitError::Unsupported(format!("array length must be positive, got {m}")));
                }
                p.eat(&Tok::RBrack)?;
                dims.push(m as usize);
            }
            0 // placeholder; we'll overwrite after parsing init below
        } else {
        let k = parse_signed_int(p)?;
        if k <= 0 {
            return Err(EmitError::Unsupported(format!(
                "array length must be positive, got {k}"
            )));
        }
        p.eat(&Tok::RBrack)?;
        let mut total = k as usize;
        dims.push(k as usize);
        // Further dimensions: `[M][P]...`.
        while matches!(p.peek(), Some(Tok::LBrack)) {
            p.bump();
            let m = parse_signed_int(p)?;
            if m <= 0 {
                return Err(EmitError::Unsupported(format!("array length must be positive, got {m}")));
            }
            p.eat(&Tok::RBrack)?;
            dims.push(m as usize);
            total *= m as usize;
        }
        total
        }
    } else {
        1
    };
    let init = if matches!(p.peek(), Some(Tok::Assign)) {
        p.bump();
        if dims.len() > 1 && matches!(p.peek(), Some(Tok::LBrace)) {
            // Multidimensional array initializer (`int m[2][3] = {{...},{...}}`):
            // flatten row-major with per-level zero padding.
            Some(parse_multidim_init(p, &dims, is_char && !is_pointer)?)
        } else if implicit_array_len && !dims.is_empty() && matches!(p.peek(), Some(Tok::LBrace)) {
            // `int m[][N] = {{...},{...}}`: outer dim implicit — parse repeated
            // inner groups (each padded to the inner dims) and count them.
            p.bump(); // outer `{`
            let mut values = Vec::new();
            let mut outer = 0usize;
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                values.extend(parse_multidim_init(p, &dims, is_char && !is_pointer)?);
                outer += 1;
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
            }
            p.eat(&Tok::RBrace)?;
            let mut full = vec![outer];
            full.extend(dims.iter().copied());
            dims = full;
            Some(values)
        } else if matches!(p.peek(), Some(Tok::LBrace)) {
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
                if is_pointer {
                    // Array-of-pointers element: a string literal (`char
                    // *names[]={"a","b"}` → CONST `$SG` + StrAddr) or `&g`
                    // (`int *table[]={&a,&b}` → GlobalAddr). Fixtures 1394,
                    // 2345, 2608, 2860.
                    match p.peek() {
                        Some(Tok::StrLit(_)) => {
                            let bytes = match p.bump().cloned() {
                                Some(Tok::StrLit(b)) => b,
                                _ => unreachable!(),
                            };
                            let mut with_nul = bytes.clone();
                            with_nul.push(0);
                            let str_idx = p.strings.len();
                            p.strings.push(with_nul);
                            values.push(GlobalInit::StrAddr(str_idx));
                        }
                        Some(Tok::Amp) => {
                            p.bump();
                            let target_name = match p.bump().cloned() {
                                Some(Tok::Ident(s)) => s,
                                other => return Err(EmitError::Unsupported(format!(
                                    "expected identifier after `&` in initializer, got {other:?}"))),
                            };
                            let target_idx = p.global_names.iter().position(|n| *n == target_name)
                                .ok_or_else(|| EmitError::Unsupported(format!(
                                    "address-of unknown global `{target_name}`")))?;
                            values.push(GlobalInit::GlobalAddr(target_idx, 0));
                        }
                        // A bare integer is a null/numeric pointer constant
                        // (`int *p[] = { &v, 0 }`) — stored as a plain word.
                        // Fixture 3279.
                        Some(Tok::Int(_)) => {
                            let n = match p.bump().cloned() { Some(Tok::Int(n)) => n, _ => unreachable!() };
                            values.push(GlobalInit::Int(n as i32));
                        }
                        other => return Err(EmitError::Unsupported(format!(
                            "expected string literal, `&`, or integer in pointer-array initializer, got {other:?}"))),
                    }
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
                } else if is_long && !is_pointer {
                    // A `long` array element occupies two _DATA words (low, high),
                    // matching the scalar-long [Int(low), Int(high)] modeling.
                    values.push(GlobalInit::Int((v as u32 & 0xFFFF) as i32));
                    values.push(GlobalInit::Int((((v as u32) >> 16) & 0xFFFF) as i32));
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
            // Partial initializer (`int a[5] = {1,2}`): zero-fill to the declared
            // array length — MSC emits the trailing zeros explicitly in _DATA.
            // Fixtures 502, 2093, 2453.
            // A long array stores two _DATA words per element, so the slot
            // target is 2*array_len; non-long types use one slot per element.
            let slot_target = if is_long && !is_pointer { array_len * 2 } else { array_len };
            while values.len() < slot_target {
                values.push(if is_float && !is_pointer {
                    GlobalInit::FloatBits(0, float_width)
                } else if is_char && !is_pointer {
                    GlobalInit::Byte(0)
                } else {
                    GlobalInit::Int(0)
                });
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
            // `char a[N] = "...";` — bytes land directly in _DATA. MSC emits the
            // full declared array: the string, an implicit NUL when it fits (or
            // always for an implicit `char a[]`), then explicit zero-fill to N.
            // Fixtures 908/2095 (implicit → strlen+1), 1386 (explicit → N).
            let bytes = match p.bump().cloned() {
                Some(Tok::StrLit(b)) => b,
                _ => unreachable!(),
            };
            let mut slots: Vec<GlobalInit> =
                bytes.iter().map(|b| GlobalInit::Byte(*b)).collect();
            if implicit_array_len || slots.len() < array_len {
                slots.push(GlobalInit::Byte(0)); // implicit NUL terminator
            }
            while slots.len() < array_len {
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
            // `&arr[K]` — address of an array element: add K*element_size.
            let elem_off = if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let k = parse_signed_int(p)?;
                p.eat(&Tok::RBrack)?;
                (k * p.globals[target_idx].element_size as i32) as u16
            } else { 0 };
            Some(vec![GlobalInit::GlobalAddr(target_idx, elem_off)])
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
        } else if is_pointer
            && matches!(p.peek(), Some(Tok::Ident(n)) if p.global_names.iter().any(|gn| gn == n))
        {
            // `int *p = data;` / `int *p = data + K;` — a global array (or var)
            // decays to its address; the optional `± K` scales by the element
            // size. Fixtures 2802, 2939, 3222.
            let target_name = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
            let target_idx = p.global_names.iter().position(|n| *n == target_name).unwrap();
            let elem = p.globals[target_idx].element_size.max(1) as i32;
            let off = match p.peek() {
                Some(Tok::Plus) => { p.bump(); (parse_signed_int(p)? * elem) as u16 }
                Some(Tok::Minus) => { p.bump(); (-parse_signed_int(p)? * elem) as u16 }
                _ => 0,
            };
            Some(vec![GlobalInit::GlobalAddr(target_idx, off)])
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
    if dims.len() > 1 {
        p.global_dims.insert(p.global_names.len(), dims.clone());
    }
    p.global_names.push(name.clone());
    // A long POINTER (`long *p`) is just a near pointer; its long-ness belongs
    // to the pointee, so it must not be flagged is_long (else `p = a` would be
    // treated as a 4-byte long store).
    p.globals.push(Global { name, init, array_len, element_size, is_pointer, struct_idx: None, is_long: is_long && !is_pointer, is_static, is_extern, is_unsigned, is_float: is_float && !is_pointer, is_const });
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
/// True when an initializer is a zero-producing algebraic identity that MSC
/// propagates as a literal-0 init (so a later `return r` is `sub ax,ax`, not a
/// reload): `e * 0` / `0 * e` and `e - e`. NOTE `e ^ e` is deliberately
/// EXCLUDED — MSC reloads it (fixture 2015 `xor-self-no-fold` vs 2016 `x - x`).
/// Only consulted in the constant-folded branch, where a call would already
/// have blocked folding, so the dropped operand is side-effect free. Fixtures
/// 2011 (`x * 0`), 2016 (`x - x`).
pub(crate) fn init_is_zero_const_identity(e: &Expr) -> bool {
    fn same_simple(l: &Expr, r: &Expr) -> bool {
        match (l, r) {
            (Expr::Local(a), Expr::Local(b)) => a == b,
            (Expr::Global(a), Expr::Global(b)) => a == b,
            (Expr::Param(a), Expr::Param(b)) => a == b,
            _ => false,
        }
    }
    match e {
        Expr::BinOp { op: BinOp::Mul, left, right } => {
            matches!(left.as_ref(), Expr::IntLit(0)) || matches!(right.as_ref(), Expr::IntLit(0))
        }
        Expr::BinOp { op: BinOp::Sub, left, right } => same_simple(left, right),
        _ => false,
    }
}
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
/// CSE normalization: rewrite an init expression into a canonical form so that
/// value-equivalent expressions compare structurally equal. MSC recognizes a
/// later init that computes the same value as an earlier scalar local's init and
/// emits `b = a` (a slot copy) instead of recomputing. We canonicalize the two
/// algebraic identities MSC folds at this level:
///   * `x * 2^n`  ≡  `x << n`
///   * `x % 2`    ≡  `x & 1`   (only for unsigned x — signed % keeps sign)
/// Fixtures 2216 (`a=x*4; b=x<<2`), 2217 (`a=x%2; b=x&1`, x unsigned).
fn cse_normalize(e: &Expr, specs: &[LocalSpec]) -> Expr {
    match e {
        Expr::BinOp { op: BinOp::Mul, left, right } => {
            if let Expr::IntLit(k) = right.as_ref() {
                if *k > 0 && (*k as u32).is_power_of_two() {
                    return Expr::BinOp {
                        op: BinOp::Shl,
                        left: Box::new(cse_normalize(left, specs)),
                        right: Box::new(Expr::IntLit((*k as u32).trailing_zeros() as i32)),
                    };
                }
            }
            Expr::BinOp {
                op: BinOp::Mul,
                left: Box::new(cse_normalize(left, specs)),
                right: Box::new(cse_normalize(right, specs)),
            }
        }
        Expr::BinOp { op: BinOp::Mod, left, right } => {
            if matches!(right.as_ref(), Expr::IntLit(2))
                && matches!(left.as_ref(), Expr::Local(l)
                    if specs.get(*l).is_some_and(|s| s.is_unsigned))
            {
                return Expr::BinOp {
                    op: BinOp::BitAnd,
                    left: Box::new(cse_normalize(left, specs)),
                    right: Box::new(Expr::IntLit(1)),
                };
            }
            Expr::BinOp {
                op: BinOp::Mod,
                left: Box::new(cse_normalize(left, specs)),
                right: Box::new(cse_normalize(right, specs)),
            }
        }
        Expr::BinOp { op, left, right } => Expr::BinOp {
            op: *op,
            left: Box::new(cse_normalize(left, specs)),
            right: Box::new(cse_normalize(right, specs)),
        },
        other => other.clone(),
    }
}
/// True when `e1` and `e2` compute the same value under CSE normalization.
fn cse_equiv(e1: &Expr, e2: &Expr, specs: &[LocalSpec]) -> bool {
    expr_struct_eq(&cse_normalize(e1, specs), &cse_normalize(e2, specs))
}
/// Structural equality over the scalar Expr subset CSE cares about.
fn expr_struct_eq(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::IntLit(x), Expr::IntLit(y)) => x == y,
        (Expr::Local(x), Expr::Local(y)) => x == y,
        (Expr::Global(x), Expr::Global(y)) => x == y,
        (Expr::Param(x), Expr::Param(y)) => x == y,
        (
            Expr::BinOp { op: o1, left: l1, right: r1 },
            Expr::BinOp { op: o2, left: l2, right: r2 },
        ) => o1 == o2 && expr_struct_eq(l1, l2) && expr_struct_eq(r1, r2),
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
    // Detect the `pascal` calling convention anywhere in the declarator prefix
    // (it may precede or follow the return type) before the modifier-skips
    // swallow it. Scan up to the parameter list's `(`.
    let is_pascal = {
        let mut j = p.pos;
        let mut found = false;
        while let Some(t) = p.toks.get(j) {
            match t {
                Tok::LParen => break,
                Tok::Kw("pascal") => { found = true; break; }
                _ => j += 1,
            }
        }
        found
    };
    // Detect the `static` storage class in the same declarator prefix. A static
    // function is TU-private: bare OMF name (no `_`), LEXTDEF/LPUBDEF records.
    let is_static_fn = {
        let mut j = p.pos;
        let mut found = false;
        while let Some(t) = p.toks.get(j) {
            match t {
                Tok::LParen => break,
                Tok::Kw("static") => { found = true; break; }
                _ => j += 1,
            }
        }
        found
    };
    // Detect the `far` calling convention in the declarator prefix (it may
    // precede or follow the return type, e.g. `int far helper(...)`). A far
    // function returns with `retf` and its params sit at [bp+6..].
    let is_far_fn = {
        let mut j = p.pos;
        let mut found = false;
        while let Some(t) = p.toks.get(j) {
            match t {
                Tok::LParen => break,
                Tok::Kw("far") => { found = true; break; }
                _ => j += 1,
            }
        }
        found
    };
    let had_mod = skip_decl_modifiers(p) > 0;
    let mut return_char = false;
    let mut return_long = false;
    let mut return_float_width = 0usize;
    let mut return_struct_bytes = 0usize;
    let mut return_struct_idx: Option<usize> = None;
    let return_int = match p.peek().cloned() {
        Some(Tok::Kw("int")) => { p.bump(); true }
        Some(Tok::Kw("char")) => { p.bump(); return_char = true; true }
        Some(Tok::Kw("long")) => {
            p.bump();
            if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
            return_long = true;
            true
        }
        // `float`/`double` returns go through the __fac floating accumulator,
        // not AX — `return_int` is false.
        Some(Tok::Kw("float")) => { p.bump(); return_float_width = 4; false }
        Some(Tok::Kw("double")) => { p.bump(); return_float_width = 8; false }
        Some(Tok::Kw("void")) => { p.bump(); false }
        // Bare `unsigned`/`signed`/`short` return (no explicit `int`) — the next
        // token is the function name. Treat as int; don't consume the name.
        Some(Tok::Ident(_)) if had_mod => true,
        // Implicit-int function definition: `name(params) { ... }` with no return
        // type at all (K&R style). The name is the current token; leave it for
        // the declarator parse below. Fixture 2163.
        Some(Tok::Ident(_)) if matches!(p.toks.get(p.pos + 1), Some(Tok::LParen)) => true,
        Some(Tok::Kw("struct")) => {
            p.bump();
            // Capture the struct's total size: a small struct returned BY VALUE
            // (<= 4 bytes, no `*`) comes back in AX / DX:AX. A struct *pointer*
            // return (`struct S *f()`) is just an int (handled by the `*` loop
            // below, which clears return_struct_bytes). Larger by-value returns
            // need the hidden-pointer ABI we don't support yet.
            if let Some(Tok::Ident(sname)) = p.peek().cloned() {
                p.bump();
                if let Some(sidx) = p.structs.iter().position(|s| s.name == sname) {
                    return_struct_bytes = p.structs[sidx].total_bytes;
                    return_struct_idx = Some(sidx);
                }
            }
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
    // AX) — sufficient for `fn()[K]` shapes (fixture 1227). Record the pointee
    // size so a caller's `fn()[K]` picks byte/word deref; a pointer return is
    // NOT a char return (clear return_char so callers don't widen the result).
    let mut return_pointee: Option<usize> = None;
    while matches!(p.peek(), Some(Tok::Star)) {
        p.bump();
        return_struct_bytes = 0; return_struct_idx = None;
        return_pointee = Some(if return_char { 1 } else if return_long { 4 } else { 2 });
        return_char = false;
    }
    // Reset the per-function `make().field` temp counter.
    p.struct_field_temp_count = 0;
    p.int_cast_ptrs.clear();
    let name = match p.bump().cloned() {
        Some(Tok::Kw("main")) => "main".to_owned(),
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected function name, got {other:?}"
            )));
        }
    };
    // Register this definition's by-value struct return so a caller's
    // `name().field` resolves the member offset (mirrors the prototype path).
    if let Some(sidx) = return_struct_idx {
        p.fn_return_struct_idx.insert(symbol_name(&name), sidx);
    }
    if let Some(pointee) = return_pointee {
        p.fn_return_pointee.insert(symbol_name(&name), pointee);
    }
    p.eat(&Tok::LParen)?;
    // Parameter list: either `void` (no params) or one or more
    // `int <name>` separated by `,`. Phase 1 only handles int
    // parameters; other types come with later fixtures.
    p.fn_ptr_params.clear();
    p.fn_ptr_locals.clear();
    p.param_dims.clear();
    p.param_dptr_elem.clear();
    p.ptr_array_stride.clear();
    p.block_scope_stack.clear();
    p.free_block_slots.clear();
    p.block_frame_max = 0;
    p.block_local_scopes.clear();
    // K&R-style definition: the parameter list is bare identifiers
    // (`add(x, y)`) whose types are declared between `)` and `{`. After
    // typedef substitution a real type would never appear as a bare
    // `Ident` immediately followed by `,`/`)`, so this is unambiguous.
    let is_knr = matches!(p.peek(), Some(Tok::Ident(_)))
        && matches!(p.toks.get(p.pos + 1), Some(Tok::Comma) | Some(Tok::RParen));
    let params = if matches!(p.peek(), Some(Tok::Kw("void"))) {
        p.bump();
        (Vec::<String>::new(), Vec::<Option<usize>>::new(), Vec::<bool>::new(), Vec::<bool>::new(), Vec::<bool>::new(), Vec::<usize>::new(), Vec::<usize>::new())
    } else if is_knr {
        // Collect the parameter names now; types default to int and are
        // patched from the declaration list below (after the `)`).
        let mut names = Vec::new();
        loop {
            match p.bump().cloned() {
                Some(Tok::Ident(s)) => names.push(s),
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected K&R parameter name, got {other:?}"
                    )));
                }
            }
            if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
            break;
        }
        let n = names.len();
        (names, vec![None; n], vec![false; n], vec![false; n], vec![false; n], vec![0; n], vec![0; n])
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
            // A bare `unsigned`/`signed`/`short` parameter with no explicit base
            // type keyword (e.g. `unsigned v`) is an `int` param.
            let bare_int_mod = (mod_start..p.pos).any(|i| matches!(p.toks.get(i),
                Some(Tok::Kw("unsigned")) | Some(Tok::Kw("signed")) | Some(Tok::Kw("short"))));
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
                // Bare `unsigned`/`signed`/`short` → implicit int; the name is
                // the next token (don't consume it as a type).
                _ if bare_int_mod => {}
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected `int`, `char`, or `struct` in parameter type, got {other:?}"
                    )));
                }
            }
            // Function-pointer parameter: `<ret> (*name)(params)` — a 2-byte
            // near pointer. Record the index so a call `name(...)` in the body
            // lowers to an indirect CallPtr (fixtures 3323, 2905, 3016, …).
            if matches!(p.peek(), Some(Tok::LParen))
                && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
                && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
                && matches!(p.toks.get(p.pos + 3), Some(Tok::RParen))
                && matches!(p.toks.get(p.pos + 4), Some(Tok::LParen))
            {
                p.bump(); p.bump(); // `(` `*`
                let fpname = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
                p.bump(); // `)`
                p.eat(&Tok::LParen)?;
                let mut depth = 1usize;
                while depth > 0 {
                    match p.bump() {
                        Some(Tok::LParen) => depth += 1,
                        Some(Tok::RParen) => depth -= 1,
                        None => return Err(EmitError::Unsupported("unterminated fnptr parameter list".to_owned())),
                        _ => {}
                    }
                }
                p.fn_ptr_params.insert(names.len());
                pointee_sizes.push(2);
                names.push(fpname);
                struct_idxs.push(None);
                is_chars.push(false);
                is_longs.push(false);
                is_unsigned_ints.push(false);
                float_widths.push(0);
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
                break;
            }
            // Pointer-to-array parameter: `<elem> (*name)[N]` — a 2-byte near
            // pointer to an array of N elements. `(*name)[K]` reads element K
            // (like `name[K]`); `name + K` strides by N*elem. Fixture 2493.
            if matches!(p.peek(), Some(Tok::LParen))
                && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
                && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
                && matches!(p.toks.get(p.pos + 3), Some(Tok::RParen))
                && matches!(p.toks.get(p.pos + 4), Some(Tok::LBrack))
            {
                p.bump(); p.bump(); // `(` `*`
                let paname = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
                p.bump(); // `)`
                p.eat(&Tok::LBrack)?;
                let dim = parse_signed_int(p).ok().filter(|&k| k > 0).map(|k| k as usize).unwrap_or(0);
                while !matches!(p.peek(), Some(Tok::RBrack)) { p.bump(); }
                p.eat(&Tok::RBrack)?;
                let elem = if is_char { 1 } else if is_long { 4 }
                    else if float_width != 0 { float_width } else { 2 };
                p.ptr_array_stride.insert(paname.clone(), dim * elem);
                pointee_sizes.push(elem); // (*p)[K] element scaling
                names.push(paname);
                struct_idxs.push(None);
                is_chars.push(false);
                is_longs.push(false);
                is_unsigned_ints.push(false);
                float_widths.push(0);
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
                break;
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
            let mut dptr_final_elem: Option<usize> = None;
            if has_ptr {
                p.bump();
                is_char = false; // pointer: always word-sized
                // Pointer-to-pointer (`int **pp`, `char **pp`): consume the
                // extra star(s). The immediate pointee is a near pointer
                // (2 bytes); the deref count is driven by `*`/`**` at the use
                // site. Fixtures 2680/2721/2906/3190/3479.
                if matches!(p.peek(), Some(Tok::Star)) {
                    // The pre-`**` `pointee_size` is the FINAL element size
                    // (char→1, int→2): record it so `argv[i][j]` picks byte/word
                    // at the inner deref. Fixture 2962.
                    dptr_final_elem = Some(pointee_size);
                    while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                    pointee_size = 2;
                }
            }
            let pname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected parameter name, got {other:?}"
                    )));
                }
            };
            if let Some(elem) = dptr_final_elem {
                p.param_dptr_elem.insert(pname.clone(), elem);
            }
            // `int a[]` / `int a[N]` decay to `int *a`; `int a[N][M]` decays to
            // `int (*)[M]`. Eat every bracket pair, capturing the dimensions so a
            // 2-D subscript `a[i][j]` can fold to a flat `ParamIndex`.
            if matches!(p.peek(), Some(Tok::LBrack)) {
                // array decays to pointer: pointee = the element type size.
                if pointee_size == 0 {
                    pointee_size = if is_char { 1 } else if is_long { 4 }
                        else if float_width != 0 { float_width } else { 2 };
                }
                is_char = false; // array decays to pointer: word-sized
                let mut dims: Vec<usize> = Vec::new();
                while matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    // `[N]` captures N; `[]` leaves a 0 placeholder (the leading
                    // dimension is unused for a fully-subscripted access).
                    let mut dim = 0usize;
                    if !matches!(p.peek(), Some(Tok::RBrack)) {
                        if let Ok(k) = parse_signed_int(p) { if k > 0 { dim = k as usize; } }
                        while !matches!(p.peek(), Some(Tok::RBrack)) { p.bump(); }
                    }
                    dims.push(dim);
                    p.eat(&Tok::RBrack)?;
                }
                if dims.len() >= 2 {
                    p.param_dims.insert(names.len(), dims);
                }
            }
            pointee_sizes.push(pointee_size);
            names.push(pname);
            struct_idxs.push(struct_idx);
            is_chars.push(is_char);
            is_longs.push(is_long && !has_ptr); // pointer-to-long is word-sized
            // `unsigned x` (not pointer) → track for /2 optimization and for
            // unsigned comparison jcc selection (incl. unsigned char).
            is_unsigned_ints.push(has_unsigned_mod && !has_ptr);
            float_widths.push(if has_ptr { 0 } else { float_width }); // pointer-to-float is word-sized
            if matches!(p.peek(), Some(Tok::Comma)) {
                p.bump();
                continue;
            }
            break;
        }
        (names, struct_idxs, is_chars, is_longs, is_unsigned_ints, float_widths, pointee_sizes)
    };
    let (mut params, param_struct_idxs, mut param_is_char, mut param_is_long, mut param_is_unsigned, mut param_float_width, mut param_pointee_size) = params;
    // Struct-by-value params: total_bytes (even-padded) when the param has a
    // struct type AND is not a pointer (pointee_size == 0). 0 otherwise.
    let mut param_struct_bytes: Vec<usize> = param_struct_idxs.iter().zip(param_pointee_size.iter())
        .map(|(si, &pointee)| match si {
            Some(sidx) if pointee == 0 => {
                let n = p.structs.get(*sidx).map(|s| s.total_bytes).unwrap_or(0);
                (n + 1) & !1
            }
            _ => 0,
        })
        .collect();
    // Struct-POINTER params: the pointee struct's even-padded byte size.
    let mut param_struct_ptr_bytes: Vec<usize> = param_struct_idxs.iter().zip(param_pointee_size.iter())
        .map(|(si, &pointee)| match si {
            Some(sidx) if pointee != 0 => {
                let n = p.structs.get(*sidx).map(|s| s.total_bytes).unwrap_or(0);
                (n + 1) & !1
            }
            _ => 0,
        })
        .collect();
    p.eat(&Tok::RParen)?;
    // K&R parameter type declarations sit between `)` and `{`:
    // `int x; int y;` (any number, multiple declarators per line). Patch
    // the default-int param vectors by name.
    if is_knr {
        while !matches!(p.peek(), Some(Tok::LBrace) | None) {
            skip_decl_modifiers(p);
            let (is_char, is_long, elem) = match p.peek() {
                Some(Tok::Kw("char")) => { p.bump(); (true, false, 1usize) }
                Some(Tok::Kw("int")) => { p.bump(); (false, false, 2) }
                Some(Tok::Kw("long")) => {
                    p.bump();
                    if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
                    (false, true, 4)
                }
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "unsupported K&R parameter type, got {other:?}"
                    )));
                }
            };
            loop {
                let has_ptr = matches!(p.peek(), Some(Tok::Star));
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                let dname = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected K&R declarator name, got {other:?}"
                        )));
                    }
                };
                let is_array = matches!(p.peek(), Some(Tok::LBrack));
                if is_array {
                    p.bump();
                    while !matches!(p.peek(), Some(Tok::RBrack) | None) { p.bump(); }
                    p.eat(&Tok::RBrack)?;
                }
                if let Some(idx) = params.iter().position(|x| *x == dname) {
                    if has_ptr || is_array {
                        // pointer / array-decayed-to-pointer parameter
                        param_pointee_size[idx] = elem;
                        param_is_char[idx] = false;
                        param_is_long[idx] = false;
                    } else {
                        param_is_char[idx] = is_char;
                        param_is_long[idx] = is_long;
                    }
                }
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
                break;
            }
            p.eat(&Tok::Semi)?;
        }
    }
    p.eat(&Tok::LBrace)?;

    // Reset per-function name lists, then populate with this
    // function's params before parsing the body.
    p.local_names.clear();
    p.local_specs.clear();
    p.local_dims.clear();
    p.param_names = params.clone();
    p.param_struct_idxs = param_struct_idxs;
    p.param_is_char = param_is_char.clone();
    p.param_is_long = param_is_long.clone();
    p.param_is_unsigned = param_is_unsigned.clone();
    p.param_pointee_sizes = param_pointee_size.clone();

    // Pascal calling convention: parameters occupy REVERSED stack slots (the
    // first-declared param sits at the highest BP offset). Reverse the param
    // metadata and name resolution so that a body reference to the first param
    // resolves to the LAST index, which the standard `param_disp` (4 + 2*idx)
    // then maps to the highest slot — no per-access reversal needed. The K&R
    // patching above has already run on declaration order, so reverse here.
    // Fixtures 1653/2062/2063/2065/2246.
    if is_pascal {
        params.reverse();
        param_is_char.reverse();
        param_is_long.reverse();
        param_is_unsigned.reverse();
        param_float_width.reverse();
        param_pointee_size.reverse();
        param_struct_bytes.reverse();
        param_struct_ptr_bytes.reverse();
        p.param_names.reverse();
        p.param_struct_idxs.reverse();
        p.param_is_char.reverse();
        p.param_is_long.reverse();
        p.param_is_unsigned.reverse();
        p.param_pointee_sizes.reverse();
    }

    // `[storage-class]+ int|char <name> [= <init>] (, <name> [= <init>])* ;`
    //
    // A non-constant init becomes a synthetic assignment statement
    // prepended to the body.
    let mut locals: Vec<LocalSpec> = Vec::new();
    let mut prelude: Vec<Stmt> = Vec::new();
    // True once a declaration's init has been emitted as a prelude (body)
    // statement — a COMPUTED init like `int *p = a`. MSC emits all declaration
    // inits in source order, so a later LITERAL init (`int sum = 0`) that would
    // normally hoist to the prologue must instead stay in the prelude (its source
    // position). Otherwise the hoisted literal would jump ahead of the computed
    // init. Fixture 1849 (`int *p = a; int sum = 0;`).
    let mut seen_computed_init = false;
    // Scalar-int local init expressions in declaration order, for CSE: a later
    // init value-equivalent to an earlier one becomes `b = a`. Fixtures 2216/2217.
    let mut scalar_inits: Vec<(usize, Expr)> = Vec::new();
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
        // A `register` storage-class on a scalar-int decl routes the local to a
        // saved register (SI/DI) rather than its stack slot. Fixtures 1550/2069.
        let has_register = (start_pos..peek_pos)
            .any(|j| matches!(p.toks.get(j), Some(Tok::Kw("register"))));
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
        if matches!(p.toks.get(peek_pos), Some(Tok::Kw("struct")) | Some(Tok::Kw("union"))) {
            skip_decl_modifiers(p);
            p.bump(); // struct / union
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
                // `struct S arr[N];` — an array of N structs is a byte array of
                // N*sizeof(S). Element `arr[i]` lives at byte i*sizeof(S).
                let mut elem_count = 1usize;
                if !is_ptr && matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let n = parse_signed_int(p)?;
                    if n <= 0 {
                        return Err(EmitError::Unsupported(format!("struct array length must be positive, got {n}")));
                    }
                    p.eat(&Tok::RBrack)?;
                    elem_count = n as usize;
                }
                let spec = if is_ptr {
                    LocalSpec { size: 2, array_len: 1, init: None, struct_idx: Some(sidx), is_long: false, init_is_literal: false, is_far_ptr: false, is_huge_ptr: false, pointee_size: stotal, pointee_unsigned: false, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None, block_offset: None, is_register: false }
                } else {
                    LocalSpec { size: 1, array_len: stotal * elem_count, init: None, struct_idx: Some(sidx), is_long: false, init_is_literal: false, is_far_ptr: false, is_huge_ptr: false, pointee_size: 0, pointee_unsigned: false, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None, block_offset: None, is_register: false }
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
            let (is_far_or_huge, is_huge) = {
                let mut i = p.pos;
                let mut found = false;
                let mut huge = false;
                while i < p.toks.len() {
                    match &p.toks[i] {
                        Tok::Kw("huge") => { found = true; huge = true; break; }
                        Tok::Kw("far") => { found = true; break; }
                        Tok::Kw("near") | Tok::Kw("unsigned") | Tok::Kw("signed")
                        | Tok::Kw("static") | Tok::Kw("extern") | Tok::Kw("register")
                        | Tok::Kw("auto") | Tok::Kw("volatile") | Tok::Kw("const")
                        | Tok::Kw("short") | Tok::Kw("cdecl") | Tok::Kw("pascal")
                        | Tok::Kw("interrupt") => { i += 1; }
                        _ => break,
                    }
                }
                (found, huge)
            };
            skip_decl_modifiers(p);
            // Function-pointer ARRAY local: `<ret> (*name[N])(params);` — an
            // N-element array of 2-byte near pointers. Elements are assigned by
            // index (`name[K] = func`) and called indirectly (`name[i](args)`).
            // Fixture 2435.
            if matches!(p.peek(), Some(Tok::LParen))
                && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
                && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
                && matches!(p.toks.get(p.pos + 3), Some(Tok::LBrack))
            {
                p.bump(); p.bump(); // `(` `*`
                let fpname = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
                p.eat(&Tok::LBrack)?;
                let n = parse_signed_int(p)?;
                if n <= 0 {
                    return Err(EmitError::Unsupported(format!("fnptr-array length must be positive, got {n}")));
                }
                p.eat(&Tok::RBrack)?;
                p.eat(&Tok::RParen)?; // close `(*name[N])`
                p.eat(&Tok::LParen)?; // parameter list
                let mut depth = 1usize;
                while depth > 0 {
                    match p.bump() {
                        Some(Tok::LParen) => depth += 1,
                        Some(Tok::RParen) => depth -= 1,
                        None => return Err(EmitError::Unsupported("unterminated fnptr-array parameter list".to_owned())),
                        _ => {}
                    }
                }
                let local_idx = locals.len();
                p.local_names.push(fpname);
                let spec = LocalSpec {
                    size: 2, array_len: n as usize, init: None, struct_idx: None, is_long: false,
                    init_is_literal: false, is_far_ptr: false, is_huge_ptr: false, pointee_size: 0, pointee_unsigned: false, is_unsigned: false,
                    init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None,
                    block_offset: None, is_register: false,
                };
                p.local_specs.push(spec.clone());
                locals.push(spec);
                break;
            }
            // Pointer-to-array local: `<elem> (*name)[N] [= expr];` — a 2-byte
            // near pointer to an array of N elements. `(*name)[K]` reads element
            // K; `name + K` strides by N*elem. Fixtures 2686, 2329.
            if matches!(p.peek(), Some(Tok::LParen))
                && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
                && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
                && matches!(p.toks.get(p.pos + 3), Some(Tok::RParen))
                && matches!(p.toks.get(p.pos + 4), Some(Tok::LBrack))
            {
                p.bump(); p.bump(); // `(` `*`
                let paname = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
                p.bump(); // `)`
                p.eat(&Tok::LBrack)?;
                let dim = parse_signed_int(p).ok().filter(|&k| k > 0).map(|k| k as usize).unwrap_or(0);
                while !matches!(p.peek(), Some(Tok::RBrack)) { p.bump(); }
                p.eat(&Tok::RBrack)?;
                let elem = size.max(1);
                p.ptr_array_stride.insert(paname.clone(), dim * elem);
                let local_idx = locals.len();
                p.local_names.push(paname);
                let spec = LocalSpec {
                    size: 2, array_len: 1, init: None, struct_idx: None, is_long: false,
                    init_is_literal: false, is_far_ptr: false, is_huge_ptr: false, pointee_size: elem, pointee_unsigned: false, is_unsigned: false,
                    init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None,
                    block_offset: None, is_register: false,
                };
                p.local_specs.push(spec.clone());
                locals.push(spec);
                if matches!(p.peek(), Some(Tok::Assign)) {
                    p.bump();
                    let value = parse_expr(p)?;
                    prelude.push(Stmt::Assign { target: AssignTarget::Local(local_idx), value });
                }
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
                break;
            }
            // Function-pointer local: `<ret> (*name)(params) [= func];` — a
            // 2-byte near pointer. The optional initializer is a function name,
            // lowered to `name = FuncAddr(func)` in the prelude. The call site
            // resolves `name(args)` to an indirect CallPtr. Fixtures 110/187/2211.
            if matches!(p.peek(), Some(Tok::LParen))
                && matches!(p.toks.get(p.pos + 1), Some(Tok::Star))
                && matches!(p.toks.get(p.pos + 2), Some(Tok::Ident(_)))
                && matches!(p.toks.get(p.pos + 3), Some(Tok::RParen))
                && matches!(p.toks.get(p.pos + 4), Some(Tok::LParen))
            {
                p.bump(); p.bump(); // `(` `*`
                let fpname = match p.bump().cloned() { Some(Tok::Ident(s)) => s, _ => unreachable!() };
                p.bump(); // `)`
                p.eat(&Tok::LParen)?;
                let mut depth = 1usize;
                while depth > 0 {
                    match p.bump() {
                        Some(Tok::LParen) => depth += 1,
                        Some(Tok::RParen) => depth -= 1,
                        None => return Err(EmitError::Unsupported("unterminated fnptr parameter list".to_owned())),
                        _ => {}
                    }
                }
                let local_idx = locals.len();
                p.local_names.push(fpname);
                p.fn_ptr_locals.insert(local_idx);
                let spec = LocalSpec {
                    size: 2, array_len: 1, init: None, struct_idx: None, is_long: false,
                    init_is_literal: false, is_far_ptr: false, is_huge_ptr: false, pointee_size: 2, pointee_unsigned: false, is_unsigned: false,
                    init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None,
                    block_offset: None, is_register: false,
                };
                p.local_specs.push(spec.clone());
                locals.push(spec);
                // Optional initializer. A bare function name → `OFFSET _func`
                // (FuncAddr); any other expression (e.g. a call `= get_op()`
                // returning a fn-ptr) is stored as its computed value. Fixture 2336.
                if matches!(p.peek(), Some(Tok::Assign)) {
                    p.bump();
                    let value = if let Some(Tok::Ident(s)) = p.peek().cloned()
                        && !matches!(p.toks.get(p.pos + 1), Some(Tok::LParen))
                    {
                        p.bump();
                        Expr::FuncAddr(s)
                    } else {
                        parse_expr(p)?
                    };
                    prelude.push(Stmt::Assign {
                        target: AssignTarget::Local(local_idx),
                        value,
                    });
                }
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
                break;
            }
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
            // Optional `[N]` (plus further dims for `int a[N][M]`) for an array.
            let mut local_dims_vec: Vec<usize> = Vec::new();
            let array_len = if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let k = parse_signed_int(p)?;
                if k <= 0 {
                    return Err(EmitError::Unsupported(format!(
                        "local array length must be positive, got {k}"
                    )));
                }
                p.eat(&Tok::RBrack)?;
                let mut total = k as usize;
                local_dims_vec.push(k as usize);
                while matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let m = parse_signed_int(p)?;
                    if m <= 0 {
                        return Err(EmitError::Unsupported(format!("local array length must be positive, got {m}")));
                    }
                    p.eat(&Tok::RBrack)?;
                    local_dims_vec.push(m as usize);
                    total *= m as usize;
                }
                total
            } else {
                1
            };
            let local_idx = locals.len();
            if local_dims_vec.len() > 1 {
                p.local_dims.insert(local_idx, local_dims_vec);
            }
            p.local_names.push(lname);
            // Long: 4-byte slot modeled as a 2-word "array". Reads via
            // `Expr::Local(idx)` pick up the low word at [bp-disp].
            let (slot_size, slot_len, is_long_slot) = if star_count > 0 {
                // A near pointer is a 2-byte slot regardless of pointee type
                // (so `char *p` / `long *p` store the address as a word, not a
                // 4-byte long). Must precede the long check — `long *p` is a
                // pointer, not a long.
                (2usize, array_len, false)
            } else if is_long_decl && array_len == 1 {
                (2usize, 2usize, true)
            } else if is_long_decl {
                // `long a[N]`: N elements of 4 bytes each. Element K lives at
                // byte offset K*4 (low word) / K*4+2 (high word). Fixtures 304/306.
                (4usize, array_len, true)
            } else {
                (size, array_len, false)
            };
            let spec = LocalSpec { size: slot_size, array_len: slot_len, init: None, struct_idx: None, is_long: is_long_slot, init_is_literal: false, is_far_ptr: is_far_ptr_decl, is_huge_ptr: is_huge && is_far_ptr_decl, pointee_size: if star_count > 0 { if is_long_decl { 4 } else { size } } else { 0 }, pointee_unsigned: has_unsigned && star_count > 0 && size == 1, is_unsigned: has_unsigned && star_count == 0 && (size == 1 || size == 2 || is_long_slot), init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None, block_offset: None, is_register: has_register && star_count == 0 && slot_len == 1 && !is_long_slot && slot_size == 2 };
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
                        // Only LITERAL-valued prior locals propagate into a later
                        // init. A COMPUTED prior local (`b = a + 1`) stores its
                        // folded value but does NOT substitute into `c = b * 2` —
                        // MSC reloads b and keeps `* 2` a runtime shl. Fixture 1811
                        // (vs the chained-literal `b = a` which propagates).
                        // A signed `char` local folds as its sign-extended byte:
                        // `char c = 200; int n = c;` makes n = -56, not 200.
                        // Fixture 2284.
                        .map(|l| if l.init_is_literal {
                            l.init.map(|v| {
                                if l.size == 1 && !l.is_unsigned && !l.is_long { (v as i8) as i32 } else { v }
                            })
                        } else {
                            None
                        })
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
                            }))
                        // Ternary CHAIN whose folding cond survives into a
                        // nested ternary arm: the fold CONSUMES the knowledge
                        // (the surviving arm re-tests at runtime), so the init
                        // must not collapse to a literal — route it through
                        // the runtime-assign prelude where const-prop applies
                        // the chain rule. Fixture 1824.
                        || matches!(&init_expr, Expr::Ternary { cond, then_arm, else_arm }
                            if matches!(cond.as_ref(), Expr::BinOp {
                                op: BinOp::Eq | BinOp::Ne | BinOp::Lt
                                    | BinOp::Le | BinOp::Gt | BinOp::Ge, ..
                            }) && cond.fold(&init_view).is_some_and(|c| matches!(
                                if c != 0 { then_arm.as_ref() } else { else_arm.as_ref() },
                                Expr::Ternary { .. })))
                        // An init containing an assignment-expression has a SIDE
                        // EFFECT (the inner store); folding it to a constant would
                        // drop that store. Keep it a runtime assign. Fixture 1217.
                        || crate::codegen::contains_assign_expr(&init_expr)
                        // A `(unsigned char)` / `(char)` cast init of a simple
                        // variable is materialized at runtime through AL (`mov al,K;
                        // sub ah,ah; mov [u],ax`), never folded to an immediate — MSC
                        // does not const-propagate through such a char cast. The
                        // shared rule (operand simple, sign/target gating) lives in
                        // codegen. Fixture 1524.
                        || (size == 2 && crate::codegen::cast_rhs_needs_al_form(&init_expr, true))
                        // An `(int)`/`(unsigned int)`/`(char)` TYPE-cast of a value
                        // that depends on a VARIABLE is not const-folded — MSC reads
                        // the operand at runtime (no propagation through the cast).
                        // `int r = (int)c` → `cbw; mov [r],ax` (reusing live AL), not
                        // `mov [r],imm`. A cast of a PURE constant expression
                        // (`(int)(5+3)`) still folds. Fixtures 2219 (var) vs 1614 (const).
                        || (init_via_type_cast && init_expr.fold(&[]).is_none());
                    // CSE: `b = E2` (a non-literal expression) value-equivalent to a
                    // prior scalar local a's init (`x*4 ≡ x<<2`, `x%2 ≡ x&1` for
                    // unsigned x) → emit `b = a`, reusing the computed value rather
                    // than re-folding. Fixtures 2216, 2217.
                    let cse_a = if matches!(init_expr, Expr::BinOp { .. })
                        && !init_via_cast && !init_via_type_cast
                    {
                        scalar_inits.iter().rev()
                            .find(|(_, e)| cse_equiv(e, &init_expr, &locals))
                            .map(|(a, _)| *a)
                    } else { None };
                    scalar_inits.push((local_idx, init_expr.clone()));
                    let fold_k = if skip_fold || cse_a.is_some() { None } else { init_expr.fold(&init_view) };
                    if let Some(a) = cse_a {
                        prelude.push(Stmt::Assign {
                            target: AssignTarget::Local(local_idx),
                            value: Expr::Local(a),
                        });
                        seen_computed_init = true;
                    } else
                    // Declaration-order preservation: if a COMPUTED init already
                    // went to the prelude, a later literal init also stays in the
                    // prelude (its source position) rather than hoisting to the
                    // prologue. Only plain (non-cast) literals — cast inits use a
                    // distinct prologue byte form. Fixture 1849.
                    if let Some(_k) = fold_k
                        && seen_computed_init
                        && !init_via_cast
                        && !init_via_type_cast
                    {
                        prelude.push(Stmt::Assign {
                            target: AssignTarget::Local(local_idx),
                            value: init_expr,
                        });
                    } else if let Some(k) = fold_k {
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
                        // `x * 0` / `x - x` fold to a constant 0 that MSC
                        // propagates like a literal init (fixtures 2011/2016).
                        let zero_identity = !init_via_type_cast && init_is_zero_const_identity(&init_expr);
                        locals[local_idx].init_is_literal = pure_literal || chained_literal || identity_literal || zero_identity;
                        locals[local_idx].init_via_cast = init_via_cast && size == 1;
                        locals[local_idx].init_via_type_cast = init_via_type_cast;
                        if let Some(spec) = p.local_specs.get_mut(local_idx) {
                            spec.init_is_literal = pure_literal || chained_literal || identity_literal || zero_identity;
                            spec.init_via_cast = init_via_cast && size == 1;
                            spec.init_via_type_cast = init_via_type_cast;
                        }
                    } else {
                        prelude.push(Stmt::Assign {
                            target: AssignTarget::Local(local_idx),
                            value: init_expr,
                        });
                        seen_computed_init = true;
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
    // `p.local_specs` is the superset of the `locals` Vec above: it carries
    // the same function-level locals (kept in sync via the `local_specs`
    // mirror writes during init folding) plus any block-level locals appended
    // while the body was parsed. For functions without nested-block
    // declarations the two are identical.
    let locals = p.local_specs.clone();
    Ok(Function { name, return_int, return_long, return_char, return_float_width, return_struct_bytes, params, param_struct_bytes, param_is_char, param_is_long, param_is_unsigned, param_float_width, param_pointee_size, param_struct_ptr_bytes, locals, local_names, int_cast_ptrs: std::mem::take(&mut p.int_cast_ptrs), body, struct_field_temp_count: p.struct_field_temp_count, is_pascal, is_static: is_static_fn, is_far: is_far_fn })
}
/// Scan a prototype's parameter list (the tokens between `(` at `lparen_idx`
/// and `)` at `close_idx`) and return each param's `is_long` flag. A param is
/// long iff it carries the `long` keyword and is NOT a pointer (`long *` is a
/// 2-byte near pointer, not a 4-byte long arg). `(void)` / `()` → empty.
fn proto_param_longs(toks: &[Tok], lparen_idx: usize, close_idx: usize) -> Vec<bool> {
    let mut longs = Vec::new();
    let mut depth = 0i32;
    let (mut has_long, mut has_star, mut saw_any) = (false, false, false);
    for j in (lparen_idx + 1)..close_idx {
        match toks.get(j) {
            Some(Tok::LParen) => { depth += 1; saw_any = true; }
            Some(Tok::RParen) => depth -= 1,
            Some(Tok::Comma) if depth == 0 => {
                longs.push(has_long && !has_star);
                has_long = false; has_star = false; saw_any = false;
            }
            Some(Tok::Kw("long")) if depth == 0 => { has_long = true; saw_any = true; }
            Some(Tok::Star) if depth == 0 => { has_star = true; saw_any = true; }
            // A lone `void` parameter list means no params.
            Some(Tok::Kw("void")) if depth == 0 && !saw_any => {}
            Some(_) => saw_any = true,
            None => {}
        }
    }
    if saw_any {
        longs.push(has_long && !has_star);
    }
    longs
}
/// Scan a prototype's parameter list and return each param's struct-by-value
/// byte size (even-padded total_bytes when it is `struct NAME` with no `*`, 0
/// otherwise). Used so a call to a prototyped function pushes a struct arg as
/// its words. `(void)` / `()` → empty.
fn proto_param_struct_bytes(toks: &[Tok], lparen_idx: usize, close_idx: usize, structs: &[StructDef]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let (mut sname, mut has_star, mut saw_any): (Option<String>, bool, bool) = (None, false, false);
    let mut flush = |sname: &Option<String>, has_star: bool| -> usize {
        match sname {
            Some(n) if !has_star => {
                structs.iter().find(|s| s.name == *n).map(|s| (s.total_bytes + 1) & !1).unwrap_or(0)
            }
            _ => 0,
        }
    };
    for j in (lparen_idx + 1)..close_idx {
        match toks.get(j) {
            Some(Tok::LParen) => { depth += 1; saw_any = true; }
            Some(Tok::RParen) => depth -= 1,
            Some(Tok::Comma) if depth == 0 => {
                out.push(flush(&sname, has_star));
                sname = None; has_star = false; saw_any = false;
            }
            Some(Tok::Kw("struct")) if depth == 0 => { saw_any = true; }
            Some(Tok::Star) if depth == 0 => { has_star = true; saw_any = true; }
            Some(Tok::Ident(n)) if depth == 0 && sname.is_none() && !has_star => {
                // The first identifier after `struct` is the tag name.
                sname = Some(n.clone()); saw_any = true;
            }
            Some(_) => saw_any = true,
            None => {}
        }
    }
    if saw_any {
        out.push(flush(&sname, has_star));
    }
    out
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
/// Storage-class / sign keywords that may lead a declaration.
fn is_decl_modifier_kw(k: &str) -> bool {
    matches!(k, "unsigned" | "signed" | "static" | "extern" | "register"
        | "auto" | "volatile" | "const" | "short")
}

/// True when the upcoming tokens begin a local variable declaration —
/// used to detect block-level `{ int a; ... }` declarations. Mirrors the
/// function-top decl detection: an optional run of storage/sign modifiers
/// followed by a primitive type keyword, or `≥1` modifier then an
/// identifier (`unsigned x;` → implicit int). Statements never start with
/// a type keyword, so this never misfires on a real statement.
pub(crate) fn looks_like_local_decl(p: &Parser<'_>) -> bool {
    let mut i = p.pos;
    while matches!(p.toks.get(i), Some(Tok::Kw(k)) if is_decl_modifier_kw(k)) {
        i += 1;
    }
    match p.toks.get(i) {
        Some(Tok::Kw("int" | "char" | "long" | "float" | "double"
            | "struct" | "union" | "enum")) => true,
        // `unsigned x;` / `register r;` — a bare modifier run then a name.
        Some(Tok::Ident(_)) => i > p.pos,
        _ => false,
    }
}

impl Parser<'_> {
    fn enter_block_scope(&mut self) {
        self.block_scope_stack.push(Vec::new());
        self.block_local_scopes.push(Vec::new());
    }
    fn exit_block_scope(&mut self) {
        if let Some(frame) = self.block_scope_stack.pop() {
            for slot in frame {
                self.free_block_slots.push(slot);
            }
        }
        self.block_local_scopes.pop();
    }
    /// Resolve a name to a local index with innermost-wins scoping: a block
    /// local declared in a currently-open block shadows an outer local of the
    /// same name. Searches the open block scopes from innermost to outermost,
    /// then falls back to the flat function-level position (the first — i.e.
    /// outermost — match). With no open block scopes this is exactly the old
    /// `local_names.iter().position(|n| n == name)`.
    fn resolve_local(&self, name: &str) -> Option<usize> {
        for frame in self.block_local_scopes.iter().rev() {
            for &idx in frame.iter().rev() {
                if self.local_names[idx] == name {
                    return Some(idx);
                }
            }
        }
        self.local_names.iter().position(|n| n == name)
    }
    /// Allocate a frame slot of `size` bytes (rounded up to even) for a
    /// block-level local. Reuses the deepest freed slot of the exact size
    /// when one is available, else extends `block_frame_max` (the high-water
    /// depth). Returns the `block_offset` — the cumulative depth below the
    /// function frame `F`, so the local's displacement is `-(F + offset)`.
    fn alloc_block_slot(&mut self, size: u16) -> u16 {
        let even = (size + 1) & !1;
        // Prefer the deepest free slot of the EXACT size; if none, reuse the
        // deepest LARGER free slot (e.g. a 2-byte `int b` reusing a freed 4-byte
        // `long a` region) and return the shallower remainder to the free list
        // (`b` takes the deepest 2 bytes, frame doesn't grow). Fixture 1969.
        let exact = self.free_block_slots.iter().enumerate()
            .filter(|(_, (_, sz))| *sz == even)
            .max_by_key(|(_, (off, _))| *off)
            .map(|(i, _)| i);
        let pick = exact.or_else(|| self.free_block_slots.iter().enumerate()
            .filter(|(_, (_, sz))| *sz > even)
            .max_by_key(|(_, (off, _))| *off)
            .map(|(i, _)| i));
        let offset = if let Some(i) = pick {
            let (off, sz) = self.free_block_slots.remove(i);
            if sz > even {
                self.free_block_slots.push((off - even, sz - even));
            }
            off
        } else {
            self.block_frame_max += even;
            self.block_frame_max
        };
        if let Some(frame) = self.block_scope_stack.last_mut() {
            frame.push((offset, even));
        }
        offset
    }
}

/// Parse one block-level declaration statement (`int a = 1, b;`),
/// allocating a reusable frame slot for each declarator and returning the
/// synthesized init-assign statements. Block-local inits emit INLINE at the
/// block's position (unlike function-top literal inits, which store in the
/// prologue): the value flows to later reads through const-prop's `l_known`
/// tracking, not `spec.init`, so `spec.init` is left `None`. Only scalar
/// `int` / `char` locals are supported; richer block-local types fall
/// through as `Unsupported` until a fixture needs them.
fn parse_block_local_decl(p: &mut Parser<'_>) -> Result<Vec<Stmt>, EmitError> {
    let mod_start = p.pos;
    skip_decl_modifiers(p);
    let has_unsigned = (mod_start..p.pos)
        .any(|i| matches!(p.toks.get(i), Some(Tok::Kw("unsigned"))));
    let enum_consumed = matches!(p.peek(), Some(Tok::Kw("enum")));
    if enum_consumed {
        p.bump();
        if matches!(p.peek(), Some(Tok::Ident(_))) { p.bump(); }
    }
    let mut is_long = false;
    let size = if enum_consumed {
        2usize
    } else {
        match p.peek() {
            Some(Tok::Kw("int")) => { p.bump(); 2 }
            Some(Tok::Kw("char")) => { p.bump(); 1 }
            // `long [int] x;` — a 4-byte slot (array_len=2, is_long). Fixture 1969.
            Some(Tok::Kw("long")) => {
                p.bump();
                if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
                is_long = true;
                2
            }
            // `unsigned x;` — a modifier run with no explicit type kw.
            Some(Tok::Ident(_)) if p.pos > mod_start => 2,
            other => return Err(EmitError::Unsupported(format!(
                "unsupported block-local declaration starting with {other:?}"))),
        }
    };
    let mut stmts = Vec::new();
    loop {
        if matches!(p.peek(), Some(Tok::Star) | Some(Tok::LParen)) {
            return Err(EmitError::Unsupported(
                "pointer block-local not yet supported".to_owned()));
        }
        let lname = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => return Err(EmitError::Unsupported(format!(
                "expected identifier in block declaration, got {other:?}"))),
        };
        if matches!(p.peek(), Some(Tok::LBrack)) {
            return Err(EmitError::Unsupported(
                "array block-local not yet supported".to_owned()));
        }
        let storage = if is_long { 4 } else { ((size + 1) & !1) as u16 };
        let off = p.alloc_block_slot(storage);
        let mut spec = if is_long {
            LocalSpec::long_(None)
        } else if size == 1 {
            LocalSpec::char_(None)
        } else {
            LocalSpec::int(None)
        };
        spec.is_unsigned = has_unsigned && size == 1;
        spec.block_offset = Some(off);
        let local_idx = p.local_specs.len();
        p.local_names.push(lname);
        p.local_specs.push(spec);
        // Register in the current block scope so a later reference resolves
        // to this (innermost) declaration even if an outer local shares its name.
        if let Some(frame) = p.block_local_scopes.last_mut() {
            frame.push(local_idx);
        }
        if matches!(p.peek(), Some(Tok::Assign)) {
            p.bump();
            let value = parse_expr(p)?;
            stmts.push(Stmt::Assign {
                target: AssignTarget::Local(local_idx),
                value,
            });
        }
        if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); continue; }
        break;
    }
    p.eat(&Tok::Semi)?;
    Ok(stmts)
}

pub(crate) fn parse_stmt(p: &mut Parser<'_>) -> Result<Stmt, EmitError> {
    // `goto <label>;`
    if matches!(p.peek(), Some(Tok::Ident(n)) if n == "goto") {
        p.bump();
        let name = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => return Err(EmitError::Unsupported(format!("expected label after goto, got {other:?}"))),
        };
        p.eat(&Tok::Semi)?;
        return Ok(Stmt::Goto(name));
    }
    // `<label>:` — an identifier immediately followed by a colon.
    if let Some(Tok::Ident(n)) = p.peek().cloned()
        && matches!(p.toks.get(p.pos + 1), Some(Tok::Colon))
    {
        p.bump(); // ident
        p.bump(); // colon
        return Ok(Stmt::Label(n));
    }
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
            // statement. A nested block opens a fresh local scope:
            // leading `int a; ...` declarations allocate reusable frame
            // slots (freed on `}`) and their inits emit inline.
            p.bump();
            p.enter_block_scope();
            let mut stmts = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                if looks_like_local_decl(p) {
                    stmts.extend(parse_block_local_decl(p)?);
                } else {
                    stmts.push(parse_stmt(p)?);
                }
            }
            p.eat(&Tok::RBrace)?;
            p.exit_block_scope();
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
                } else if let Some(idx) = p.resolve_local(&name) {
                    AssignTarget::DoubleDerefLocal(idx)
                } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                    AssignTarget::DoubleDerefParam(idx)
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
            // `*(<ptr> +/- <idx>) = <value>;` — store through pointer arithmetic
            // (`*(p + 1) = v` ≡ `p[1] = v`). The index scales by the pointee size
            // in codegen, so it's passed unscaled. Fixture 3591.
            if matches!(p.peek(), Some(Tok::LParen)) {
                p.bump(); // (
                let inner = parse_expr(p)?;
                p.eat(&Tok::RParen)?;
                let (op, base, idx) = match inner {
                    Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } => (op, *left, *right),
                    other => return Err(EmitError::Unsupported(format!(
                        "deref-store through `*({other:?})` not yet supported"))),
                };
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let index = match (&op, idx.fold(&init_view)) {
                    (BinOp::Sub, Some(k)) => Expr::IntLit(-k),
                    (BinOp::Add, _) => idx,
                    (BinOp::Sub, None) => return Err(EmitError::Unsupported(
                        "runtime `*(p - i)` deref-store not yet supported".to_owned())),
                    _ => unreachable!(),
                };
                let target = if let Expr::Param(pi) = base {
                    let elem = p.param_pointee_sizes.get(pi).copied().filter(|&s| s > 0).unwrap_or(2);
                    AssignTarget::ParamIndexStore { param: pi, index: Box::new(index), elem }
                } else if let Expr::Local(li) = base {
                    let ptsz = p.local_specs[li].pointee_size.max(1);
                    let k = index.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
                        "runtime `*(local + i)` deref-store not yet supported".to_owned()))?;
                    let byte_off = (k * ptsz as i32) as u16;
                    AssignTarget::DerefLocalOffset { local: li, byte_off, is_byte: ptsz == 1 }
                } else {
                    return Err(EmitError::Unsupported(format!(
                        "deref-store through `*({base:?} + idx)` not yet supported")));
                };
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            // `*<call>(...) = <value>;` — store through a call's pointer result.
            // The pointer expr is evaluated then stored via a generic DerefExpr
            // target (`call f; mov bx,ax; mov word [bx],v`). Fixture 1322.
            if matches!(p.peek(), Some(Tok::Ident(_)))
                && matches!(p.toks.get(p.pos + 1), Some(Tok::LParen))
            {
                let ptr_expr = parse_atom(p)?;
                if matches!(ptr_expr, Expr::Call { .. } | Expr::CallPtr { .. }) {
                    let target = AssignTarget::DerefExpr { ptr: Box::new(ptr_expr), is_byte: false };
                    p.eat(&Tok::Assign)?;
                    let value = parse_expr(p)?;
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                return Err(EmitError::Unsupported(
                    "deref-store through non-call `*<ident>(...)` not yet supported".to_owned()));
            }
            // `*arr[K] = <value>;` — store through a local pointer-array element.
            // Lowered to a generic DerefExpr{LocalIndex}; const-prop folds an
            // `arr[K]=&x` element alias into a direct store to x. Fixture 1565.
            if let Some(Tok::Ident(nm)) = p.peek().cloned()
                && matches!(p.toks.get(p.pos + 1), Some(Tok::LBrack))
                && let Some(li) = p.resolve_local(&nm)
                && p.local_specs[li].array_len > 1
            {
                p.bump(); // ident
                p.bump(); // [
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let ptr = Expr::LocalIndex { local: li, index: Box::new(index) };
                let target = AssignTarget::DerefExpr { ptr: Box::new(ptr), is_byte: false };
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
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
                let step_sign = if matches!(p.peek(), Some(Tok::PlusPlus)) { 1i32 } else { -1i32 };
                if let Some(local_idx) = p.resolve_local(&target_name) {
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
                } else if let Some(param_idx) = p.param_names.iter().position(|n| *n == target_name) {
                    p.bump();
                    let ptsz = p.param_pointee_sizes.get(param_idx).copied().unwrap_or(0);
                    let step = step_sign * if ptsz > 0 { ptsz as i32 } else { 1 };
                    p.eat(&Tok::Assign)?;
                    let value = parse_expr(p)?;
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign {
                        target: AssignTarget::DerefPostMutateParam { param_idx, step },
                        value,
                    });
                }
            }
            let target = if let Some(idx) = p.resolve_local(&target_name) {
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
            // A plain `*p = v` through a `char *` local is a byte store (fixture
            // 1299). Compound `*p op= K` keeps DerefLocal so the in-place
            // self-assign peephole still fires.
            let target = match target {
                AssignTarget::DerefLocal(idx) if p.local_specs[idx].pointee_size == 1 =>
                    AssignTarget::DerefLocalByte(idx),
                other => other,
            };
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
            // `<struct-array-global>[K].<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(global_idx) = p.global_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.globals[global_idx].struct_idx
                && !p.globals[global_idx].is_pointer
            {
                p.bump();
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let stotal = p.structs[sidx].total_bytes;
                p.eat(&Tok::Dot)?;
                let (field_off, size) = parse_field_lookup(p, sidx)?;
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let target = if let Some(k) = index.fold(&init_view) {
                    let byte_off = u16::try_from(k as i64 * stotal as i64 + field_off as i64)
                        .expect("struct-array field offset fits");
                    AssignTarget::GlobalField { global: global_idx, byte_off, size }
                } else {
                    AssignTarget::StructArrayField { array: global_idx, index: Box::new(index), stride: stotal as u16, field_off, size }
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
            // `<struct-global>.<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Dot))
                && let Some(global_idx) = p.global_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.globals[global_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                let target = if let Some((bit_off, bit_width)) = p.last_field_bits {
                    AssignTarget::BitField { base: BitBase::Global(global_idx), byte_off, bit_off, bit_width }
                } else {
                    AssignTarget::GlobalField { global: global_idx, byte_off, size }
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
            // `<ptr-param>[idx] = <expr>;` — store through a pointer parameter.
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(param_idx) = p.param_names.iter().position(|n| *n == name)
                && p.param_pointee_sizes.get(param_idx).copied().unwrap_or(0) > 0
            {
                p.bump();
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let elem = p.param_pointee_sizes[param_idx];
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::ParamIndexStore { param: param_idx, index: Box::new(index), elem },
                    value,
                });
            }
            // `<struct-array-local>[K].<field> = <expr>;` — store into an element
            // of an array of structs (byte_off = K*sizeof(S) + field_off).
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(local_idx) = p.resolve_local(&name)
                && let Some(sidx) = p.local_specs[local_idx].struct_idx
                && p.local_specs[local_idx].pointee_size == 0
            {
                p.bump();
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let stotal = p.structs[sidx].total_bytes;
                p.eat(&Tok::Dot)?;
                let (field_off, size) = parse_field_lookup(p, sidx)?;
                // Constant index → plain LocalField store. Non-constant index
                // defers to LocalStructArrayField (const-prop may fold it once
                // the index value is known; otherwise runtime si-scaling codegen).
                // Fixtures 1821/1914 (runtime), 2438 (`i=2` folded by const-prop).
                let target = if let Some(k) = index.fold(&init_view) {
                    let byte_off = u16::try_from(k as i64 * stotal as i64 + field_off as i64)
                        .expect("struct-array field offset fits");
                    AssignTarget::LocalField { local: local_idx, byte_off, size }
                } else {
                    AssignTarget::LocalStructArrayField {
                        local: local_idx, index: Box::new(index),
                        stride: stotal as u16, field_off, size,
                    }
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
            // `<struct-local>.<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Dot))
                && let Some(local_idx) = p.resolve_local(&name)
                && let Some(sidx) = p.local_specs[local_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                let target = if let Some((bit_off, bit_width)) = p.last_field_bits {
                    AssignTarget::BitField { base: BitBase::Local(local_idx), byte_off, bit_off, bit_width }
                } else {
                    AssignTarget::LocalField { local: local_idx, byte_off, size }
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
            // `<struct-value-param>.<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Dot))
                && let Some(param_idx) = p.param_names.iter().position(|n| *n == name)
                && let Some(Some(sidx)) = p.param_struct_idxs.get(param_idx).cloned()
                && p.param_pointee_sizes.get(param_idx).copied().unwrap_or(0) == 0
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                let target = AssignTarget::ParamField { param: param_idx, byte_off, size };
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
                && let Some(local_idx) = p.resolve_local(&name)
                && let Some(sidx) = p.local_specs[local_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                let target = AssignTarget::DerefLocalField { ptr_local: local_idx, byte_off, size };
                let value = if let Some(v) = parse_compound_rhs(p, &target)? {
                    v
                } else {
                    p.eat(&Tok::Assign)?;
                    parse_expr(p)?
                };
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            // `<local-array>[K] = <expr>;` (and compound shapes:
            // `+=`, `-=`, `*=`, `++`, `--`, etc.) — indexed local
            // array store.
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(local_idx) = p.resolve_local(&name)
            {
                p.bump(); // [
                let mut index_expr = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                // Multidimensional local `a[i][j] = ...`: constant folds to a flat
                // element index (flows through the 1-D path); runtime 2-D → Index2D.
                if let Some(dims) = p.local_dims.get(&local_idx).cloned()
                    && let Some(ms) = parse_multidim_sub(p, &index_expr, &dims)?
                {
                    match ms {
                        MultiSub::Flat(flat) => { index_expr = Expr::IntLit(flat); }
                        MultiSub::Runtime(mut ix) if ix.len() == 2 => {
                            let elem = p.local_specs[local_idx].size;
                            let col = Box::new(ix.pop().unwrap());
                            let row = Box::new(ix.pop().unwrap());
                            let target = AssignTarget::Index2D { is_global: false, base: local_idx, row, col, cols: dims[1], elem };
                            p.eat(&Tok::Assign)?;
                            let value = parse_expr(p)?;
                            p.eat(&Tok::Semi)?;
                            return Ok(Stmt::Assign { target, value });
                        }
                        MultiSub::Runtime(_) => return Err(EmitError::Unsupported(
                            "runtime index on a >2-D local array store not yet supported".to_owned())),
                    }
                }
                // Try folding against the local-init view so simple
                // `a[i] = ...` with `i = K` known at decl folds.
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let elem_bytes = p.local_specs[local_idx].size;
                // A POINTER local: `p[0] = v` is `*p = v` (deref store), so the
                // alias pass can redirect it to the pointee. (Non-zero indices
                // through a pointer local are deferred.)
                // Gate the deref-store forms to SCALAR pointers (array_len==1);
                // an `int *p[N]` array stores into its K-th element (IndexedLocal
                // via the fall-through below), not through a deref. Fixture 1565.
                let ptsz = if p.local_specs[local_idx].array_len == 1 {
                    p.local_specs[local_idx].pointee_size
                } else { 0 };
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
                    let value = if let Some(v) = parse_compound_rhs(p, &target)? {
                        v
                    } else {
                        p.eat(&Tok::Assign)?;
                        parse_expr(p)?
                    };
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                if let Some(k) = index_expr.fold(&init_view) {
                    // Constant index — use existing byte-offset forms.
                    let byte_off = u16::try_from((k as i64) * (elem_bytes as i64))
                        .expect("indexed-store byte offset fits");
                    let folded = if elem_bytes == 1 {
                        AssignTarget::IndexedLocalByte { local: local_idx, byte_off }
                    } else {
                        AssignTarget::IndexedLocal { local: local_idx, byte_off }
                    };
                    if let Some(v) = parse_compound_rhs_for_indexed(
                        p, local_idx, byte_off, elem_bytes == 1, false,
                    )? {
                        p.eat(&Tok::Semi)?;
                        return Ok(Stmt::Assign { target: folded, value: v });
                    }
                    p.eat(&Tok::Assign)?;
                    let value = parse_expr(p)?;
                    p.eat(&Tok::Semi)?;
                    // A plain `=` through a VARIABLE index keeps the var form so
                    // const-prop can track variable-write forwarding — a later
                    // `a[i]` read folds only from a variable-indexed write, not a
                    // literal one (fixtures 144/1620 vs 1090/1428). const-prop
                    // converts it back to the direct store. A SOURCE-LITERAL index
                    // stays a direct store.
                    let target = if matches!(index_expr, Expr::IntLit(_)) {
                        folded
                    } else if elem_bytes == 1 {
                        AssignTarget::IndexedLocalByteVar { local: local_idx, index: Box::new(index_expr) }
                    } else {
                        AssignTarget::IndexedLocalVar { local: local_idx, index: Box::new(index_expr) }
                    };
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
                // Multidimensional `a[i][j] = ...`: constant → flat element index;
                // runtime 2-D → AssignTarget::Index2D (plain store).
                let multidim = if let Some(dims) = p.global_dims.get(&array_idx).cloned() {
                    parse_multidim_sub(p, &index_expr, &dims)?.map(|ms| (ms, dims))
                } else { None };
                if let Some((MultiSub::Runtime(mut ix), dims)) = multidim {
                    if ix.len() != 2 {
                        return Err(EmitError::Unsupported("runtime index on a >2-D array store not yet supported".to_owned()));
                    }
                    let elem = p.globals[array_idx].element_size;
                    let col = Box::new(ix.pop().unwrap());
                    let row = Box::new(ix.pop().unwrap());
                    let target = AssignTarget::Index2D { is_global: true, base: array_idx, row, col, cols: dims[1], elem };
                    p.eat(&Tok::Assign)?;
                    let value = parse_expr(p)?;
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign { target, value });
                }
                let k = match multidim {
                    Some((MultiSub::Flat(flat), _)) => flat,
                    None if index_expr.fold(&[]).is_none() => {
                        // Runtime (non-constant) index into a global array.
                        let g = &p.globals[array_idx];
                        if g.is_pointer {
                            return Err(EmitError::Unsupported(
                                "runtime index on a global pointer store not yet supported".to_owned(),
                            ));
                        }
                        let byte_elem = g.element_size == 1;
                        // The element self-read (for compound `arr[i] op= rhs`).
                        let read = if byte_elem {
                            Expr::IndexByte { array: array_idx, index: Box::new(index_expr.clone()) }
                        } else {
                            Expr::Index { array: array_idx, index: Box::new(index_expr.clone()) }
                        };
                        let value = match p.peek() {
                            Some(Tok::Assign) => { p.bump(); parse_expr(p)? }
                            Some(Tok::PlusPlus) => { p.bump(); Expr::BinOp { op: BinOp::Add, left: Box::new(read), right: Box::new(Expr::IntLit(1)) } }
                            Some(Tok::MinusMinus) => { p.bump(); Expr::BinOp { op: BinOp::Sub, left: Box::new(read), right: Box::new(Expr::IntLit(1)) } }
                            Some(tok) if compound_assign_op(tok).is_some() => {
                                let op = compound_assign_op(tok).unwrap();
                                p.bump();
                                let rhs = parse_expr(p)?;
                                Expr::BinOp { op, left: Box::new(read), right: Box::new(rhs) }
                            }
                            other => return Err(EmitError::Unsupported(
                                format!("expected assignment to runtime array element, got {other:?}"))),
                        };
                        p.eat(&Tok::Semi)?;
                        let target = if byte_elem {
                            AssignTarget::IndexedGlobalByteVar { array: array_idx, index: Box::new(index_expr) }
                        } else {
                            AssignTarget::IndexedGlobalVar { array: array_idx, index: Box::new(index_expr) }
                        };
                        return Ok(Stmt::Assign { target, value });
                    }
                    _ => index_expr.fold(&[]).ok_or_else(|| EmitError::Unsupported(
                        "non-constant array index in store not yet supported".to_owned(),
                    ))?,
                };
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
            let target = if let Some(idx) = p.resolve_local(&name) {
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
            let value = parse_assign_rhs(p)?;
            // Whole-struct copy `g1 = g2;` (both struct globals).
            if let AssignTarget::Global(dst) = target
                && let Expr::Global(src) = value
                && let Some(sidx) = p.globals[dst].struct_idx
                && p.globals[src].struct_idx == Some(sidx)
                // ≤2 words → AX/DX direct; >4 even bytes → rep movsw via a di/si
                // frame (fixture 3612). Odd-byte structs aren't handled yet.
                && p.structs[sidx].total_bytes % 2 == 0
            {
                let bytes = u16::try_from(p.structs[sidx].total_bytes).expect("struct size fits");
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::StructGlobalCopy { dst, src, bytes },
                    value: Expr::IntLit(0),
                });
            }
            // Whole-struct copy `s2 = s1;` (both struct locals). ≤4 bytes
            // copies via AX/DX; larger structs movsw their even-padded
            // storage (a 5-byte struct copies 3 words — fixture 2747).
            if let AssignTarget::Local(dst) = target
                && let Expr::Local(src) = value
                && let Some(sidx) = p.local_specs[dst].struct_idx
                && p.local_specs[src].struct_idx == Some(sidx)
            {
                let padded = p.structs[sidx].total_bytes.div_ceil(2) * 2;
                let bytes = u16::try_from(padded).expect("struct size fits");
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::StructLocalCopy { dst, src, bytes },
                    value: Expr::IntLit(0),
                });
            }
            // Whole-struct copy `a = g;` (struct local from a struct global).
            if let AssignTarget::Local(dst) = target
                && let Expr::Global(src) = value
                && let Some(sidx) = p.local_specs[dst].struct_idx
                && p.globals[src].struct_idx == Some(sidx)
            {
                let padded = p.structs[sidx].total_bytes.div_ceil(2) * 2;
                let bytes = u16::try_from(padded).expect("struct size fits");
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::StructLocalFromGlobalCopy { dst, src, bytes },
                    value: Expr::IntLit(0),
                });
            }
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
            // `++(*p);` / `--(*p);` — pre-inc/dec the POINTEE. Statement value is
            // unused, so it lowers to the deref store `*p = *p ± 1` (the in-place
            // mem-op peephole turns it into `mov bx,[p]; inc/dec word [bx]`).
            // Fixture 1302.
            if matches!(p.peek(), Some(Tok::LParen)) {
                p.bump(); // (
                let inner = parse_expr(p)?;
                p.eat(&Tok::RParen)?;
                p.eat(&Tok::Semi)?;
                if let Some(target) = expr_to_deref_target(&inner) {
                    return Ok(Stmt::Assign {
                        target,
                        value: Expr::BinOp {
                            op: if inc { BinOp::Add } else { BinOp::Sub },
                            left: Box::new(inner), right: Box::new(Expr::IntLit(1)),
                        },
                    });
                }
                return Err(EmitError::Unsupported(format!(
                    "prefix `++/--` on parenthesized non-deref: {inner:?}")));
            }
            // `++*p;` / `--*p;` — pre-inc/dec the pointee through a BARE deref
            // (no parens). Same lowering as the parenthesized `++(*p)` form:
            // `*p = *p ± 1`. Fixture 2331.
            if matches!(p.peek(), Some(Tok::Star)) {
                let inner = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                if let Some(target) = expr_to_deref_target(&inner) {
                    return Ok(Stmt::Assign {
                        target,
                        value: Expr::BinOp {
                            op: if inc { BinOp::Add } else { BinOp::Sub },
                            left: Box::new(inner), right: Box::new(Expr::IntLit(1)),
                        },
                    });
                }
                return Err(EmitError::Unsupported(format!(
                    "prefix `++/--` on non-deref star expr: {inner:?}")));
            }
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after prefix `++/--`, got {other:?}"
                    )));
                }
            };
            // `++a[K];` — pre-inc/dec of an array element. Lowers to the indexed
            // self-assign `a[K] = a[K] ± 1`, which the in-place mem-op peephole
            // turns into `inc/dec [..]`. Constant index. Fixtures 547, 718.
            if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let k = index.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
                    "non-constant array-element pre-inc not yet supported".to_owned()))?;
                let (target, read) = if let Some(li) = p.resolve_local(&name) {
                    let esz = p.local_specs[li].size as u16;
                    let bo = k as u16 * esz;
                    if esz == 1 { (AssignTarget::IndexedLocalByte { local: li, byte_off: bo }, Expr::LocalIndexByte { local: li, index: Box::new(Expr::IntLit(k)) }) }
                    else { (AssignTarget::IndexedLocal { local: li, byte_off: bo }, Expr::LocalIndex { local: li, index: Box::new(Expr::IntLit(k)) }) }
                } else if let Some(gi) = p.global_names.iter().position(|n| *n == name) {
                    let esz = p.globals[gi].element_size as u16;
                    let bo = k as u16 * esz;
                    if esz == 1 { (AssignTarget::IndexedGlobalByte { array: gi, byte_off: bo }, Expr::IndexByte { array: gi, index: Box::new(Expr::IntLit(k)) }) }
                    else { (AssignTarget::IndexedGlobal { array: gi, byte_off: bo }, Expr::Index { array: gi, index: Box::new(Expr::IntLit(k)) }) }
                } else {
                    return Err(EmitError::Unsupported(format!("pre-inc of unknown array `{name}`")));
                };
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target,
                    value: Expr::BinOp {
                        op: if inc { BinOp::Add } else { BinOp::Sub },
                        left: Box::new(read), right: Box::new(Expr::IntLit(1)),
                    },
                });
            }
            // `++<ident>.field;` / `++<ident>->field;` — pre-inc/dec of a struct
            // field. Desugar to `lv = lv ± 1` with the field target + its read
            // expr. Fixtures 404, 405, 709.
            if matches!(p.peek(), Some(Tok::Dot) | Some(Tok::Arrow)) {
                let is_arrow = matches!(p.peek(), Some(Tok::Arrow));
                p.bump();
                let (target, read) = struct_field_lvalue(p, &name, is_arrow)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target,
                    value: Expr::BinOp {
                        op: if inc { BinOp::Add } else { BinOp::Sub },
                        left: Box::new(read), right: Box::new(Expr::IntLit(1)),
                    },
                });
            }
            let target = if let Some(idx) = p.resolve_local(&name) {
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
            // A pointer steps by its pointee size, not 1 (`++p` on `int *p` adds
            // 2). Fixture 561.
            let (lvalue, stride) = match target {
                AssignTarget::Local(i) => {
                    let s = p.local_specs[i].pointee_size;
                    (Expr::Local(i), if s > 0 { s as i32 } else { 1 })
                }
                AssignTarget::Param(i) => {
                    let s = p.param_pointee_sizes.get(i).copied().unwrap_or(0);
                    (Expr::Param(i), if s > 0 { s as i32 } else { 1 })
                }
                AssignTarget::Global(g) => {
                    let s = if p.globals[g].is_pointer { p.globals[g].element_size as i32 } else { 1 };
                    (Expr::Global(g), s)
                }
                _ => unreachable!(),
            };
            Ok(Stmt::Assign {
                target,
                value: Expr::BinOp {
                    op: if inc { BinOp::Add } else { BinOp::Sub },
                    left: Box::new(lvalue),
                    right: Box::new(Expr::IntLit(stride)),
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
/// Multidimensional subscript folding. The caller has parsed `name[idx0]` and
/// passes `idx0` plus the array's recorded `dims`. If there are further `[idxN]`
/// subscripts, parse them and fold the whole list to a flat row-major element
/// index (constant indices only — a runtime multidim index is left unsupported
/// for now). Returns `Ok(Some(flat))` when this is a multidim access, `Ok(None)`
/// when it is an ordinary 1-D subscript (no extra `[`).
/// Parse a (possibly nested) braced initializer for a multidimensional array,
/// flattening it row-major and zero-padding each level to its declared size.
/// `dims` is the remaining dimension slice (e.g. `[3,2]` → `[2]` on recursion).
/// `is_char` selects Byte vs Int leaves. Handles partial inner inits (`{3}` →
/// `[3,0]`) and char string rows (`"AB"` in a char[N] row → bytes then 0-pad).
pub(crate) fn parse_multidim_init(
    p: &mut Parser<'_>,
    dims: &[usize],
    is_char: bool,
) -> Result<Vec<GlobalInit>, EmitError> {
    let total: usize = dims.iter().product();
    let mut out: Vec<GlobalInit> = Vec::new();
    p.eat(&Tok::LBrace)?;
    let inner_stride: usize = dims.get(1..).map(|r| r.iter().product()).unwrap_or(1);
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        if dims.len() > 1 && matches!(p.peek(), Some(Tok::LBrace)) {
            out.extend(parse_multidim_init(p, &dims[1..], is_char)?);
        } else if dims.len() > 1 && is_char && matches!(p.peek(), Some(Tok::StrLit(_))) {
            // A string literal fills one row of a char array (NUL + pad as 0).
            let bytes = match p.bump().cloned() { Some(Tok::StrLit(b)) => b, _ => unreachable!() };
            let start = out.len();
            for b in bytes.iter().take(inner_stride) { out.push(GlobalInit::Byte(*b)); }
            while out.len() - start < inner_stride { out.push(GlobalInit::Byte(0)); }
        } else {
            // Scalar leaf at the innermost level.
            let v = parse_signed_int(p)?;
            out.push(if is_char { GlobalInit::Byte((v as u32 & 0xFF) as u8) } else { GlobalInit::Int(v) });
        }
        match p.peek() {
            Some(Tok::Comma) => { p.bump(); }
            Some(Tok::RBrace) => break,
            other => return Err(EmitError::Unsupported(format!(
                "expected `,` or `}}` in array initializer, got {other:?}"))),
        }
    }
    p.eat(&Tok::RBrace)?;
    // Zero-pad this level to its full size.
    while out.len() < total {
        out.push(if is_char { GlobalInit::Byte(0) } else { GlobalInit::Int(0) });
    }
    Ok(out)
}
pub(crate) enum MultiSub {
    /// All indices compile-time constant → flat row-major element index.
    Flat(i32),
    /// At least one runtime index → the parsed index expressions, in order.
    Runtime(Vec<Expr>),
}
pub(crate) fn parse_multidim_sub(
    p: &mut Parser<'_>,
    idx0: &Expr,
    dims: &[usize],
) -> Result<Option<MultiSub>, EmitError> {
    if !matches!(p.peek(), Some(Tok::LBrack)) {
        return Ok(None);
    }
    let mut idxs: Vec<Expr> = vec![idx0.clone()];
    while matches!(p.peek(), Some(Tok::LBrack)) {
        p.bump();
        let e = parse_expr(p)?;
        p.eat(&Tok::RBrack)?;
        idxs.push(e);
    }
    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
    let consts: Option<Vec<i32>> = idxs.iter().map(|e| e.fold(&init_view)).collect();
    if let Some(cs) = consts {
        let mut flat = 0i32;
        for (d, &ix) in cs.iter().enumerate() {
            let stride: usize = dims.get(d + 1..).map(|rest| rest.iter().product()).unwrap_or(1);
            flat += ix * stride as i32;
        }
        Ok(Some(MultiSub::Flat(flat)))
    } else {
        Ok(Some(MultiSub::Runtime(idxs)))
    }
}
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
/// Map a compound-assignment token (`+=`, `*=`, …) to its `BinOp`. Returns None
/// for non-compound tokens.
pub(crate) fn compound_assign_op(tok: &Tok) -> Option<BinOp> {
    Some(match tok {
        Tok::PlusEq => BinOp::Add,
        Tok::MinusEq => BinOp::Sub,
        Tok::StarEq => BinOp::Mul,
        Tok::SlashEq => BinOp::Div,
        Tok::PercentEq => BinOp::Mod,
        Tok::AndEq => BinOp::BitAnd,
        Tok::PipeEq => BinOp::BitOr,
        Tok::CaretEq => BinOp::BitXor,
        Tok::ShlEq => BinOp::Shl,
        Tok::ShrEq => BinOp::Shr,
        _ => return None,
    })
}
/// Map a deref lvalue expression (`*p`) to the AssignTarget that stores
/// to it, for assignment-as-expression `(*p = v)`. Only the scalar
/// pointer-deref forms are handled. Fixture 3333.
pub(crate) fn expr_to_deref_target(e: &Expr) -> Option<AssignTarget> {
    match e {
        Expr::DerefWord { ptr } => match ptr.as_ref() {
            Expr::Local(i) => Some(AssignTarget::DerefLocal(*i)),
            Expr::Param(i) => Some(AssignTarget::DerefParam(*i)),
            Expr::Global(g) => Some(AssignTarget::DerefGlobal(*g)),
            _ => None,
        },
        Expr::DerefByte { ptr } => match ptr.as_ref() {
            Expr::Local(i) => Some(AssignTarget::DerefLocalByte(*i)),
            _ => None,
        },
        // `*p++ = v` as an expression/condition (the strcpy idiom).
        Expr::PostIncDeref { ptr, step, .. } => match ptr.as_ref() {
            Expr::Param(i) => Some(AssignTarget::DerefPostMutateParam { param_idx: *i, step: *step }),
            Expr::Local(i) => Some(AssignTarget::DerefPostMutateLocal { local_idx: *i, step: *step }),
            _ => None,
        },
        _ => None,
    }
}
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
        AssignTarget::DoubleDerefParam(i) => {
            Expr::DerefWord { ptr: Box::new(Expr::DerefWord { ptr: Box::new(Expr::Param(*i)) }) }
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
        // `p[K] op= v` on a pointer local: the self-read is `*(p + byte_off)`,
        // matching the read-side parse so the alias pass folds it. Fixture 863.
        AssignTarget::DerefLocalOffset { local, byte_off, is_byte } => {
            let inner = Expr::BinOp {
                op: BinOp::Add,
                left: Box::new(Expr::Local(*local)),
                right: Box::new(Expr::IntLit(*byte_off as i32)),
            };
            if *is_byte { Expr::DerefByte { ptr: Box::new(inner) } }
            else { Expr::DerefWord { ptr: Box::new(inner) } }
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
    // Pointer arithmetic through a double-pointer deref: `*pp += K` where pp is
    // `T **` advances the pointee pointer by K * sizeof(T). Scale the step by the
    // recorded final element size. Fixture 3647 (`struct Pt **pp; *pp += 1` → +=4).
    let rhs = if matches!(op, BinOp::Add | BinOp::Sub)
        && let AssignTarget::DerefParam(pp) = target
        && let Some(&elem) = p.param_dptr_elem.get(&p.param_names[*pp])
        && elem > 1
    {
        match rhs {
            Expr::IntLit(k) => Expr::IntLit(k * elem as i32),
            other => Expr::BinOp {
                op: BinOp::Mul,
                left: Box::new(other),
                right: Box::new(Expr::IntLit(elem as i32)),
            },
        }
    } else {
        rhs
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
        let (target, lvalue) = if let Some(idx) = p.resolve_local(&name) {
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
    let target = if let Some(idx) = p.resolve_local(&name) {
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
    // Assignment as a condition: `while (*d++ = *s++)` / `if ((*p = v))`. The
    // store runs and its value is tested for truthiness. Fixture 1808.
    if matches!(p.peek(), Some(Tok::Assign))
        && let Some(target) = expr_to_deref_target(&expr)
    {
        p.bump();
        let value = parse_assign_rhs(p)?;
        return Ok(Cond::Truthy(Expr::AssignExpr { target, value: Box::new(value) }));
    }
    Ok(cond_from_expr(expr))
}
/// When `base` (a struct pointer to `sidx`) is followed by `-><ptr-field>` and
/// then more member access (`->`/`.`/`[`), build a `PtrChainField` that walks
/// the pointer chain. The `->`/`.` is NOT yet consumed. Returns `None` (consuming
/// nothing) when it's a plain single-field access, so the caller keeps its
/// existing per-base codegen. Fixtures 2816, 2703.
fn try_build_chain(p: &mut Parser<'_>, base: Expr, sidx: usize) -> Result<Option<Expr>, EmitError> {
    // p.pos points at `->`/`.`; the field name is one past it.
    let fname = match p.toks.get(p.pos + 1) {
        Some(Tok::Ident(s)) => s.clone(),
        _ => return Ok(None),
    };
    let next2 = p.toks.get(p.pos + 2).cloned();
    // `o->m.<...>` where the first field `m` is an embedded VALUE struct (not a
    // pointer): no initial deref hop — enter the chain at the param's target
    // struct (`sidx`) with `acc` accumulating m's offset. Fixture 3448.
    if let Some(f) = p.structs[sidx].fields.iter().find(|f| f.name == fname).cloned()
        && !f.is_pointer && f.struct_idx.is_some()
        && matches!(next2, Some(Tok::Dot))
    {
        return Ok(Some(continue_chain(p, base, vec![], sidx)?));
    }
    let f = match p.structs[sidx].fields.iter().find(|f| f.name == fname) {
        Some(f) if f.is_pointer => f.clone(),
        _ => return Ok(None),
    };
    // `o->ptr[K]` — index the pointer field directly (one hop + element read).
    if matches!(next2, Some(Tok::LBrack)) {
        p.bump(); p.bump(); p.bump(); // `->`, field, `[`
        let index = parse_expr(p)?;
        p.eat(&Tok::RBrack)?;
        let k = chain_const_index(p, &index)?;
        let elem = f.pointee_size.max(1);
        return Ok(Some(Expr::PtrChainField {
            base: Box::new(base), hops: vec![f.byte_off],
            final_off: (k as i64 * elem as i64) as u16, final_size: elem,
        }));
    }
    // `o->ptr->...` / `o->ptr.field` — hop into the struct the pointer targets.
    if f.struct_idx.is_some() && matches!(next2, Some(Tok::Arrow) | Some(Tok::Dot)) {
        p.bump(); p.bump(); // `->`, field
        return Ok(Some(continue_chain(p, base, vec![f.byte_off], f.struct_idx.unwrap())?));
    }
    Ok(None)
}
/// Fold a chain subscript index to a constant.
fn chain_const_index(p: &Parser<'_>, index: &Expr) -> Result<i32, EmitError> {
    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
    index.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
        "non-constant pointer-chain index not yet supported".to_owned()))
}
/// Continue a pointer member-chain after the first hop. `cur` is the struct that
/// the BX pointer currently points to. Consumes `->`/`.`/`[` until the leaf.
fn continue_chain(p: &mut Parser<'_>, base: Expr, mut hops: Vec<u16>, mut cur: usize) -> Result<Expr, EmitError> {
    // `acc` accumulates the byte offset of value-struct sub-fields traversed by
    // `.` since the last pointer deref (a value struct is embedded, so `.field`
    // just adds its offset without an extra `mov bx,[bx+off]`). A `->` deref of a
    // pointer field flushes `acc` into a new hop. Fixtures 3448 (`o->m.p->c`).
    let mut acc: u16 = 0;
    loop {
        let dot = matches!(p.peek(), Some(Tok::Dot));
        let arrow = matches!(p.peek(), Some(Tok::Arrow));
        if !dot && !arrow {
            return Err(EmitError::Unsupported("malformed pointer member-chain".to_owned()));
        }
        p.bump();
        let fname = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => return Err(EmitError::Unsupported(format!("expected field in chain, got {other:?}"))),
        };
        let f = p.structs[cur].fields.iter().find(|f| f.name == fname)
            .ok_or_else(|| EmitError::Unsupported(format!("field `{fname}` not in chain struct")))?
            .clone();
        let field_loc = acc + f.byte_off;
        // Continuation? `->` derefs a pointer field (new hop); `.` descends into
        // an embedded value struct (accumulate offset, no hop); `[K]` indexes a
        // pointer field; else f is the leaf.
        if matches!(p.peek(), Some(Tok::Arrow)) {
            if !f.is_pointer || f.struct_idx.is_none() {
                return Err(EmitError::Unsupported("`->` on a non-pointer field mid-chain".to_owned()));
            }
            hops.push(field_loc);
            cur = f.struct_idx.unwrap();
            acc = 0;
            continue;
        }
        if matches!(p.peek(), Some(Tok::Dot)) {
            if f.is_pointer || f.struct_idx.is_none() {
                return Err(EmitError::Unsupported("`.` on a non-value-struct field mid-chain".to_owned()));
            }
            acc = field_loc;
            cur = f.struct_idx.unwrap();
            continue;
        }
        if matches!(p.peek(), Some(Tok::LBrack)) {
            p.bump();
            let index = parse_expr(p)?;
            p.eat(&Tok::RBrack)?;
            let k = chain_const_index(p, &index)?;
            hops.push(field_loc);
            let elem = f.pointee_size.max(1);
            return Ok(Expr::PtrChainField { base: Box::new(base), hops, final_off: (k as i64 * elem as i64) as u16, final_size: elem });
        }
        return Ok(Expr::PtrChainField { base: Box::new(base), hops, final_off: field_loc, final_size: f.size });
    }
}
/// Resolve `<expr>.<field>` or `<expr>-><field>` to its byte offset
/// and field size by looking up `field` in the struct definition at
/// `sidx`. Caller has already consumed `.` or `->`.
/// Resolve a struct-member lvalue `<name>.field` / `<name>->field` (the `.`/`->`
/// already consumed) to its (AssignTarget, read-Expr) pair, for desugaring
/// `++<member>` into `member = member ± 1`. Bit-fields are rejected (they need
/// a dedicated BitField target). Fixtures 404, 405, 709.
fn struct_field_lvalue(p: &mut Parser<'_>, name: &str, is_arrow: bool) -> Result<(AssignTarget, Expr), EmitError> {
    let unsupported = || EmitError::Unsupported(format!(
        "prefix `++/--` of unsupported member `{name}`"));
    if !is_arrow {
        if let Some(gi) = p.global_names.iter().position(|n| n == name)
            && let Some(sidx) = p.globals[gi].struct_idx
            && !p.globals[gi].is_pointer
        {
            let (byte_off, size) = parse_field_lookup(p, sidx)?;
            if p.last_field_bits.is_some() { return Err(unsupported()); }
            return Ok((AssignTarget::GlobalField { global: gi, byte_off, size },
                       Expr::GlobalField { global: gi, byte_off, size }));
        }
        if let Some(li) = p.resolve_local(name)
            && let Some(sidx) = p.local_specs[li].struct_idx
            && p.local_specs[li].pointee_size == 0
        {
            let (byte_off, size) = parse_field_lookup(p, sidx)?;
            if p.last_field_bits.is_some() { return Err(unsupported()); }
            return Ok((AssignTarget::LocalField { local: li, byte_off, size },
                       Expr::LocalField { local: li, byte_off, size }));
        }
    } else {
        if let Some(gi) = p.global_names.iter().position(|n| n == name)
            && let Some(sidx) = p.globals[gi].struct_idx
            && p.globals[gi].is_pointer
        {
            let (byte_off, size) = parse_field_lookup(p, sidx)?;
            if p.last_field_bits.is_some() { return Err(unsupported()); }
            return Ok((AssignTarget::DerefGlobalField { ptr_global: gi, byte_off, size },
                       Expr::DerefGlobalField { ptr_global: gi, byte_off, size }));
        }
        if let Some(pi) = p.param_names.iter().position(|n| n == name)
            && let Some(Some(sidx)) = p.param_struct_idxs.get(pi).cloned()
        {
            let (byte_off, size) = parse_field_lookup(p, sidx)?;
            if p.last_field_bits.is_some() { return Err(unsupported()); }
            return Ok((AssignTarget::DerefParamField { ptr_param: pi, byte_off, size },
                       Expr::DerefParamField { ptr_param: pi, byte_off, size }));
        }
        if let Some(li) = p.resolve_local(name)
            && let Some(sidx) = p.local_specs[li].struct_idx
        {
            let (byte_off, size) = parse_field_lookup(p, sidx)?;
            if p.last_field_bits.is_some() { return Err(unsupported()); }
            return Ok((AssignTarget::DerefLocalField { ptr_local: li, byte_off, size },
                       Expr::DerefLocalField { ptr_local: li, byte_off, size }));
        }
    }
    Err(unsupported())
}
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
    // Inline struct VALUE field carries struct_idx for `o.inner.a` recursion; a
    // struct POINTER field also carries struct_idx but must NOT recurse inline.
    let inner = if field.is_pointer { None } else { field.struct_idx };
    let (byte_off, size) = (field.byte_off, field.size);
    let leaf_bits = (field.bit_off, field.bit_width);
    p.last_field_bits = None;
    // Multi-level access `o.inner.a`: recurse into the nested struct, summing
    // byte offsets; the returned size is the leaf field's.
    if let Some(inner_sidx) = inner
        && matches!(p.peek(), Some(Tok::Dot))
    {
        p.bump(); // .
        let (inner_off, inner_size) = parse_field_lookup(p, inner_sidx)?;
        return Ok((byte_off + inner_off, inner_size));
    }
    // A leaf bit-field: expose its (bit_off, bit_width) so the caller emits a
    // BitField access. Ordinary fields leave `last_field_bits` cleared.
    if leaf_bits.1 > 0 {
        p.last_field_bits = Some(leaf_bits);
    }
    // Array field element `s.v[K]` (constant K) → byte_off + K*element_size.
    if matches!(p.peek(), Some(Tok::LBrack)) {
        p.bump();
        let idx = parse_expr(p)?;
        p.eat(&Tok::RBrack)?;
        let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
        let k = idx.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
            "non-constant struct array-field index not yet supported".to_owned()))?;
        let off = u16::try_from(byte_off as i64 + k as i64 * size as i64)
            .expect("struct array-field offset fits");
        return Ok((off, size));
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
            // Normalize a constant LEFT operand to the right (`3 > i` → `i < 3`):
            // the emit arms compare `cmp [var], k` (= var - k), so a const-on-left
            // ordering flips the subtraction sign and the jcc would be wrong unless
            // the relational op is mirrored. Fixture 1531 (`for (i=0; 3 > i; i++)`).
            if matches!(left.as_ref(), Expr::IntLit(_)) && !matches!(right.as_ref(), Expr::IntLit(_)) {
                return Cond::Cmp {
                    op: mirror_relop(op),
                    left: right.as_ref().clone(),
                    right: left.as_ref().clone(),
                };
            }
            return Cond::Cmp {
                op,
                left: left.as_ref().clone(),
                right: right.as_ref().clone(),
            };
        }
    }
    Cond::Truthy(expr)
}
/// Mirror a relational operator when its operands are swapped: `a < b` ⟺ `b > a`.
/// Eq/Ne are symmetric (unchanged).
pub(crate) fn mirror_relop(op: RelOp) -> RelOp {
    match op {
        RelOp::Eq => RelOp::Eq,
        RelOp::Ne => RelOp::Ne,
        RelOp::Lt => RelOp::Gt,
        RelOp::Gt => RelOp::Lt,
        RelOp::Le => RelOp::Ge,
        RelOp::Ge => RelOp::Le,
    }
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
    // Postfix `++`/`--` in expression position (`i++ < n`, `(a++, ...)`). Yields
    // the OLD value then mutates. Pointers step by their element size.
    while matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
        let step = if matches!(p.peek(), Some(Tok::PlusPlus)) { 1 } else { -1 };
        p.bump();
        left = match left {
            Expr::Local(i) => {
                let st = if p.local_specs[i].pointee_size > 0 { step * p.local_specs[i].pointee_size as i32 } else { step };
                Expr::PostMutateLocal { local_idx: i, step: st }
            }
            Expr::Param(i) => Expr::PostMutateParam { param_idx: i, step },
            Expr::Global(i) => {
                let g = &p.globals[i];
                let st = if g.is_pointer { step * g.element_size as i32 } else { step };
                Expr::PostMutateGlobal { global_idx: i, step: st }
            }
            // `*p++` parsed as `(*p)` then `++` — reparent to `*(p++)`: the deref
            // reads the OLD pointee while the pointer advances by its stride.
            Expr::DerefByte { ptr }
                if matches!(ptr.as_ref(), Expr::Param(_) | Expr::Local(_) | Expr::Global(_)) =>
                Expr::PostIncDeref { ptr, step, is_byte: true },
            Expr::DerefWord { ptr }
                if matches!(ptr.as_ref(), Expr::Param(_) | Expr::Local(_) | Expr::Global(_)) =>
                Expr::PostIncDeref { ptr, step: step * 2, is_byte: false },
            // `arr[i]++` / `a[K]++` on a global array — post-mutate the element.
            Expr::Index { array, index } =>
                Expr::PostMutateIndexedGlobal { array, index, step, is_byte: false },
            Expr::IndexByte { array, index } =>
                Expr::PostMutateIndexedGlobal { array, index, step, is_byte: true },
            // `a[K]++` on a LOCAL array — post-mutate the element (const index).
            Expr::LocalIndex { local, index } =>
                Expr::PostMutateLocalIndex { local, index, step, is_byte: false },
            Expr::LocalIndexByte { local, index } =>
                Expr::PostMutateLocalIndex { local, index, step, is_byte: true },
            other => return Err(EmitError::Unsupported(format!(
                "postfix ++/-- on unsupported operand: {other:?}"))),
        };
    }
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
        // `e & 0xFF` is the zero-extended low byte — MSC lowers it exactly like
        // `(unsigned char)e` (`mov al,[e]; sub ah,ah`). Fixtures 2935 / 2539.
        if matches!(op, BinOp::BitAnd) && matches!(right, Expr::IntLit(255)) {
            // `&0xff` masks keep folding to an immediate (from_var: false) — the
            // AL-form rule is reserved for explicit `(char)`/`(unsigned char)` casts.
            left = Expr::CastChar { value: Box::new(left), unsigned: true, from_var: false };
            continue;
        }
        // Pointer arithmetic on a DECAYED ARRAY (`a + K` / `a - K`): the array
        // name decayed to `AddrOf{Local,Global}(a)` and `K` counts ELEMENTS, so
        // scale it to bytes — matching `&a[K]`, which is already byte-scaled at
        // parse. (Pointer param/local arithmetic is scaled later in emit_binop,
        // so only the address forms are scaled here.) Fixtures 1047, 1052, 1814.
        if matches!(op, BinOp::Add | BinOp::Sub)
            && let Expr::IntLit(k) = right
        {
            let elem = match &left {
                Expr::AddrOfLocal(i)
                    if p.local_specs[*i].array_len > 1 && p.local_specs[*i].pointee_size == 0 =>
                    Some(p.local_specs[*i].size as i32),
                Expr::AddrOfGlobal(i)
                    if p.globals[*i].array_len > 1 && !p.globals[*i].is_pointer =>
                    Some(p.globals[*i].element_size as i32),
                _ => None,
            };
            if let Some(elem) = elem {
                let off = if matches!(op, BinOp::Sub) { -(k * elem) } else { k * elem };
                left = Expr::BinOp { op: BinOp::Add, left: Box::new(left), right: Box::new(Expr::IntLit(off)) };
                continue;
            }
        }
        left = Expr::BinOp { op, left: Box::new(left), right: Box::new(right) };
    }
    Ok(left)
}
/// Best-effort pointee-size inference for `*<expr>` lowering.
/// Returns the byte width of `*expr`. `char *` resolves to 1; `int *`
/// (and unrecognized shapes) to 2. Used by parse_atom to pick between
/// `DerefByte` and `DerefWord` variants. Parameters carry no type
/// info in Phase 1 so they default to int-pointer (word).
pub(crate) fn pointee_size_of(e: &Expr, globals: &[Global], locals: &[LocalSpec], params: &[usize]) -> usize {
    match e {
        Expr::Global(idx) => globals[*idx].element_size,
        // A pointer local carries its pointee size (1 for `char *`, 2 for `int *`).
        Expr::Local(idx) => locals.get(*idx).map(|s| s.pointee_size).unwrap_or(0),
        Expr::Param(idx) => params.get(*idx).copied().unwrap_or(0),
        // Decayed arrays: element size of the pointed-to array.
        Expr::AddrOfGlobal(idx) => globals.get(*idx).map(|g| g.element_size).unwrap_or(2),
        Expr::AddrOfLocal(idx) => locals.get(*idx).map(|s| s.size).unwrap_or(2),
        // A plain integer is not a pointer (so the other operand of `K + ptr`
        // wins the pointee-size vote).
        Expr::IntLit(_) => 0,
        // Pre/postfix on a pointer: step magnitude = pointee element size.
        // step=±1 → char*, step=±2 → int*. Covers `*++p` / `*p++`.
        Expr::PostMutateLocal { step, .. } | Expr::PostMutateGlobal { step, .. }
        | Expr::PreMutateLocal { step, .. } | Expr::PreMutateGlobal { step, .. }
        | Expr::PreMutateParam { step, .. } | Expr::PostMutateParam { step, .. } => {
            step.unsigned_abs() as usize
        }
        Expr::BinOp { op: BinOp::Add, left, right } => {
            // `<ptr> + K` / `K + <ptr>` inherits the pointer operand's pointee
            // size, so `*(char_ptr + i)` is a byte deref. Whichever side is a
            // pointer (nonzero pointee) wins; default to int.
            let ls = pointee_size_of(left, globals, locals, params);
            if ls != 0 { return ls; }
            let rs = pointee_size_of(right, globals, locals, params);
            if rs != 0 { return rs; }
            2
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
/// Parse the RHS of a statement assignment, threading chained assignment
/// `a = b = c = V` into right-associative nested `AssignExpr` (each stores the
/// same AX value). Only fires for simple scalar lvalue links; used solely at the
/// statement site so comma/ternary contexts (which call `parse_expr`) are
/// unaffected. Fixtures 500, 2951, 3334.
pub(crate) fn parse_assign_rhs(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let e = parse_expr(p)?;
    if matches!(p.peek(), Some(Tok::Assign))
        && matches!(e, Expr::Local(_) | Expr::Param(_) | Expr::Global(_))
    {
        p.bump();
        let rhs = parse_assign_rhs(p)?;
        let target = match e {
            Expr::Local(i) => AssignTarget::Local(i),
            Expr::Param(i) => AssignTarget::Param(i),
            Expr::Global(g) => AssignTarget::Global(g),
            _ => unreachable!(),
        };
        return Ok(Expr::AssignExpr { target, value: Box::new(rhs) });
    }
    Ok(e)
}
/// Identity for the comma-operator value path; future widening for
/// implicit type promotions can hook here.
pub(crate) fn expr_from_stmt_value(e: Expr) -> Expr { e }
pub(crate) fn parse_atom(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let tok = p.bump().cloned();
    match tok {
        Some(Tok::LParen) => {
            // `(type) <expr>` cast. A `(char)` / `(unsigned char)` VALUE cast
            // (no `*`) truncates to a byte (handled below); other scalar/pointer
            // casts are identity (Phase 1 doesn't model widening semantics).
            let after_lparen = p.pos;
            let cast_unsigned = matches!(p.peek(), Some(Tok::Kw("unsigned")));
            skip_decl_modifiers(p);
            // `(struct Name *) <expr>` / `(union Name *) <expr>` — pointer cast,
            // treated as identity. Fixture 1702 (`(struct Node *)0`).
            if matches!(p.peek(), Some(Tok::Kw("struct")) | Some(Tok::Kw("union"))) {
                p.bump();
                if matches!(p.peek(), Some(Tok::Ident(_))) { p.bump(); } // tag name
                skip_decl_modifiers(p);
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                p.eat(&Tok::RParen)?;
                return parse_atom(p);
            }
            // Modifier-only cast: `(unsigned)`, `(signed)`, `(short)` — an
            // int-width identity cast (the keyword was a modifier, with the
            // base `int` implied). Fixtures 2751, 3678.
            if p.pos > after_lparen && matches!(p.peek(), Some(Tok::RParen)) {
                p.bump();
                return parse_atom(p);
            }
            if matches!(p.peek(), Some(Tok::Kw("int")) | Some(Tok::Kw("char")) | Some(Tok::Kw("long"))
                | Some(Tok::Kw("float")) | Some(Tok::Kw("double"))) {
                let cast_char = matches!(p.peek(), Some(Tok::Kw("char")));
                let cast_int = matches!(p.peek(), Some(Tok::Kw("int")));
                let cast_long = matches!(p.peek(), Some(Tok::Kw("long")));
                p.bump();
                // Accept `long int`, then skip any pointer-distance
                // qualifiers (`far`/`near`/`huge`) that may appear between
                // the type and `*` (e.g. `(int far *)`), then skip `*`s.
                while matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
                skip_decl_modifiers(p);
                let mut had_star = false;
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); had_star = true; }
                p.eat(&Tok::RParen)?;
                let inner = parse_atom(p)?;
                if cast_char && !had_star {
                    // A literal operand in source (`(char)200`) folds completely
                    // to the truncated constant — MSC materializes it as a word
                    // `mov ax,K` (fixture 171), unlike `(char)<var>` which keeps
                    // the byte form even when the var is const-propagated.
                    if let Expr::IntLit(k) = inner {
                        let t = if cast_unsigned { (k as u8) as i32 } else { (k as i8) as i32 };
                        return Ok(Expr::IntLit(t));
                    }
                    // Collapse a nested byte cast: `(unsigned char)(x & 0xFF)`
                    // (the mask already lowered to CastChar) is one truncation.
                    let value = match inner {
                        Expr::CastChar { value, .. } => value,
                        other => Box::new(other),
                    };
                    // An explicit cast of a bare scalar variable selects the AL
                    // materialize form for assign/init RHS (see cast_rhs_needs_al_form).
                    let from_var = matches!(value.as_ref(),
                        Expr::Local(_) | Expr::Param(_) | Expr::Global(_));
                    return Ok(Expr::CastChar { value, unsigned: cast_unsigned, from_var });
                }
                // `(int)<ptr>` strips pointer-ness for binop operand-order
                // decisions: record the cast pointer so `(int)p + n` keeps
                // the plain int left-first order (3429) while an uncast
                // `p + n` swaps to int-side-first (2711).
                if cast_int && !had_star {
                    match &inner {
                        Expr::Param(i)
                            if p.param_pointee_sizes.get(*i).copied().unwrap_or(0) > 0 =>
                        {
                            p.int_cast_ptrs.insert((true, *i));
                        }
                        Expr::Local(i)
                            if p.local_specs.get(*i).map(|s| s.pointee_size).unwrap_or(0) > 0 =>
                        {
                            p.int_cast_ptrs.insert((false, *i));
                        }
                        _ => {}
                    }
                }
                // A pointer cast `(char *)`/`(int *)`/`(long *)` records its
                // pointee size so a directly-following unary `*` reads the right
                // width (`*(char *)p` → byte). Set AFTER the inner atom so the
                // OUTERMOST cast wins for nested casts; a following deref consumes
                // (and clears) it. Fixtures 3163/3278/2430.
                if had_star {
                    p.cast_ptr_pointee = if cast_char { Some(1) } else if cast_long { Some(4) } else { Some(2) };
                }
                // `(long)<scalar int>` — preserve long-ness that the bare AST drops
                // (a long cast is otherwise identity), so a later long multiply /
                // arithmetic recognizes it. fold() delegates to the inner, so const
                // contexts (a long-local init) are unaffected. Fixture 1683.
                if cast_long && !had_star {
                    let int_scalar_unsigned: Option<bool> = match &inner {
                        Expr::Param(i)
                            if !p.param_is_long.get(*i).copied().unwrap_or(false)
                                && p.param_pointee_sizes.get(*i).copied().unwrap_or(0) == 0
                                && p.param_struct_idxs.get(*i).map(|s| s.is_none()).unwrap_or(true)
                                && !p.param_is_char.get(*i).copied().unwrap_or(false) =>
                            Some(cast_unsigned || p.param_is_unsigned.get(*i).copied().unwrap_or(false)),
                        _ => None,
                    };
                    if let Some(unsigned) = int_scalar_unsigned {
                        return Ok(Expr::CastLong { value: Box::new(inner), unsigned });
                    }
                }
                return Ok(inner);
            }
            let inner = parse_expr(p)?;
            // `(*p = v)` — assignment-as-expression through a pointer deref.
            // The value is produced in AX, stored, and left in AX for the
            // enclosing expression. Fixture 3333.
            if matches!(p.peek(), Some(Tok::Assign))
                && let Some(target) = expr_to_deref_target(&inner)
            {
                p.bump();
                let value = parse_assign_rhs(p)?;
                p.eat(&Tok::RParen)?;
                return Ok(Expr::AssignExpr { target, value: Box::new(value) });
            }
            // `(a[i] = e)` — assignment-as-expression to an array element. Build
            // the runtime-index store target; const-prop folds a known index to
            // the direct form. Fixture 1986.
            if matches!(p.peek(), Some(Tok::Assign)) {
                let idx_target = match &inner {
                    Expr::LocalIndex { local, index } =>
                        Some(AssignTarget::IndexedLocalVar { local: *local, index: index.clone() }),
                    Expr::LocalIndexByte { local, index } =>
                        Some(AssignTarget::IndexedLocalByteVar { local: *local, index: index.clone() }),
                    Expr::Index { array, index } =>
                        Some(AssignTarget::IndexedGlobalVar { array: *array, index: index.clone() }),
                    Expr::IndexByte { array, index } =>
                        Some(AssignTarget::IndexedGlobalByteVar { array: *array, index: index.clone() }),
                    _ => None,
                };
                if let Some(target) = idx_target {
                    p.bump();
                    let value = parse_assign_rhs(p)?;
                    p.eat(&Tok::RParen)?;
                    return Ok(Expr::AssignExpr { target, value: Box::new(value) });
                }
            }
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
                if let Stmt::Assign { target, value } = last {
                    // Pure `(x = v)` (no comma) with a SIMPLE RHS: an assignment
                    // EXPRESSION whose value is the RHS, kept in AX for the
                    // surrounding cond/use (fixtures 513/1434/2996). A compound
                    // RHS like `(a = a+1)` stays the Seq form so MSC's in-place
                    // peephole + reload fires (fixture 2992). With prior comma
                    // sides, also fall back to the Seq form.
                    if sides.is_empty()
                        && matches!(value, Expr::IntLit(_) | Expr::Local(_) | Expr::Param(_) | Expr::Global(_))
                    {
                        return Ok(Expr::AssignExpr { target, value: Box::new(value) });
                    }
                    let val_expr = match &target {
                        AssignTarget::Local(i) => Expr::Local(*i),
                        AssignTarget::Param(i) => Expr::Param(*i),
                        AssignTarget::Global(g) => Expr::Global(*g),
                        _ => return Err(EmitError::Unsupported(
                            "assign-tail value with unsupported target".to_owned()
                        )),
                    };
                    sides.push(Stmt::Assign { target, value });
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
            // `(c ? a : b)[K]` — index a ternary of arrays by distributing the
            // subscript into both branches (`c ? a[K] : b[K]`); const-prop folds
            // a constant cond to the chosen element. Fixture 2379.
            if let Expr::Ternary { cond, then_arm, else_arm } = &inner
                && matches!(p.peek(), Some(Tok::LBrack))
            {
                p.bump();
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let idx_branch = |br: &Expr| -> Result<Expr, EmitError> {
                    match br {
                        Expr::AddrOfLocal(i) => Ok(if p.local_specs[*i].size == 1 {
                            Expr::LocalIndexByte { local: *i, index: Box::new(index.clone()) }
                        } else {
                            Expr::LocalIndex { local: *i, index: Box::new(index.clone()) }
                        }),
                        Expr::AddrOfGlobal(g) => Ok(if p.globals[*g].element_size == 1 {
                            Expr::IndexByte { array: *g, index: Box::new(index.clone()) }
                        } else {
                            Expr::Index { array: *g, index: Box::new(index.clone()) }
                        }),
                        other => Err(EmitError::Unsupported(format!(
                            "subscript of ternary branch {other:?} not supported"))),
                    }
                };
                let t = idx_branch(then_arm)?;
                let e = idx_branch(else_arm)?;
                return Ok(Expr::Ternary { cond: cond.clone(), then_arm: Box::new(t), else_arm: Box::new(e) });
            }
            // `(*pfn)(args)` / `(pfn)(args)` — call through a function pointer.
            // Dereferencing a function pointer yields the function, so an
            // explicit `(*pfn)` is the same indirect call as `pfn`. Fixture 2414.
            if matches!(p.peek(), Some(Tok::LParen)) {
                let is_fnptr = |e: &Expr| match e {
                    Expr::Local(i) => p.fn_ptr_locals.contains(i),
                    Expr::Param(i) => p.fn_ptr_params.contains(i),
                    Expr::Global(g) => p.global_names.get(*g).is_some_and(|n| p.fn_ptr_globals.contains(n)),
                    _ => false,
                };
                let target = match &inner {
                    Expr::DerefWord { ptr } | Expr::DerefByte { ptr } if is_fnptr(ptr) => Some((**ptr).clone()),
                    e if is_fnptr(e) => Some(e.clone()),
                    _ => None,
                };
                if let Some(target) = target {
                    p.bump(); // `(`
                    let args = parse_call_args(p)?;
                    return Ok(Expr::CallPtr { target: Box::new(target), args });
                }
            }
            // `(*p)[K]` where p is a pointer-to-array param/local (`int (*p)[N]`):
            // the deref decays to the array base, so this reads element K — the
            // same access as `p[K]` on an `int *`. A param uses ParamIndex (load
            // ptr into BX, `[bx+K*elem]`); a local uses the `*(p + K*elem)`
            // pointer-deref form so const-prop folds it through a known alias
            // (`row = &matrix[1]` → moffs). Fixtures 2493, 2329, 2686.
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Expr::DerefWord { ptr } | Expr::DerefByte { ptr } = &inner
                && match ptr.as_ref() {
                    Expr::Param(i) => p.param_names.get(*i).is_some_and(|n| p.ptr_array_stride.contains_key(n)),
                    Expr::Local(i) => p.local_names.get(*i).is_some_and(|n| p.ptr_array_stride.contains_key(n)),
                    _ => false,
                }
            {
                let ptr = (**ptr).clone();
                p.bump(); // `[`
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                if let Expr::Param(i) = ptr {
                    return Ok(Expr::ParamIndex { param: i, index: Box::new(index) });
                }
                let Expr::Local(i) = ptr else { unreachable!() };
                let elem = p.local_specs[i].pointee_size as i32;
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let inner_addr = match index.fold(&init_view) {
                    Some(0) => Expr::Local(i),
                    Some(k) => Expr::BinOp { op: BinOp::Add, left: Box::new(Expr::Local(i)), right: Box::new(Expr::IntLit(k * elem)) },
                    None => Expr::BinOp { op: BinOp::Add, left: Box::new(Expr::Local(i)), right: Box::new(index) },
                };
                return Ok(if elem == 1 {
                    Expr::DerefByte { ptr: Box::new(inner_addr) }
                } else {
                    Expr::DerefWord { ptr: Box::new(inner_addr) }
                });
            }
            // `(*p)++` / `(*p)--` — the parens group the deref, so this mutates
            // the POINTEE (by 1), unlike `*p++` which advances the pointer.
            // Build a PostMutateDeref directly so the generic postfix loop
            // doesn't reparent it as a pointer advance. Fixtures 2857, 3107.
            if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus))
                && let Expr::DerefWord { ptr } | Expr::DerefByte { ptr } = &inner
                && matches!(ptr.as_ref(), Expr::Param(_) | Expr::Local(_) | Expr::Global(_))
            {
                let step = if matches!(p.peek(), Some(Tok::PlusPlus)) { 1 } else { -1 };
                p.bump();
                let is_byte = matches!(inner, Expr::DerefByte { .. });
                let ptr = ptr.clone();
                return Ok(Expr::PostMutateDeref { ptr, step, is_byte });
            }
            // `(p ± K)->field` — pointer arithmetic on a struct pointer then a
            // member read. The struct stride scales K (a negative result is a
            // signed displacement off BX). Fixture 3251 (`(p - 1)->x`).
            if let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = &inner
                && let Expr::Param(i) = left.as_ref()
                && let Some(Some(sidx)) = p.param_struct_idxs.get(*i).cloned()
                && let Expr::IntLit(k) = right.as_ref()
                && matches!(p.peek(), Some(Tok::Arrow))
            {
                let i = *i;
                let stride = p.structs[sidx].total_bytes as i32;
                let signed_k = if matches!(op, BinOp::Sub) { -*k } else { *k };
                let base_off = signed_k * stride;
                p.bump(); // `->`
                let (field_off, size) = parse_field_lookup(p, sidx)?;
                let final_off = (base_off + field_off as i32) as i16 as u16;
                return Ok(Expr::PtrChainField {
                    base: Box::new(Expr::Param(i)), hops: vec![], final_off, final_size: size,
                });
            }
            // `(*p).field` (p a struct pointer) / `(*pp)->field` (pp a pointer to
            // struct pointer) — a pointer member chain. `.` after the deref reads
            // the struct directly (no extra hop); `->` derefs one more level.
            if let Expr::DerefWord { ptr } | Expr::DerefByte { ptr } = &inner
                && let Expr::Param(i) = ptr.as_ref()
                && let Some(Some(sidx)) = p.param_struct_idxs.get(*i).cloned()
                && matches!(p.peek(), Some(Tok::Dot) | Some(Tok::Arrow))
            {
                let i = *i;
                let hops = if matches!(p.peek(), Some(Tok::Arrow)) { vec![0u16] } else { vec![] };
                return continue_chain(p, Expr::Param(i), hops, sidx);
            }
            // `(&s)->field` / `(&s).field` ≡ `s.field` — address-of-struct then
            // member is a direct field access. Fixture 3561.
            if let Expr::AddrOfLocal(i) = &inner
                && let Some(sidx) = p.local_specs.get(*i).and_then(|s| s.struct_idx)
                && matches!(p.peek(), Some(Tok::Dot) | Some(Tok::Arrow))
            {
                let i = *i;
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                if let Some((bit_off, bit_width)) = p.last_field_bits {
                    return Ok(Expr::BitField { base: BitBase::Local(i), byte_off, bit_off, bit_width });
                }
                return Ok(Expr::LocalField { local: i, byte_off, size });
            }
            if let Expr::AddrOfGlobal(g) = &inner
                && let Some(sidx) = p.globals.get(*g).and_then(|gg| gg.struct_idx)
                && matches!(p.peek(), Some(Tok::Dot) | Some(Tok::Arrow))
            {
                let g = *g;
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                if let Some((bit_off, bit_width)) = p.last_field_bits {
                    return Ok(Expr::BitField { base: BitBase::Global(g), byte_off, bit_off, bit_width });
                }
                return Ok(Expr::GlobalField { global: g, byte_off, size });
            }
            Ok(inner)
        }
        Some(Tok::Int(n)) => Ok(Expr::IntLit(n)),
        Some(Tok::Float(bits, double)) => Ok(Expr::FloatLit(bits, double)),
        Some(Tok::PlusPlus) => {
            let inner = parse_atom(p)?;
            // Prefix `++` on a pointer advances by the pointee element size
            // (`++p` on `int *p` adds 2), not 1. Fixture 561.
            let gstep = |idx: usize| if p.globals[idx].is_pointer { p.globals[idx].element_size as i32 } else { 1 };
            let lstep = |idx: usize| { let s = p.local_specs[idx].pointee_size; if s > 0 { s as i32 } else { 1 } };
            match inner {
                Expr::Local(idx) => Ok(Expr::PreMutateLocal { local_idx: idx, step: lstep(idx) }),
                Expr::Global(idx) => Ok(Expr::PreMutateGlobal { global_idx: idx, step: gstep(idx) }),
                Expr::Param(idx) => Ok(Expr::PreMutateParam { param_idx: idx, step: 1 }),
                Expr::DerefWord { ptr } => Ok(Expr::PreMutateDeref { ptr, step: 1, is_byte: false }),
                Expr::DerefByte { ptr } => Ok(Expr::PreMutateDeref { ptr, step: 1, is_byte: true }),
                Expr::Index { array, index } if matches!(index.as_ref(), Expr::Param(_) | Expr::Local(_)) =>
                    Ok(Expr::PreMutateIndexedGlobal { array, index, step: 1, is_byte: false }),
                Expr::IndexByte { array, index } if matches!(index.as_ref(), Expr::Param(_) | Expr::Local(_)) =>
                    Ok(Expr::PreMutateIndexedGlobal { array, index, step: 1, is_byte: true }),
                Expr::GlobalField { global, byte_off, size } =>
                    Ok(Expr::PreMutateGlobalField { global, byte_off, size, step: 1 }),
                // `++a[K]` on a global array at a constant index — same in-place
                // `inc word/byte [a+off]; mov ax,[a+off]` shape as a struct field.
                Expr::Index { array, index } if matches!(index.as_ref(), Expr::IntLit(_)) =>
                    Ok(Expr::PreMutateGlobalField { global: array, byte_off: (index.fold(&[]).unwrap() * 2) as u16, size: 2, step: 1 }),
                Expr::IndexByte { array, index } if matches!(index.as_ref(), Expr::IntLit(_)) =>
                    Ok(Expr::PreMutateGlobalField { global: array, byte_off: index.fold(&[]).unwrap() as u16, size: 1, step: 1 }),
                other => Ok(Expr::BinOp {
                    op: BinOp::Add,
                    left: Box::new(other),
                    right: Box::new(Expr::IntLit(1)),
                }),
            }
        }
        Some(Tok::MinusMinus) => {
            let inner = parse_atom(p)?;
            let gstep = |idx: usize| if p.globals[idx].is_pointer { p.globals[idx].element_size as i32 } else { 1 };
            let lstep = |idx: usize| { let s = p.local_specs[idx].pointee_size; if s > 0 { s as i32 } else { 1 } };
            match inner {
                Expr::Local(idx) => Ok(Expr::PreMutateLocal { local_idx: idx, step: -lstep(idx) }),
                Expr::Global(idx) => Ok(Expr::PreMutateGlobal { global_idx: idx, step: -gstep(idx) }),
                Expr::Param(idx) => Ok(Expr::PreMutateParam { param_idx: idx, step: -1 }),
                Expr::DerefWord { ptr } => Ok(Expr::PreMutateDeref { ptr, step: -1, is_byte: false }),
                Expr::DerefByte { ptr } => Ok(Expr::PreMutateDeref { ptr, step: -1, is_byte: true }),
                Expr::GlobalField { global, byte_off, size } =>
                    Ok(Expr::PreMutateGlobalField { global, byte_off, size, step: -1 }),
                Expr::Index { array, index } if matches!(index.as_ref(), Expr::IntLit(_)) =>
                    Ok(Expr::PreMutateGlobalField { global: array, byte_off: (index.fold(&[]).unwrap() * 2) as u16, size: 2, step: -1 }),
                Expr::IndexByte { array, index } if matches!(index.as_ref(), Expr::IntLit(_)) =>
                    Ok(Expr::PreMutateGlobalField { global: array, byte_off: index.fold(&[]).unwrap() as u16, size: 1, step: -1 }),
                other => Ok(Expr::BinOp {
                    op: BinOp::Sub,
                    left: Box::new(other),
                    right: Box::new(Expr::IntLit(1)),
                }),
            }
        }
        Some(Tok::Bang) => {
            // `!<expr>` — equivalent to `<expr> == 0`. A nested not
            // (`!!x` → `(x == 0) == 0`) collapses to `x != 0` / `x == 0`
            // so the value form takes the carry-trick path (`cmp [x],1;
            // sbb ax,ax; inc ax`) like a source-level `x != 0`. 3140, 3415.
            let inner = parse_atom(p)?;
            if let Expr::BinOp { op: op @ (BinOp::Eq | BinOp::Ne), left, right } = &inner
                && matches!(right.as_ref(), Expr::IntLit(0))
            {
                return Ok(Expr::BinOp {
                    op: if matches!(op, BinOp::Eq) { BinOp::Ne } else { BinOp::Eq },
                    left: left.clone(),
                    right: Box::new(Expr::IntLit(0)),
                });
            }
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
            // `"abc"[K]` — byte at constant offset K of the literal.
            if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let index = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                let k = index.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
                    "non-constant string-literal index not yet supported".to_owned()))?;
                return Ok(Expr::StrLitByte { string_idx: idx, byte_off: k as u16 });
            }
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
            } else if let Some(Tok::Kw(
                kw @ ("int" | "char" | "long" | "double" | "float"),
            )) = p.peek().cloned()
            {
                p.bump();
                if kw == "long" && matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
                if matches!(p.peek(), Some(Tok::Star)) {
                    // pointer to any type = 2 in the small model
                    while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                    2
                } else {
                    match kw {
                        "char" => 1,
                        "int" => 2,
                        "long" | "float" => 4,
                        "double" => 8,
                        _ => 2,
                    }
                }
            } else if let Some(Tok::StrLit(b)) = p.peek().cloned() {
                // `sizeof("...")` → byte length + 1 for the NUL terminator.
                p.bump();
                (b.len() + 1) as i32
            } else if matches!(p.peek(), Some(Tok::Star)) {
                // `sizeof(*ptr)` → the pointee size (no evaluation).
                p.bump();
                let sz = if let Some(Tok::Ident(name)) = p.peek().cloned() {
                    p.bump();
                    if let Some(idx) = p.resolve_local(&name) {
                        let ls = &p.local_specs[idx];
                        if ls.pointee_size > 0 { ls.pointee_size as i32 } else { ls.size as i32 }
                    } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                        let ps = p.param_pointee_sizes.get(idx).copied().unwrap_or(0);
                        if ps > 0 { ps as i32 } else { 2 }
                    } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                        p.globals[idx].element_size as i32
                    } else { 2 }
                } else { 2 };
                if has_paren { skip_balanced_to_rparen(p); }
                sz
            } else if let Some(Tok::Ident(name)) = p.peek().cloned() {
                p.bump();
                // `sizeof(a[K])` → the array's ELEMENT size (the index is
                // irrelevant; consumed and discarded). `sizeof(a)` → full storage.
                let is_elem = matches!(p.peek(), Some(Tok::LBrack));
                if is_elem {
                    p.bump();
                    let _ = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                }
                let base = if let Some(idx) = p.resolve_local(&name) {
                    if is_elem { p.local_specs[idx].size as i32 }
                    else { p.local_specs[idx].storage_bytes() as i32 }
                } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                    // A param array has decayed to a 2-byte pointer; a scalar
                    // param sizes by its type.
                    if p.param_is_long[idx] { 4 } else if p.param_is_char[idx] { 1 } else { 2 }
                } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                    let g = &p.globals[idx];
                    if is_elem { g.element_size as i32 } else { (g.element_size * g.array_len) as i32 }
                } else {
                    return Err(EmitError::Unsupported(format!(
                        "sizeof unknown identifier `{name}`"
                    )));
                };
                // If the operand continues (`sizeof(x + 1)`), the result type
                // is the expression's type — int (2), or long (4) if this
                // operand is long. Consume the remaining operand tokens.
                if has_paren && !matches!(p.peek(), Some(Tok::RParen)) {
                    let is_long = p.resolve_local(&name)
                        .map(|i| p.local_specs[i].is_long)
                        .unwrap_or(false);
                    skip_balanced_to_rparen(p);
                    if is_long { 4 } else { 2 }
                } else {
                    base
                }
            } else {
                // General unevaluated expression (`sizeof(++i)`, `sizeof(1+2)`)
                // — the result type is int. Consume the operand tokens without
                // emitting (so side effects in the operand never run).
                if has_paren { skip_balanced_to_rparen(p); }
                2
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
            // `&<ident>.field` — address of a struct field. Resolve the field
            // offset and synthesize `<base-addr> + field_off` (off 0 → the base
            // address itself). Fixtures 485, 3262.
            if matches!(p.peek(), Some(Tok::Dot)) {
                p.bump(); // `.`
                let fname = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected field name after `&{name}.`, got {other:?}"
                        )));
                    }
                };
                let (base, sidx) = if let Some(li) = p.resolve_local(&name) {
                    (Expr::AddrOfLocal(li), p.local_specs[li].struct_idx)
                } else if let Some(gi) = p.global_names.iter().position(|n| *n == name) {
                    (Expr::AddrOfGlobal(gi), p.globals[gi].struct_idx)
                } else {
                    return Err(EmitError::Unsupported(format!(
                        "address-of unknown identifier `{name}`"
                    )));
                };
                let sidx = sidx.ok_or_else(|| EmitError::Unsupported(format!(
                    "`&{name}.{fname}` on a non-struct"
                )))?;
                let off = p.structs[sidx].fields.iter().find(|f| f.name == fname)
                    .ok_or_else(|| EmitError::Unsupported(format!(
                        "unknown field `{fname}` in `&{name}.{fname}`"
                    )))?.byte_off as i32;
                if off == 0 {
                    return Ok(base);
                }
                return Ok(Expr::BinOp {
                    op: BinOp::Add,
                    left: Box::new(base),
                    right: Box::new(Expr::IntLit(off)),
                });
            }
            // `&<ident>[K]` — address of an array element. Synthesize
            // `<base-addr> + K*elem_size` as a BinOp.
            if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let idx_expr = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                if let Some(local_idx) = p.resolve_local(&name) {
                    let elem_size = p.local_specs[local_idx].size as i32;
                    // A subscript on the OUTER dimension of a multi-D array
                    // (`&m[1]` for `int m[2][3]`) yields a ROW; its address
                    // strides by the row size (inner dims product * elem). 2493.
                    let stride = match p.local_dims.get(&local_idx) {
                        Some(dims) if dims.len() > 1 => dims[1..].iter().product::<usize>() as i32 * elem_size,
                        _ => elem_size,
                    };
                    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                    let k = idx_expr.fold(&init_view).ok_or_else(|| EmitError::Unsupported(
                        "non-constant index in `&<local>[K]` not yet supported".to_owned()
                    ))?;
                    return Ok(Expr::BinOp {
                        op: BinOp::Add,
                        left: Box::new(Expr::AddrOfLocal(local_idx)),
                        right: Box::new(Expr::IntLit(k * stride)),
                    });
                }
                if let Some(global_idx) = p.global_names.iter().position(|n| *n == name) {
                    let g = &p.globals[global_idx];
                    let elem_size = g.element_size as i32;
                    let stride = match p.global_dims.get(&global_idx) {
                        Some(dims) if dims.len() > 1 => dims[1..].iter().product::<usize>() as i32 * elem_size,
                        _ => elem_size,
                    };
                    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                    if let Some(k) = idx_expr.fold(&init_view) {
                        return Ok(Expr::BinOp {
                            op: BinOp::Add,
                            left: Box::new(Expr::AddrOfGlobal(global_idx)),
                            right: Box::new(Expr::IntLit(k * stride)),
                        });
                    }
                    // Runtime index → `<i→ax>; shl ax; add ax,OFFSET _arr`.
                    return Ok(Expr::AddrOfIndexedGlobal { array: global_idx, index: Box::new(idx_expr), elem: elem_size as u8 });
                }
                // `&p[n]` on a pointer PARAM ≡ `p + n` (pointer arithmetic): the
                // param's value is already the base address, so the same
                // pointer-scaling BinOp the bare `p + n` builds applies. Fixture
                // 2978.
                if let Some(pi) = p.param_names.iter().position(|n| *n == name)
                    && p.param_pointee_sizes.get(pi).copied().unwrap_or(0) > 0
                {
                    return Ok(Expr::BinOp {
                        op: BinOp::Add,
                        left: Box::new(Expr::Param(pi)),
                        right: Box::new(idx_expr),
                    });
                }
                return Err(EmitError::Unsupported(format!(
                    "address-of unknown identifier `{name}`"
                )));
            }
            if let Some(idx) = p.resolve_local(&name) {
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
            // variant from the inner expression's pointee type. Clear any
            // stale pointer-cast pointee first; a cast inside `inner`
            // (`*(char *)p`) sets it, and we consume it below.
            p.cast_ptr_pointee = None;
            let inner = parse_atom(p)?;
            let cast_pointee = p.cast_ptr_pointee.take();
            // `*arr[i]` on an array-of-pointers — deref the pointer element,
            // equivalent to `arr[i][0]`. Fixture 2608.
            if let Expr::PtrArrayElem { array, index } = inner {
                let elem_size = p.globals[array].element_size as u8;
                return Ok(Expr::PtrArrayDeref {
                    array, index, inner: Box::new(Expr::IntLit(0)), elem_size,
                });
            }
            // An explicit pointer cast on the operand (`*(char *)p`) overrides
            // the inferred pointee size so the access width matches the cast.
            // A call operand (`*nextp(...)`) takes the callee's recorded return
            // pointee size, so `*<char*-returning call>` is a byte deref. 1343.
            let pointee_size = cast_pointee
                .or_else(|| match &inner {
                    Expr::Call { name, .. } => p.fn_return_pointee.get(&symbol_name(name)).copied(),
                    _ => None,
                })
                .unwrap_or_else(|| pointee_size_of(&inner, &p.globals, &p.local_specs, &p.param_pointee_sizes));
            // `*(p + K)` over a scalar pointer LOCAL: scale the literal index
            // by the pointee size, matching the byte-scaled `p[K]` subscript
            // production — the alias-fold and emit paths see one convention.
            // Fixtures 1152, 2646.
            let inner = match inner {
                Expr::BinOp { op: BinOp::Add, left, right } => {
                    if let (Expr::Local(pi), Expr::IntLit(k)) = (left.as_ref(), right.as_ref())
                        && let Some(spec) = p.local_specs.get(*pi)
                        && spec.pointee_size > 1
                        && spec.array_len == 1
                    {
                        Expr::BinOp {
                            op: BinOp::Add,
                            left,
                            right: Box::new(Expr::IntLit(k * spec.pointee_size as i32)),
                        }
                    } else {
                        Expr::BinOp { op: BinOp::Add, left, right }
                    }
                }
                other => other,
            };
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
                // Indirect call when `name` is a function-pointer variable: a
                // fnptr local (shadows all), param (shadows globals), or global.
                if let Some(idx) = p.resolve_local(&name)
                    && p.fn_ptr_locals.contains(&idx)
                {
                    return Ok(Expr::CallPtr { target: Box::new(Expr::Local(idx)), args });
                }
                if let Some(idx) = p.param_names.iter().position(|n| *n == name)
                    && p.fn_ptr_params.contains(&idx)
                {
                    return Ok(Expr::CallPtr { target: Box::new(Expr::Param(idx)), args });
                }
                if p.fn_ptr_globals.contains(&name)
                    && let Some(idx) = p.global_names.iter().position(|n| *n == name)
                {
                    return Ok(Expr::CallPtr { target: Box::new(Expr::Global(idx)), args });
                }
                // `make().field` — member access on a by-value struct return. Spill
                // the DX:AX result to a frame temp, then read the field. Fixtures
                // 2629/2634.
                if matches!(p.peek(), Some(Tok::Dot) | Some(Tok::Arrow))
                    && let Some(&sidx) = p.fn_return_struct_idx.get(&symbol_name(&name))
                {
                    p.bump(); // `.` / `->`
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    let temp_idx = p.struct_field_temp_count;
                    p.struct_field_temp_count += 1;
                    return Ok(Expr::CallStructField { name, args, byte_off, size, temp_idx });
                }
                // `fn()[K]` — index the pointer a call returns. The fn's return
                // pointee size picks byte vs word deref; K scales by it. The deref
                // ptr is the Call itself (emit_load_bx evaluates it into BX). 1227.
                if matches!(p.peek(), Some(Tok::LBrack))
                    && let Some(&pointee) = p.fn_return_pointee.get(&symbol_name(&name))
                {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    let call = Expr::Call { name, args };
                    let ptr = if matches!(index, Expr::IntLit(0)) {
                        call
                    } else {
                        let scaled = if pointee == 1 { index }
                            else { Expr::BinOp { op: BinOp::Mul, left: Box::new(index), right: Box::new(Expr::IntLit(pointee as i32)) } };
                        Expr::BinOp { op: BinOp::Add, left: Box::new(call), right: Box::new(scaled) }
                    };
                    return Ok(if pointee == 1 {
                        Expr::DerefByte { ptr: Box::new(ptr) }
                    } else {
                        Expr::DerefWord { ptr: Box::new(ptr) }
                    });
                }
                return Ok(Expr::Call { name, args });
            }
            // Enum constants substitute directly to their literal value.
            if let Some(&v) = p.enum_consts.get(&name) {
                return Ok(Expr::IntLit(v));
            }
            if let Some(idx) = p.resolve_local(&name) {
                // `<local>[<expr>]` — element access on a local
                // array. Picks the byte-load + cbw variant for char
                // arrays, word load otherwise.
                // `<struct-array-local>[K].<field>` — element access into an array
                // of structs. byte_off = K*sizeof(S) + field_off. Constant K.
                if matches!(p.peek(), Some(Tok::LBrack))
                    && let Some(sidx) = p.local_specs[idx].struct_idx
                    && p.local_specs[idx].pointee_size == 0 // value array, not a pointer
                {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                    let stotal = p.structs[sidx].total_bytes;
                    p.eat(&Tok::Dot)?;
                    let (field_off, size) = parse_field_lookup(p, sidx)?;
                    // Constant index → fold to a plain LocalField. A non-constant
                    // index defers to LocalStructArrayField (const-prop may still
                    // fold it once the index value is known; otherwise runtime
                    // codegen scales si by the struct stride). Fixtures 1821/1914,
                    // and 2438 (`i=2` folded by const-prop, not the decl view).
                    if let Some(k) = index.fold(&init_view) {
                        let byte_off = u16::try_from(k as i64 * stotal as i64 + field_off as i64)
                            .expect("struct-array field offset fits");
                        return Ok(Expr::LocalField { local: idx, byte_off, size });
                    }
                    return Ok(Expr::LocalStructArrayField {
                        local: idx, index: Box::new(index),
                        stride: stotal as u16, field_off, size,
                    });
                }
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    // Multidimensional local `a[i][j]`: constant → flat 1-D read;
                    // runtime 2-D → Expr::Index2D.
                    if let Some(dims) = p.local_dims.get(&idx).cloned()
                        && let Some(ms) = parse_multidim_sub(p, &index, &dims)?
                    {
                        let elem = p.local_specs[idx].size;
                        match ms {
                            MultiSub::Flat(flat) => {
                                let fi = Box::new(Expr::IntLit(flat));
                                return if elem == 1 {
                                    Ok(Expr::LocalIndexByte { local: idx, index: fi })
                                } else {
                                    Ok(Expr::LocalIndex { local: idx, index: fi })
                                };
                            }
                            MultiSub::Runtime(mut ix) if ix.len() == 2 => {
                                let col = Box::new(ix.pop().unwrap());
                                let row = Box::new(ix.pop().unwrap());
                                return Ok(Expr::Index2D { is_global: false, base: idx, row, col, cols: dims[1], elem });
                            }
                            MultiSub::Runtime(_) => return Err(EmitError::Unsupported(
                                "runtime index on a >2-D local array not yet supported".to_owned())),
                        }
                    }
                    // A scalar POINTER local: `p[K]` is `*(p + K)` — a deref, not
                    // an array-slot read. K==0 yields a bare `*p` so the alias
                    // pass can fold it; K!=0 derefs `p + K*pointee`. An ARRAY of
                    // pointers (`int *p[N]`, array_len>1) instead reads the K-th
                    // element (a pointer) via the LocalIndex fall-through. 1565.
                    let ptsz = p.local_specs[idx].pointee_size;
                    if ptsz > 0 && p.local_specs[idx].array_len == 1 {
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
                    // `ops[i](args)` — call through a function-pointer array
                    // element (a word element followed by a call). Fixture 2435.
                    if p.local_specs[idx].size == 2 && matches!(p.peek(), Some(Tok::LParen)) {
                        let target = Expr::LocalIndex { local: idx, index: Box::new(index) };
                        p.bump(); // `(`
                        let args = parse_call_args(p)?;
                        return Ok(Expr::CallPtr { target: Box::new(target), args });
                    }
                    // Array-of-pointers local (`char *strs[N]`): `strs[i]` is the
                    // pointer element; a following `[j]` derefs it (`strs[i][j]`).
                    // Mirrors the global PtrArrayDeref path; const-prop folds when
                    // the element aliases a known string. Fixtures 1710, 1921.
                    if ptsz > 0 && matches!(p.peek(), Some(Tok::LBrack)) {
                        p.bump();
                        let inner = parse_expr(p)?;
                        p.eat(&Tok::RBrack)?;
                        return Ok(Expr::LocalPtrArrayDeref {
                            local: idx, index: Box::new(index),
                            inner: Box::new(inner), elem_size: ptsz as u8,
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
                    // `a.ptr->...` — the field is a struct pointer; chain into it.
                    // The pointer field load is the chain base. Fixtures 1928, 1419.
                    if let Some(fname) = match p.toks.get(p.pos + 1) { Some(Tok::Ident(s)) => Some(s.clone()), _ => None }
                        && let Some(f) = p.structs[sidx].fields.iter().find(|f| f.name == fname).cloned()
                        && f.is_pointer && f.struct_idx.is_some()
                        && matches!(p.toks.get(p.pos + 2), Some(Tok::Arrow) | Some(Tok::Dot))
                    {
                        p.bump(); p.bump(); // `.`, field
                        let base = Expr::LocalField { local: idx, byte_off: f.byte_off, size: 2 };
                        return continue_chain(p, base, vec![], f.struct_idx.unwrap());
                    }
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    if let Some((bit_off, bit_width)) = p.last_field_bits {
                        return Ok(Expr::BitField { base: BitBase::Local(idx), byte_off, bit_off, bit_width });
                    }
                    let field = Expr::LocalField { local: idx, byte_off, size };
                    // `op.fn(args)` — calling a function-pointer field. Fixture 2378.
                    if matches!(p.peek(), Some(Tok::LParen)) {
                        p.bump();
                        let args = parse_call_args(p)?;
                        return Ok(Expr::CallPtr { target: Box::new(field), args });
                    }
                    return Ok(field);
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
                    // `argv[i][j]` — double-pointer param: index the pointer array
                    // then deref the element pointer. The recorded final-element
                    // size picks byte/word. Fixture 2962.
                    if matches!(p.peek(), Some(Tok::LBrack))
                        && let Some(&elem) = p.param_dptr_elem.get(&name)
                    {
                        p.bump();
                        let inner = parse_expr(p)?;
                        p.eat(&Tok::RBrack)?;
                        return Ok(Expr::ParamPtrArrayDeref {
                            param: idx, index: Box::new(index),
                            inner: Box::new(inner), elem_size: elem as u8,
                        });
                    }
                    // `pts[i].field` — index a struct-POINTER param then a field.
                    // Constant index folds to a DerefParamField; a runtime index
                    // defers to ParamStructArrayField (si = pts + i*stride). 2208.
                    if matches!(p.peek(), Some(Tok::Dot))
                        && let Some(Some(sidx)) = p.param_struct_idxs.get(idx).cloned()
                        && p.param_pointee_sizes.get(idx).copied().unwrap_or(0) > 0
                    {
                        let stotal = p.structs[sidx].total_bytes;
                        p.bump(); // .
                        let (field_off, size) = parse_field_lookup(p, sidx)?;
                        let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                        if let Some(k) = index.fold(&init_view) {
                            let byte_off = u16::try_from(k as i64 * stotal as i64 + field_off as i64)
                                .expect("struct-ptr-param field offset fits");
                            return Ok(Expr::DerefParamField { ptr_param: idx, byte_off, size });
                        }
                        return Ok(Expr::ParamStructArrayField {
                            param: idx, index: Box::new(index),
                            stride: stotal as u16, field_off, size,
                        });
                    }
                    // 2-D array param `a[i][j]` (`int a[2][3]` decays to `int(*)[3]`):
                    // fold the trailing subscript(s) into a flat element index.
                    if let Some(dims) = p.param_dims.get(&idx).cloned()
                        && let Some(ms) = parse_multidim_sub(p, &index, &dims)?
                    {
                        let elem = p.param_pointee_sizes.get(idx).copied().unwrap_or(2);
                        let flat = match ms {
                            MultiSub::Flat(f) => f,
                            MultiSub::Runtime(_) => return Err(EmitError::Unsupported(
                                "runtime 2-D array parameter index not yet supported".to_owned())),
                        };
                        if elem == 1 {
                            let ptr = if flat == 0 { Expr::Param(idx) }
                                else { Expr::BinOp { op: BinOp::Add, left: Box::new(Expr::Param(idx)), right: Box::new(Expr::IntLit(flat)) } };
                            return Ok(Expr::DerefByte { ptr: Box::new(ptr) });
                        }
                        return Ok(Expr::ParamIndex { param: idx, index: Box::new(Expr::IntLit(flat)) });
                    }
                    // `char *` / `char []` param subscript → byte deref + widen.
                    // `s[0]` is `*s`; `s[K]` is `*(s + K)` (fixtures 2618/2919).
                    if p.param_pointee_sizes.get(idx).copied().unwrap_or(0) == 1 {
                        let ptr = if matches!(index, Expr::IntLit(0)) {
                            Expr::Param(idx)
                        } else {
                            Expr::BinOp { op: BinOp::Add, left: Box::new(Expr::Param(idx)), right: Box::new(index) }
                        };
                        return Ok(Expr::DerefByte { ptr: Box::new(ptr) });
                    }
                    return Ok(Expr::ParamIndex { param: idx, index: Box::new(index) });
                }
                // `<struct-ptr-param>-><field>` member access.
                if matches!(p.peek(), Some(Tok::Arrow))
                    && let Some(Some(sidx)) = p.param_struct_idxs.get(idx).cloned()
                {
                    if let Some(chain) = try_build_chain(p, Expr::Param(idx), sidx)? {
                        return Ok(chain);
                    }
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    if let Some((bit_off, bit_width)) = p.last_field_bits {
                        return Ok(Expr::BitField { base: BitBase::DerefParam(idx), byte_off, bit_off, bit_width });
                    }
                    return Ok(Expr::DerefParamField { ptr_param: idx, byte_off, size });
                }
                // `<struct-value-param>.<field>` — field of a by-value struct
                // param (struct type, not a pointer: pointee_size == 0).
                if matches!(p.peek(), Some(Tok::Dot))
                    && let Some(Some(sidx)) = p.param_struct_idxs.get(idx).cloned()
                    && p.param_pointee_sizes.get(idx).copied().unwrap_or(0) == 0
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    return Ok(Expr::ParamField { param: idx, byte_off, size });
                }
                Ok(Expr::Param(idx))
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                // `<struct-array-global>[K].<field>` — element of an array of
                // structs → GlobalField at K*sizeof(S) + field_off (constant K).
                if matches!(p.peek(), Some(Tok::LBrack))
                    && let Some(sidx) = p.globals[idx].struct_idx
                    && !p.globals[idx].is_pointer
                {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    let stotal = p.structs[sidx].total_bytes;
                    p.eat(&Tok::Dot)?;
                    let (field_off, size) = parse_field_lookup(p, sidx)?;
                    let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                    if let Some(k) = index.fold(&init_view) {
                        let byte_off = u16::try_from(k as i64 * stotal as i64 + field_off as i64)
                            .expect("struct-array field offset fits");
                        return Ok(Expr::GlobalField { global: idx, byte_off, size });
                    }
                    // Runtime index → scale at codegen time.
                    return Ok(Expr::StructArrayField {
                        array: idx, index: Box::new(index),
                        stride: stotal as u16, field_off, size,
                    });
                }
                // `<global>[<expr>]` — array index or pointer index.
                // Array (`int a[N]`): direct addressing.
                // Pointer (`char *p`): load pointer first, then offset.
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    // Array-of-pointers (`char *names[]`, `int *table[]`):
                    // `arr[i]` is the pointer VALUE at element i; a following
                    // `[j]` derefs it (`arr[i][j]`). Distinguished from a scalar
                    // pointer (`char *p`, array_len==1) by array_len>1.
                    {
                        let g = &p.globals[idx];
                        if g.is_pointer && g.array_len > 1 {
                            let elem_size = g.element_size as u8;
                            // `ops[i](args)` — call through a fn-ptr array element.
                            // Fixtures 2944, 3696.
                            if matches!(p.peek(), Some(Tok::LParen)) {
                                let target = Expr::Index { array: idx, index: Box::new(index) };
                                p.bump();
                                let args = parse_call_args(p)?;
                                return Ok(Expr::CallPtr { target: Box::new(target), args });
                            }
                            if matches!(p.peek(), Some(Tok::LBrack)) {
                                p.bump();
                                let inner = parse_expr(p)?;
                                p.eat(&Tok::RBrack)?;
                                return Ok(Expr::PtrArrayDeref {
                                    array: idx, index: Box::new(index),
                                    inner: Box::new(inner), elem_size,
                                });
                            }
                            // `arr[i]->field` on an array of STRUCT pointers:
                            // the element is a struct pointer; chain into it.
                            // Fixtures 3541, 2997.
                            if let Some(sidx) = g.struct_idx
                                && matches!(p.peek(), Some(Tok::Arrow) | Some(Tok::Dot))
                            {
                                let base = Expr::PtrArrayElem { array: idx, index: Box::new(index) };
                                return continue_chain(p, base, vec![], sidx);
                            }
                            return Ok(Expr::PtrArrayElem { array: idx, index: Box::new(index) });
                        }
                    }
                    // Multidimensional `a[i][j]`: constant indices fold to a flat
                    // 1-D read; a runtime 2-D index becomes Expr::Index2D.
                    if let Some(dims) = p.global_dims.get(&idx).cloned()
                        && let Some(ms) = parse_multidim_sub(p, &index, &dims)?
                    {
                        let g = &p.globals[idx];
                        let elem = g.element_size;
                        match ms {
                            MultiSub::Flat(flat) => {
                                let fi = Box::new(Expr::IntLit(flat));
                                return Ok(if elem == 1 {
                                    Expr::IndexByte { array: idx, index: fi }
                                } else {
                                    Expr::Index { array: idx, index: fi }
                                });
                            }
                            MultiSub::Runtime(mut ix) if ix.len() == 2 => {
                                let col = Box::new(ix.pop().unwrap());
                                let row = Box::new(ix.pop().unwrap());
                                return Ok(Expr::Index2D { is_global: true, base: idx, row, col, cols: dims[1], elem });
                            }
                            MultiSub::Runtime(_) => return Err(EmitError::Unsupported(
                                "runtime index on a >2-D array not yet supported".to_owned())),
                        }
                    }
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
                    // `g.ptr->...` — struct-pointer field of a global struct → chain.
                    if let Some(fname) = match p.toks.get(p.pos + 1) { Some(Tok::Ident(s)) => Some(s.clone()), _ => None }
                        && let Some(f) = p.structs[sidx].fields.iter().find(|f| f.name == fname).cloned()
                        && f.is_pointer && f.struct_idx.is_some()
                        && matches!(p.toks.get(p.pos + 2), Some(Tok::Arrow) | Some(Tok::Dot))
                    {
                        p.bump(); p.bump(); // `.`, field
                        let base = Expr::GlobalField { global: idx, byte_off: f.byte_off, size: 2 };
                        return continue_chain(p, base, vec![], f.struct_idx.unwrap());
                    }
                    p.bump();
                    // `<global-struct>.<array-field>[i]` with a RUNTIME index → a
                    // runtime-indexed global read with the field's element offset
                    // folded into the index (`_g[bx + field_off]`). Constant index
                    // still folds to a GlobalField. Fixtures 2940, 3422.
                    if let Some(Tok::Ident(fname)) = p.peek().cloned()
                        && matches!(p.toks.get(p.pos + 1), Some(Tok::LBrack))
                        && let Some(f) = p.structs[sidx].fields.iter().find(|ff| ff.name == fname).cloned()
                        && !f.is_pointer && f.bit_width == 0
                    {
                        p.bump(); // field name
                        p.bump(); // [
                        let index = parse_expr(p)?;
                        p.eat(&Tok::RBrack)?;
                        let init_view: Vec<Option<i32>> = p.local_specs.iter().map(|l| l.init).collect();
                        let elem = f.size.max(1) as i32;
                        if let Some(k) = index.fold(&init_view) {
                            let byte_off = u16::try_from(f.byte_off as i64 + k as i64 * elem as i64)
                                .expect("struct array-field offset fits");
                            return Ok(Expr::GlobalField { global: idx, byte_off, size: f.size });
                        }
                        // Runtime: fold the field's element offset into the index so
                        // `Expr::Index` emits `_g[bx + field_off]`.
                        let elem_off = f.byte_off as i32 / elem;
                        let idx_expr = if elem_off == 0 { index } else {
                            Expr::BinOp { op: BinOp::Add, left: Box::new(index), right: Box::new(Expr::IntLit(elem_off)) }
                        };
                        return Ok(if f.size == 1 {
                            Expr::IndexByte { array: idx, index: Box::new(idx_expr) }
                        } else {
                            Expr::Index { array: idx, index: Box::new(idx_expr) }
                        });
                    }
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    if let Some((bit_off, bit_width)) = p.last_field_bits {
                        Ok(Expr::BitField { base: BitBase::Global(idx), byte_off, bit_off, bit_width })
                    } else {
                        Ok(Expr::GlobalField { global: idx, byte_off, size })
                    }
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
                    // A struct global's `array_len` is BYTE storage (stotal*count),
                    // so a single struct also has array_len>1. Decay only a genuine
                    // struct ARRAY (count>1, i.e. storage exceeds one struct) to its
                    // base address — a bare `struct S g` stays a value. Fixture 2208.
                    let struct_is_array = match g.struct_idx {
                        Some(sidx) => g.array_len > p.structs[sidx].total_bytes,
                        None => true,
                    };
                    if !g.is_pointer && is_array && struct_is_array {
                        Ok(Expr::AddrOfGlobal(idx))
                    } else {
                        Ok(Expr::Global(idx))
                    }
                }
            } else if p.fn_names.contains(&name) {
                // A bare function name in value position is its address
                // (`apply(sq, 6)` passes `OFFSET _sq`). Fixture 2314.
                Ok(Expr::FuncAddr(name))
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
        // `parse_assign_rhs` so an assignment-as-argument (`f(n = 7)`) parses to
        // an AssignExpr; a plain arg is returned unchanged. Fixture 1816.
        args.push(parse_assign_rhs(p)?);
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
