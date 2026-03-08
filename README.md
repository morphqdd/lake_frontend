# Lake Frontend

Compiler frontend for the **Lake** programming language — a language built around **machines**, **branches**, and **message-passing** semantics.

Implements the full frontend pipeline: lexing, parsing, name resolution, and type checking.

## Pipeline

```
.lake source
  → lexer     → Vec<Spanned<Token>>
  → parser    → Vec<Spanned<Expr>>
  → resolver  → Vec<Spanned<Expr>>  (types filled in)
  → typeck    → diagnostics
```

All stages produce structured errors rendered via [ariadne](https://github.com/zesterer/ariadne).

## Language Overview

Lake programs are composed of **machines** — named entities that define behavior through pattern-matched **branches**.

```lake
+core.{ io.writer }

main is {
  @init _ -> {
    self@print_string()
  }

  @print_string n str."hello" -> {
    writer(n)
  }
}
```

### Imports

```lake
+core.io.writer              # simple import
+core.{ box.box result.result error.error }  # multi-import
```

### Machines

Machines are the core abstraction — they hold data fields and branches.

```lake
pub result(T) is {
  @ok.T                      # data field
  @err.error                 # data field

  @init _ -> { }             # branch
}
```

- `pub` — public visibility
- `(T)` — generic type parameters
- `@name.Type` — data field declaration
- `@label pattern+ -> { body }` — branch with label

### Branches

```lake
@label pattern+ -> [ret Type] { body }
```

- `@label` — optional label for targeted calls (`self@label(...)`)
- `pattern+` — one or more parameters: `name Type[.default]` or `_` (wildcard)
- `ret Type` — optional return type annotation
- `{ body }` — branch body with expressions

### Patterns

```lake
n i32                        # typed parameter
n str."hello"                # with default value
ptr box(T)                   # generic type
_                            # wildcard (ignored)
```

### Expressions

```lake
writer(n)                    # call (jump)
self()                       # self-recursion
self@print_string()          # labeled call (method)
box@len(data_ptr)            # method call on receiver

let x i32 = 42               # let binding

2 * n + 1                    # arithmetic (+, -, *, /)
n <= 10                      # comparison (<=, >=, ==, <, >)

point.{ 10 20 }              # struct initialization
point.x                      # field access

when overflow(buf sz) {      # pattern matching
  false -> { self(n + 1 data_ptr) }
  true  -> { error.{ "overflow" } }
}

wait {                       # process suspension
  @ready _ -> { self() }
}
```

### Directives

Compiler attributes placed before a machine definition:

```lake
@rt(rt_write)                             # runtime function binding
@rt_st(raw_box)                           # runtime state
@ffi(syscall { i64 i64 } { i64 })         # foreign function interface
```

## Building

Requires Rust (edition 2024).

```sh
cargo build
```

## Running

The binary compiles a set of example files from `examples/simple/`:

```sh
cargo run
```

On success, it writes `.laketokens` and `.lakeast` dump files next to each source file.
On error, it renders diagnostics to stderr and exits with code 1.

Example error output:

```
[E003] Error: no branch of `writer` matches the call `self(i64 box(u8) {})`
   ╭─[ examples/simple/core/io.lake:8:5 ]
   │
 8 │     self(1 data_ptr box@len(data_ptr))
   │     ──┬─
   │       ╰─── no branch accepts (i64 box(u8) {})
   │
   │ Help: add a branch whose parameter types match the argument types
───╯
```

## Project Structure

```
src/
├── lib.rs                    # crate root, lint rules
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

## API

The crate exposes two entry points through `lake_frontend::prelude`:

```rust
// Parse only (lex + parse)
fn parse(path, src) -> Result<Vec<Spanned<Expr>>, LakeErrors>

// Full pipeline (lex + parse + resolve + typecheck)
fn build_ast(path, src) -> Result<(tokens, ast), (tokens, errors)>
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
| [miette](https://crates.io/crates/miette) 7.6 | Diagnostics |
| [thiserror](https://crates.io/crates/thiserror) 2.0 | Error derive macros |
| [anyhow](https://crates.io/crates/anyhow) 1 | Error context |
| [chrono](https://crates.io/crates/chrono) 0.4 | Timestamps for dump files |

## Tests

```sh
cargo test
```

Covers lexer tokenization, resolver scoping, and type checker diagnostics.
