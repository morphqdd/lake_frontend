# Lake Frontend

Compiler frontend for the [Lake](https://morphqdd.github.io/lake-book/) programming language.

Implements: lexing, parsing, name resolution, and type checking.

## Pipeline

```
.lake source
  → lexer     → Vec<Spanned<Token>>
  → parser    → Vec<Spanned<Expr>>
  → resolver  → Vec<Spanned<Expr>>  (types filled in)
  → typeck    → diagnostics
```

## What the Parser Handles

### Imports

```lake
+core.io.writer                              # simple
+core.{ box.box result.result error.error }  # multi-import
```

### Directives

```lake
@rt(rt_write)
@rt_st(raw_box)
@ffi(syscall { i64 i64 i64 i64 i64 i64 i64 } { i64 })
```

### Machines

```lake
pub box(T) is {             # visibility, generics
  @ok.T                     # named data field
  @.raw_box(T)              # anonymous data field
  @.{ str }                 # anonymous struct field

  @len ptr box(T) -> ret usize { ... }  # labeled branch with ret type
  n i64 -> { ... }                      # simple branch
  _ -> { ... }                          # wildcard branch
}
```

### Expressions

```lake
42                           # i64 literal
"hello\n"                    # string literal
true                         # boolean literal
{}                           # unit literal

n                            # variable
let x i64 = 42              # let binding

writer(n)                    # call
self(n-1 acc+n)             # self-transition with args
self@print_string()         # transition to a labeled branch
box@len(data_ptr)           # spawn machine at a labeled branch
point.{ 10 20 }             # struct initialization
error.{ "overflow" }        # struct initialization

2 * n + 1                   # arithmetic (*, /, +, -)
n <= 10                     # comparison (<=, >=, ==, <, >)

when 0 == n {               # conditional
  true  -> { ... }
  false -> { ... }
}

wait {                      # process suspension
  @ready _ -> { ... }
}
```

### Types

```lake
i64                          # named type
box(T)                       # generic type
{ str }                      # anonymous struct
{}                           # unit type
```

## Building

```sh
cargo build
```

## API

```rust
use lake_frontend::prelude::{parse, build_ast};

// Lex + parse only
let ast = parse(path, src)?;

// Full pipeline: lex + parse + resolve + typecheck
let (tokens, ast) = build_ast(path, src)?;
```

## Project Structure

```
src/
├── lib.rs                    # crate root
├── bin/main.rs               # CLI entry point
├── api/
│   ├── token/mod.rs          # Token enum
│   ├── ast/mod.rs            # Machine, Branch, Pattern, Field, Type, Import
│   └── expr/mod.rs           # Expr enum
├── lexer/mod.rs              # tokenization (chumsky recursive)
├── parser/
│   ├── mod.rs                # program() — top-level parser
│   ├── helpers.rs            # shared parser utilities
│   ├── import.rs             # import statements
│   ├── directive.rs          # @rt, @ffi, @rt_st
│   ├── pattern.rs            # branch parameters
│   ├── branch.rs             # branch definitions
│   ├── machine/
│   │   ├── mod.rs            # machine definitions
│   │   └── field_decl.rs     # @field.Type declarations
│   └── expr/
│       ├── mod.rs            # expressions (pratt precedence)
│       └── type_expr.rs      # type expressions
├── resolver/mod.rs           # name resolution, scope management
├── typeck/mod.rs             # type checking, signature validation
└── error_handle/mod.rs       # diagnostics via ariadne
```

## Error Codes

| Code | Stage | Description |
|------|-------|-------------|
| L001 | Lexer | Unexpected character |
| P001 | Parser | Syntax error |
| E001 | Typeck | Undeclared variable |
| E002 | Typeck | Unknown type in arithmetic |
| E003 | Typeck | No matching branch for call |

## Dependencies

| Crate | Role |
|-------|------|
| [chumsky](https://github.com/zesterer/chumsky) (git, pratt) | Parser combinators |
| [ariadne](https://crates.io/crates/ariadne) 0.6 | Error rendering |
| [thiserror](https://crates.io/crates/thiserror) 2.0 | Error derive macros |
| [chrono](https://crates.io/crates/chrono) 0.4 | Timestamps for dump files |

## See Also

- [Lake Book](https://morphqdd.github.io/lake-book/) — language documentation
- [lake-native-compiler](https://github.com/morphqdd/lake-native-compiler) — native compiler (Cranelift backend)
