# MSC parser — what we accept so far

Our `crates/msc` front-end is a hand-written recursive-descent parser
over the same Phase-1 subset BCC's front-end covers, with MSC-specific
acceptance shaped by the corpus.

## Topic index (write as we discover)

- [`TYPES.md`](TYPES.md) — recognized type prefixes (`int`, `char`,
  `int *`, `char *`, `int [N]`, `char [N]`, pointer params).
- [`EXPRESSIONS.md`](EXPRESSIONS.md) — atom shapes: int literal,
  identifier, call, string literal, address-of, deref, array index.
- [`STATEMENTS.md`](STATEMENTS.md) — `return`, `if/else`, `while`,
  `do-while`, `for`, expression statement, deref-store, indexed-store.
- [`DECLARATIONS.md`](DECLARATIONS.md) — file-scope global decls,
  initializer shapes (literal, brace-list, string literal,
  address-of-global), function definitions.

## Grammar fragments accepted in Phase 1

```
unit         := { preproc-line | global-decl | function-def }
global-decl  := type-prefix ident [ "[" int "]" ] [ "=" init ] ";"
type-prefix  := "int" | "int" "*" | "char" "*" | "char"
init         := signed-int
              | "{" signed-int ("," signed-int)* "}"
              | string-literal       -- only after char*/char[] decl
              | "&" ident            -- only after int* decl
function-def := type-prefix ident "(" params ")" "{" body "}"
params       := "void"
              | param ("," param)*
param        := "int" [ "*" ] ident
body         := { decl }* { stmt }*
decl         := "int" ident [ "=" signed-int ] ";"
stmt         := "return" expr ";"
              | "if" "(" cond ")" stmt [ "else" stmt ]
              | "while" "(" cond ")" stmt
              | "do" stmt "while" "(" cond ")" ";"
              | "for" "(" assign ";" cond ";" assign ")" stmt
              | ident "=" expr ";"
              | ident "[" expr "]" "=" expr ";"
              | "*" ident "=" expr ";"
              | ident "(" args ")" ";"
              | ";"
expr         := atom [ ("+" | "-" | "*") atom ]
atom         := int-literal
              | "-" int-literal
              | string-literal
              | "&" ident
              | "*" atom
              | ident
              | ident "[" expr "]"
              | ident "(" args ")"
cond         := expr [ ("==" | "!=") expr ]
```

## Disambiguations

- `int <ident> ...;` vs `int <ident>(`: lookahead-2 picks global vs function.
- `int * <ident>`: always a global pointer decl (since it can't start a
  function definition with that lookahead shape).
- `char <ident> [`: char-array global decl.
- `*<ident>` at statement start: deref-store target.

## What we do not yet accept

- Multi-line C-style comments (the source comes pre-stripped from the
  oracle's input).
- The `register`, `static`, `extern`, `typedef`, `struct`, `union`,
  `enum` keywords.
- Compound expressions beyond a single binary operator pair.
- Pointer arithmetic outside `p[K]` form.
- Initializer lists with sub-aggregates (only flat lists).
