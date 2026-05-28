use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    prelude::choice,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{ConstDecl, Directive, EnumDecl, Item, Machine, RecordDecl},
        token::Token,
    },
    parser::{
        const_item::const_item, directive::directive, enum_decl::enum_decl,
        helpers::TokenInput, import::import, machine::machine, record::record,
    },
};

mod branch;
mod const_item;
mod directive;
mod enum_decl;
mod expr;
mod helpers;
mod import;
mod machine;
mod pattern;
mod record;

/// Top-level parser.  Returns a flat list of `Item`s: imports, standalone
/// directives, and machines.
///
/// Directives are parsed as standalone items because they may be separated
/// from the machine they annotate by imports (e.g. in `io.lake`).
/// Attaching directives to machines is left to a later semantic pass.
pub fn program<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Vec<Spanned<Item<'src>>>, Err<Rich<'t, Token<'src>>>> {
    let record_item = record().map(|r: Spanned<RecordDecl>| {
        let span = r.span;
        Item::Record(r).with_span(span)
    });

    // Enum declarations share the `IDENT is { ... }` shape with records but
    // insert an `enum` keyword between `is` and the body.  Tried BEFORE
    // `record_item` in the choice so chumsky doesn't eat `enum` as a
    // record field name.
    let enum_item = enum_decl().map(|e: Spanned<EnumDecl>| {
        let span = e.span;
        Item::Enum(e).with_span(span)
    });

    let machine_item = machine().map(|m: Spanned<Machine>| {
        let span = m.span;
        Item::Machine(m).with_span(span)
    });

    let directive_item = directive().map(|d: Spanned<Directive>| {
        let span = d.span;
        Item::Directive(d).with_span(span)
    });

    let import_item = import().map(|i| {
        let span = i.span;
        Item::Import(i).with_span(span)
    });

    let const_item_node = const_item().map(|c: Spanned<ConstDecl>| {
        let span = c.span;
        Item::Const(c).with_span(span)
    });

    choice((import_item, directive_item, const_item_node, enum_item, record_item, machine_item))
        .repeated()
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use crate::{
        api::{
            ast::{Directive, Ident, Item, Pattern, PatternKind, Type},
            expr::Expr,
        },
        lexer::lexer,
        parser::pattern::pattern,
    };

    fn parse_pattern(src: &str) -> Result<chumsky::span::Spanned<Pattern<'_>>, String> {
        let tokens = lexer()
            .parse(src)
            .into_result()
            .map_err(|e| format!("lex error: {e:?}"))?;
        pattern()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .map_err(|e| format!("parse error: {e:?}"))
    }

    fn parse_directive(src: &str) -> Result<chumsky::span::Spanned<Directive<'_>>, String> {
        let tokens = lexer()
            .parse(src)
            .into_result()
            .map_err(|e| format!("lex error: {e:?}"))?;
        super::directive()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .map_err(|e| format!("parse error: {e:?}"))
    }

    #[test]
    fn parse_simple_pattern() {
        let result = parse_pattern("n i32").expect("should parse");
        assert_eq!(result.inner.ident.inner, Ident::new("n"));
        assert!(matches!(result.inner.ty.inner, Type::Named(_)));
    }

    #[test]
    fn parse_wildcard_pattern() {
        let result = parse_pattern("_").expect("should parse");
        assert!(result.inner.is_wildcard());
    }

    #[test]
    fn parse_generic_type_pattern() {
        let result = parse_pattern("ptr box(T)").expect("should parse");
        assert_eq!(result.inner.ident.inner, Ident::new("ptr"));
        assert!(matches!(result.inner.ty.inner, Type::Generic(_, _)));
    }

    #[test]
    fn parse_numeric_literal_guard() {
        let result = parse_pattern("0 i64").expect("should parse");
        assert!(result.inner.is_literal_guard());
        assert_eq!(result.inner.guard_i64(), Some(0));
    }

    #[test]
    fn parse_string_literal_guard() {
        let result = parse_pattern("\"hello\" str").expect("should parse");
        assert!(result.inner.is_string_guard());
        assert_eq!(result.inner.guard_str(), Some("hello"));
    }

    #[test]
    fn directive_no_args() {
        // `@rt_st` without parens — args must be empty
        let result = parse_directive("@rt_st").expect("should parse");
        assert_eq!(result.inner.name.inner, Ident::new("rt_st"));
        assert!(result.inner.args.is_empty());
    }

    #[test]
    fn directive_single_named_arg() {
        // `@rt_st(raw_box)` — one Named type arg
        let result = parse_directive("@rt_st(raw_box)").expect("should parse");
        assert_eq!(result.inner.name.inner, Ident::new("rt_st"));
        assert_eq!(result.inner.args.len(), 1);
        assert!(matches!(result.inner.args[0].inner, Type::Named(_)));
        let Type::Named(ref ident) = result.inner.args[0].inner else {
            panic!()
        };
        assert_eq!(ident.inner, Ident::new("raw_box"));
    }

    #[test]
    fn directive_named_and_struct_args() {
        // `@rt(overflow {raw_box usize} {bool})` — Named + two Struct args
        let result = parse_directive("@rt(overflow {raw_box usize} {bool})").expect("should parse");
        assert_eq!(result.inner.name.inner, Ident::new("rt"));
        assert_eq!(result.inner.args.len(), 3);
        assert!(matches!(result.inner.args[0].inner, Type::Named(_)));
        let Type::Struct(ref fields) = result.inner.args[1].inner else {
            panic!()
        };
        assert_eq!(fields.len(), 2);
        assert!(matches!(fields[0].inner, Type::Named(_)));
        assert!(matches!(fields[1].inner, Type::Named(_)));
        assert!(matches!(result.inner.args[2].inner, Type::Struct(_)));
    }

    #[test]
    fn directive_ffi_full() {
        // `@ffi(syscall { i64 i64 i64 i64 i64 i64 i64 } { i64 })`
        let result = parse_directive("@ffi(syscall { i64 i64 i64 i64 i64 i64 i64 } { i64 })")
            .expect("should parse");
        assert_eq!(result.inner.name.inner, Ident::new("ffi"));
        assert_eq!(result.inner.args.len(), 3);
        // first arg: Named("syscall")
        assert!(matches!(result.inner.args[0].inner, Type::Named(_)));
        // second arg: Struct with 7 fields
        let Type::Struct(ref params) = result.inner.args[1].inner else {
            panic!()
        };
        assert_eq!(params.len(), 7);
        // third arg: Struct with 1 field
        let Type::Struct(ref ret) = result.inner.args[2].inner else {
            panic!()
        };
        assert_eq!(ret.len(), 1);
    }

    #[test]
    fn directive_len_two_struct_args() {
        // `@rt(len {raw_box} {usize})` — Named + two single-element Structs
        let result = parse_directive("@rt(len {raw_box} {usize})").expect("should parse");
        assert_eq!(result.inner.name.inner, Ident::new("rt"));
        assert_eq!(result.inner.args.len(), 3);
        let Type::Named(ref name) = result.inner.args[0].inner else {
            panic!()
        };
        assert_eq!(name.inner, Ident::new("len"));
        let Type::Struct(ref a) = result.inner.args[1].inner else {
            panic!()
        };
        assert_eq!(a.len(), 1);
        let Type::Struct(ref b) = result.inner.args[2].inner else {
            panic!()
        };
        assert_eq!(b.len(), 1);
    }

    /// Parse a full program string and pass the first body expression of the
    /// first branch to `f`.  The callback owns the lifetime of the borrow so
    /// we avoid returning a reference into a locally-owned `String`.
    fn with_branch_expr<F>(src: &str, f: F)
    where
        F: for<'a> FnOnce(Expr<'a>),
    {
        let tokens = lexer().parse(src).into_result().expect("lex error");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse error");
        let Item::Machine(m) = &ast[0].inner else {
            panic!("expected Machine")
        };
        let crate::api::ast::MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!("expected Branch")
        };
        f(b.body[0].inner.clone());
    }

    #[test]
    fn import_with_as_alias_attaches_to_leaf() {
        use crate::api::ast::Item;
        let src = "+core.io.writer as wr";
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        let Item::Import(import_rc) = &ast[0].inner else {
            panic!("expected Import, got {:?}", ast[0].inner)
        };
        // Walk to leaf and assert alias.
        let mut current = import_rc.clone();
        loop {
            let next_opt = match &*current.inner.borrow() {
                crate::api::ast::Import::Import(_, next, _) => next.clone(),
                _ => panic!(),
            };
            match next_opt {
                Some(n) => current = n,
                None => break,
            }
        }
        let leaf = current.inner.borrow();
        let crate::api::ast::Import::Import(ident, _, alias) = &*leaf else {
            panic!()
        };
        assert_eq!(ident.inner, Ident::new("writer"));
        assert_eq!(alias.as_ref().map(|a| a.inner.clone()), Some(Ident::new("wr")));
    }

    #[test]
    fn import_block_per_entry_aliases() {
        use crate::api::ast::Item;
        let src = "+core.{ box.box as B  result.result as R }";
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        let Item::Import(import_rc) = &ast[0].inner else { panic!() };
        // Top: core.{ ... } — `core` chains into MultiImport
        let top = import_rc.inner.borrow();
        let crate::api::ast::Import::Import(_, Some(next), _) = &*top else { panic!() };
        let multi = next.inner.borrow();
        let crate::api::ast::Import::MultiImport(entries) = &*multi else { panic!() };
        assert_eq!(entries.len(), 2);
        // Each entry's leaf has alias B / R.
        for (entry, expected_alias) in entries.iter().zip(["B", "R"]) {
            let mut current = entry.clone();
            loop {
                let next_opt = match &*current.inner.borrow() {
                    crate::api::ast::Import::Import(_, next, _) => next.clone(),
                    _ => panic!(),
                };
                match next_opt {
                    Some(n) => current = n,
                    None => break,
                }
            }
            let leaf = current.inner.borrow();
            let crate::api::ast::Import::Import(_, _, alias) = &*leaf else { panic!() };
            assert_eq!(
                alias.as_ref().map(|a| a.inner.0),
                Some(expected_alias),
                "entry {expected_alias}"
            );
        }
    }

    #[test]
    fn import_without_alias_leaves_alias_none() {
        use crate::api::ast::Item;
        let src = "+core.io.writer";
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        let Item::Import(import_rc) = &ast[0].inner else { panic!() };
        let mut current = import_rc.clone();
        loop {
            let next_opt = match &*current.inner.borrow() {
                crate::api::ast::Import::Import(_, next, _) => next.clone(),
                _ => panic!(),
            };
            match next_opt {
                Some(n) => current = n,
                None => break,
            }
        }
        let leaf = current.inner.borrow();
        let crate::api::ast::Import::Import(_, _, alias) = &*leaf else { panic!() };
        assert!(alias.is_none());
    }

    #[test]
    fn module_path_parses_as_path_expr() {
        with_branch_expr("m is { _ -> { core:io:writer } }", |expr| {
            let Expr::Path(segments) = expr else {
                panic!("expected Path, got {expr:?}")
            };
            assert_eq!(segments.len(), 3);
            assert_eq!(segments[0].inner, Ident::new("core"));
            assert_eq!(segments[1].inner, Ident::new("io"));
            assert_eq!(segments[2].inner, Ident::new("writer"));
        });
    }

    #[test]
    fn module_path_call_wraps_in_jump() {
        with_branch_expr("m is { _ -> { core:io:writer(42) } }", |expr| {
            let Expr::Jump { ident, args } = expr else {
                panic!("expected Jump, got {expr:?}")
            };
            assert!(matches!(ident.inner, Expr::Path(_)));
            assert_eq!(args.len(), 1);
        });
    }

    #[test]
    fn single_ident_is_var_not_path() {
        // No colons — bare ident should still be Var, not a single-segment Path.
        with_branch_expr("m is { _ -> { writer } }", |expr| {
            assert!(matches!(expr, Expr::Var("writer", _)));
        });
    }

    #[test]
    fn self_parses_as_var() {
        with_branch_expr("m is { _ -> { self } }", |expr| {
            assert!(matches!(expr, Expr::Var("self", _)));
        });
    }

    #[test]
    fn self_call_parses_as_jump() {
        // `self(n)` → Jump { ident: Var("self"), args: [Var("n")] }
        with_branch_expr("m is { _ -> { self(n) } }", |expr| {
            let Expr::Jump { ident, args } = expr else {
                panic!("expected Jump, got {expr:?}")
            };
            assert!(matches!(ident.inner, Expr::Var("self", _)));
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].inner, Expr::Var("n", _)));
        });
    }

    #[test]
    fn self_call_multi_args() {
        with_branch_expr("m is { _ -> { self(a b) } }", |expr| {
            let Expr::Jump { args, .. } = expr else {
                panic!("expected Jump")
            };
            assert_eq!(args.len(), 2);
        });
    }

    #[test]
    fn wait_with_labeled_handler() {
        with_branch_expr(
            "m is { _ -> { wait { @ping n i64 -> { self(n) } } } }",
            |expr| {
                let Expr::Wait { handlers, .. } = expr else {
                    panic!("expected Wait, got {expr:?}")
                };
                assert_eq!(handlers.len(), 1);
                assert_eq!(
                    handlers[0].inner.label.as_ref().map(|l| l.inner.clone()),
                    Some(Ident::new("ping"))
                );
                assert_eq!(handlers[0].inner.patterns.len(), 1);
                assert_eq!(handlers[0].inner.body.len(), 1);
            },
        );
    }

    #[test]
    fn wait_with_multiple_handlers() {
        with_branch_expr(
            "m is { _ -> { wait { @inc n i64 -> { n } @get _ -> { self() } } } }",
            |expr| {
                let Expr::Wait { handlers, .. } = expr else {
                    panic!("expected Wait, got {expr:?}")
                };
                assert_eq!(handlers.len(), 2);
                assert_eq!(
                    handlers[0].inner.label.as_ref().map(|l| l.inner.clone()),
                    Some(Ident::new("inc"))
                );
                assert_eq!(
                    handlers[1].inner.label.as_ref().map(|l| l.inner.clone()),
                    Some(Ident::new("get"))
                );
            },
        );
    }

    #[test]
    fn wait_handler_without_label() {
        with_branch_expr("m is { _ -> { wait { n i64 -> { n } } } }", |expr| {
            let Expr::Wait { handlers, .. } = expr else {
                panic!("expected Wait, got {expr:?}")
            };
            assert_eq!(handlers.len(), 1);
            assert!(handlers[0].inner.label.is_none());
        });
    }

    #[test]
    fn let_with_type_and_expr() {
        with_branch_expr("m is { _ -> { let x i64 = 42 } }", |expr| {
            let Expr::Let { ident, ty, default } = expr else {
                panic!("expected Let, got {expr:?}")
            };
            assert_eq!(ident.inner, Ident::new("x"));
            assert!(matches!(ty.inner, Type::Named(_)));
            assert!(default.is_some());
            assert!(matches!(
                default.expect("expected default").inner,
                Expr::Num(_, _)
            ));
        });
    }

    #[test]
    fn let_without_type() {
        with_branch_expr("m is { _ -> { let x = 42 } }", |expr| {
            let Expr::Let { ident, ty, default } = expr else {
                panic!("expected Let, got {expr:?}")
            };
            assert_eq!(ident.inner, Ident::new("x"));
            // Missing annotation now parses as `Type::Unknown` (was `Unit`)
            // so the resolver can fill it in from the RHS.
            assert!(matches!(ty.inner, Type::Unknown));
            assert!(default.is_some());
        });
    }

    #[test]
    fn let_with_call_expr() {
        with_branch_expr("m is { _ -> { let x = foo(42) } }", |expr| {
            let Expr::Let { default, .. } = expr else {
                panic!("expected Let, got {expr:?}")
            };
            assert!(matches!(
                default.expect("expected default").inner,
                Expr::Jump { .. }
            ));
        });
    }

    #[test]
    fn let_with_tuple_literal() {
        // `{ 1 "alice" }` in value position is a tuple, not a block.
        with_branch_expr(r#"m is { _ -> { let p = { 1 "alice" } } }"#, |expr| {
            let Expr::Let { default, .. } = expr else {
                panic!("expected Let, got {expr:?}")
            };
            let inner = default.expect("expected default").inner;
            let Expr::Tuple(elems) = inner else {
                panic!("expected Tuple, got {inner:?}")
            };
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].inner, Expr::Num("1", _)));
            assert!(matches!(elems[1].inner, Expr::String("alice", _)));
        });
    }

    #[test]
    fn let_with_atom_literal() {
        with_branch_expr("m is { _ -> { let r = :ok } }", |expr| {
            let Expr::Let { default, .. } = expr else {
                panic!("expected Let, got {expr:?}")
            };
            assert!(matches!(
                default.expect("expected default").inner,
                Expr::Atom("ok")
            ));
        });
    }

    #[test]
    fn let_destructure_two_fields() {
        with_branch_expr(
            r#"m is { _ -> { let { a b } = { 1 2 } } }"#,
            |expr| {
                let Expr::LetTuple { fields, default } = expr else {
                    panic!("expected LetTuple, got {expr:?}")
                };
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].inner.ident.inner, Ident::new("a"));
                assert_eq!(fields[1].inner.ident.inner, Ident::new("b"));
                assert!(matches!(default.inner, Expr::Tuple(_)));
            },
        );
    }

    #[test]
    fn tuple_index_access() {
        with_branch_expr("m is { _ -> { let x = pair.0 } }", |expr| {
            let Expr::Let { default, .. } = expr else {
                panic!("expected Let, got {expr:?}")
            };
            let inner = default.expect("expected default").inner;
            let Expr::TupleIndex { index, receiver } = inner else {
                panic!("expected TupleIndex, got {inner:?}")
            };
            assert_eq!(index, 0);
            assert!(matches!(receiver.inner, Expr::Var("pair", _)));
        });
    }

    #[test]
    fn parse_const_int() {
        use crate::api::ast::Item;
        let src = "const N = 42";
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        let Item::Const(c) = &ast[0].inner else {
            panic!("expected Const, got {:?}", ast[0].inner)
        };
        assert!(!c.inner.vis);
        assert_eq!(c.inner.ident.inner, Ident::new("N"));
        assert!(matches!(c.inner.value.inner, Expr::Num("42", _)));
    }

    #[test]
    fn parse_pub_const_atom() {
        use crate::api::ast::Item;
        let src = "pub const STATUS = :ok";
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        let Item::Const(c) = &ast[0].inner else {
            panic!("expected Const, got {:?}", ast[0].inner)
        };
        assert!(c.inner.vis);
        assert!(matches!(c.inner.value.inner, Expr::Atom("ok")));
    }

    #[test]
    fn parse_const_string() {
        use crate::api::ast::Item;
        let src = r#"const HELLO = "world""#;
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        let Item::Const(c) = &ast[0].inner else {
            panic!("expected Const, got {:?}", ast[0].inner)
        };
        assert!(matches!(c.inner.value.inner, Expr::String("world", _)));
    }

    #[test]
    fn const_rejects_non_literal_rhs() {
        let src = "const N = 1 + 2";
        let tokens = lexer().parse(src).into_result().expect("lex");
        let result = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result();
        // Either an outright parse error (preferred), or the literal_rhs
        // matched only `1` and the rest spilled — both surfaces let the
        // user know the RHS isn't allowed.  Verify the AST is not the
        // happy `Const { value: Add(..) }` shape.
        if let Ok(ast) = result {
            if let Some(item) = ast.first() {
                if let crate::api::ast::Item::Const(c) = &item.inner {
                    let bad = matches!(
                        c.inner.value.inner,
                        Expr::Add(..) | Expr::Sub(..) | Expr::Mul(..) | Expr::Div(..)
                    );
                    assert!(!bad, "non-literal const RHS should not parse as compute");
                }
            }
        }
    }

    #[test]
    fn when_arm_pattern_tuple_atom_dispatch() {
        // Feature #055 — `when r { { :ok v } -> body }` parses with
        // an `Expr::Tuple` in the arm-key position; the lowering pass
        // rewrites it into nested atom dispatch + binding lets.
        with_branch_expr(
            r#"m is { _ -> { when r { { :ok v } -> { v } } } }"#,
            |expr| {
                let Expr::When { branches, .. } = expr else {
                    panic!("expected When, got {expr:?}")
                };
                assert_eq!(branches.len(), 1);
                let (key, _body) = &branches[0];
                let Expr::Tuple(elems) = &key.inner else {
                    panic!("expected Tuple arm pattern, got {:?}", key.inner);
                };
                assert_eq!(elems.len(), 2);
                assert!(matches!(elems[0].inner, Expr::Atom("ok")));
                assert!(matches!(elems[1].inner, Expr::Var("v", _)));
            },
        );
    }

    #[test]
    fn let_with_tagged_tuple() {
        // Erlang-flavoured tagged tuple: first element is an atom, rest values.
        with_branch_expr("m is { _ -> { let r = { :ok 42 } } }", |expr| {
            let Expr::Let { default, .. } = expr else {
                panic!("expected Let, got {expr:?}")
            };
            let inner = default.expect("expected default").inner;
            let Expr::Tuple(elems) = inner else {
                panic!("expected Tuple, got {inner:?}")
            };
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].inner, Expr::Atom("ok")));
            assert!(matches!(elems[1].inner, Expr::Num("42", _)));
        });
    }

    // ── expr parser helpers ─────────────────────────────────────────────────

    fn try_parse_one_expr(src: &str) -> Result<chumsky::span::Spanned<Expr<'_>>, String> {
        let tokens = lexer()
            .parse(src)
            .into_result()
            .map_err(|e| format!("lex error: {e:?}"))?;
        crate::parser::expr::expr()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .map_err(|e| format!("parse error: {e:?}"))
    }

    fn parse_one_expr(src: &str) -> chumsky::span::Spanned<Expr<'_>> {
        try_parse_one_expr(src).expect("expected successful parse")
    }

    // ── record literal tests ────────────────────────────────────────────────

    #[test]
    fn parses_record_literal() {
        let src = r#"Request { method = "GET"  path = "/" }"#;
        let parsed = parse_one_expr(src);
        let Expr::RecordLiteral { name, fields } = parsed.inner else {
            panic!("not a literal: {:?}", parsed)
        };
        assert_eq!(name.inner.0, "Request");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0.inner.0, "method");
    }

    #[test]
    fn record_literal_does_not_collide_with_jump() {
        // `Foo(args)` is a Jump, NOT a literal — confirm
        let src = "Foo(1 2)";
        let parsed = parse_one_expr(src);
        assert!(matches!(parsed.inner, Expr::Jump { .. }));
    }

    #[test]
    fn record_literal_does_not_collide_with_bare_var() {
        let src = "Foo";
        let parsed = parse_one_expr(src);
        assert!(matches!(parsed.inner, Expr::Var(_, _)));
    }

    #[test]
    fn rejects_empty_record_literal() {
        let src = "Empty { }";
        // Should EITHER not parse as a literal OR error out.
        let parsed_or_err = try_parse_one_expr(src);
        match parsed_or_err {
            Ok(p) if matches!(p.inner, Expr::RecordLiteral { .. }) => {
                panic!("empty literal must not parse as RecordLiteral");
            }
            _ => {}
        }
    }

    // ── record parser helpers ────────────────────────────────────────────────

    fn try_parse_one_item(src: &str) -> Result<chumsky::span::Spanned<Item<'_>>, String> {
        let tokens = lexer()
            .parse(src)
            .into_result()
            .map_err(|e| format!("lex error: {e:?}"))?;
        let mut items = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .map_err(|e| format!("parse error: {e:?}"))?;
        if items.is_empty() {
            return Err("no items parsed".to_string());
        }
        Ok(items.remove(0))
    }

    fn parse_one_item(src: &str) -> chumsky::span::Spanned<Item<'_>> {
        try_parse_one_item(src).expect("expected successful parse")
    }

    // ── record parser tests ──────────────────────────────────────────────────

    #[test]
    fn parses_record_declaration() {
        let src = "Request is { method buf  path buf  body buf }";
        let parsed = parse_one_item(src);
        let Item::Record(rec) = &parsed.inner else { panic!("not a record: {parsed:?}") };
        assert_eq!(rec.inner.ident.inner.0, "Request");
        assert_eq!(rec.inner.fields.len(), 3);
        assert_eq!(rec.inner.fields[0].inner.name.inner.0, "method");
    }

    #[test]
    fn rejects_mixed_record_machine_body() {
        // A body that mixes field-only items with a branch `->` must not parse
        // as a record.  It may parse as a machine or fail entirely — both OK.
        let src = "Bad is { x i64  y i64 -> ret i64 { ret x } }";
        let parsed_or_err = try_parse_one_item(src);
        match parsed_or_err {
            Ok(chumsky::span::Spanned { inner: Item::Record(_), .. }) => {
                panic!("mixed body must NOT parse as a record");
            }
            _ => {}
        }
    }

    #[test]
    fn rejects_empty_record_body() {
        // `at_least(1)` on fields means an empty `{ }` must NOT be a record.
        let src = "Empty is { }";
        let parsed_or_err = try_parse_one_item(src);
        match parsed_or_err {
            Ok(chumsky::span::Spanned { inner: Item::Record(_), .. }) => {
                panic!("empty body must NOT parse as a record");
            }
            _ => {}
        }
    }

    #[test]
    fn machine_still_parses_after_record_added() {
        let src = "echo is { x i64 -> ret i64 { ret x } }";
        let parsed = parse_one_item(src);
        assert!(matches!(&parsed.inner, Item::Machine(_)));
    }

    // ── #142 generics phase 1 — parser/AST tests ────────────────────────────
    // agent: generics-142 — see docs/state/features/142_generics.md

    #[test]
    fn parses_generic_record_single_param() {
        let src = "Vec[T] is { data buf  len i64 }";
        let parsed = parse_one_item(src);
        let Item::Record(rec) = &parsed.inner else {
            panic!("not a record: {parsed:?}")
        };
        assert_eq!(rec.inner.ident.inner.0, "Vec");
        assert_eq!(rec.inner.type_params.len(), 1);
        assert_eq!(rec.inner.type_params[0].inner.0, "T");
        assert_eq!(rec.inner.fields.len(), 2);
        assert_eq!(rec.inner.fields[0].inner.name.inner.0, "data");
    }

    #[test]
    fn parses_generic_record_two_params() {
        let src = "Map[K V] is { keys buf  values buf }";
        let parsed = parse_one_item(src);
        let Item::Record(rec) = &parsed.inner else {
            panic!("not a record: {parsed:?}")
        };
        assert_eq!(rec.inner.ident.inner.0, "Map");
        assert_eq!(rec.inner.type_params.len(), 2);
        assert_eq!(rec.inner.type_params[0].inner.0, "K");
        assert_eq!(rec.inner.type_params[1].inner.0, "V");
    }

    #[test]
    fn non_generic_record_has_empty_type_params() {
        let src = "Request is { method buf  path buf }";
        let parsed = parse_one_item(src);
        let Item::Record(rec) = &parsed.inner else { panic!("not a record") };
        assert!(rec.inner.type_params.is_empty());
    }

    #[test]
    fn parses_generic_machine_with_brackets() {
        // `id[T] is { x T -> ret T { ret x } }` — the `T` in param and
        // return position parses as Type::Named at this stage (resolver
        // rewrites to Type::TypeVar later, exercised in resolver tests).
        let src = "id[T] is { x T -> ret T { ret x } }";
        let parsed = parse_one_item(src);
        let Item::Machine(m) = &parsed.inner else {
            panic!("not a machine: {parsed:?}")
        };
        assert_eq!(m.inner.ident.inner.0, "id");
        assert_eq!(m.inner.generics.len(), 1);
        assert_eq!(m.inner.generics[0].inner.0, "T");
        // First branch pattern's type is `T` — Type::Named pre-resolve.
        let crate::api::ast::MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!("expected branch")
        };
        let pat_ty = &b.patterns[0].inner.ty.inner;
        assert!(matches!(pat_ty, Type::Named(i) if i.inner.0 == "T"));
        let ret_ty = b.ret_ty.as_ref().expect("ret_ty");
        assert!(matches!(&ret_ty.inner, Type::Named(i) if i.inner.0 == "T"));
    }

    #[test]
    fn parses_named_generic_use_site() {
        // Use-site `Vec[i64]` in a let-binding type position becomes
        // `Type::NamedGeneric { name: "Vec", args: [Named("i64")] }`.
        let src = "m is { _ -> { let v Vec[i64] = 0 } }";
        let parsed = parse_one_item(src);
        let Item::Machine(m) = &parsed.inner else { panic!("not a machine") };
        let crate::api::ast::MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!("expected branch")
        };
        let Expr::Let { ty, .. } = &b.body[0].inner else {
            panic!("expected let, got {:?}", b.body[0].inner)
        };
        let Type::NamedGeneric { name, args } = &ty.inner else {
            panic!("expected NamedGeneric, got {:?}", ty.inner)
        };
        assert_eq!(name.inner.0, "Vec");
        assert_eq!(args.len(), 1);
        assert!(matches!(&args[0].inner, Type::Named(i) if i.inner.0 == "i64"));
    }

    #[test]
    fn non_generic_machine_has_empty_type_params() {
        let src = "echo is { x i64 -> ret i64 { ret x } }";
        let parsed = parse_one_item(src);
        let Item::Machine(m) = &parsed.inner else { panic!("not a machine") };
        assert!(m.inner.generics.is_empty());
    }

    #[test]
    fn resolver_rewrites_typevar_in_generic_machine() {
        // Through the full resolver pipeline: `id<T>` should produce
        // pattern + ret_ty types of Type::TypeVar(T), not Type::Named.
        use chumsky::input::Input;
        let src = "id[T] is { x T -> ret T { ret x } }";
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = super::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        let resolved = crate::resolver::resolve(ast);
        let Item::Machine(m) = &resolved[0].inner else { panic!() };
        let crate::api::ast::MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!()
        };
        let pat_ty = &b.patterns[0].inner.ty.inner;
        assert!(
            matches!(pat_ty, Type::TypeVar(i) if i.inner.0 == "T"),
            "expected TypeVar(T) in pattern, got {:?}",
            pat_ty
        );
        let ret_ty = b.ret_ty.as_ref().expect("ret_ty").inner.clone();
        assert!(
            matches!(&ret_ty, Type::TypeVar(i) if i.inner.0 == "T"),
            "expected TypeVar(T) in ret_ty, got {:?}",
            ret_ty
        );
    }

    // ── tuple pattern helpers ────────────────────────────────────────────────

    fn parse_one_pattern(src: &str) -> chumsky::span::Spanned<Pattern<'_>> {
        parse_pattern(src).expect("expected successful parse")
    }

    // ── tuple pattern tests (#055) ───────────────────────────────────────────

    #[test]
    fn parses_arity_2_tuple_pattern() {
        let src = "{ a b }";
        let p = parse_one_pattern(src);
        let PatternKind::Tuple(elems) = &p.inner.kind else {
            panic!("not a tuple: {:?}", p)
        };
        assert_eq!(elems.len(), 2);
    }

    #[test]
    fn parses_wildcard_in_tuple_pattern() {
        let src = "{ _ x _ }";
        let p = parse_one_pattern(src);
        let PatternKind::Tuple(elems) = &p.inner.kind else {
            panic!()
        };
        assert_eq!(elems.len(), 3);
        assert!(matches!(elems[0].inner.kind, PatternKind::Wildcard));
        assert!(matches!(elems[1].inner.kind, PatternKind::Var));
        assert!(matches!(elems[2].inner.kind, PatternKind::Wildcard));
    }

    #[test]
    fn parses_nested_tuple_pattern() {
        let src = "{ a { b c } _ }";
        let p = parse_one_pattern(src);
        let PatternKind::Tuple(outer) = &p.inner.kind else {
            panic!()
        };
        assert_eq!(outer.len(), 3);
        assert!(matches!(outer[1].inner.kind, PatternKind::Tuple(_)));
    }

    #[test]
    fn parses_atom_in_tuple_pattern() {
        let src = "{ :ok x }";
        let p = parse_one_pattern(src);
        let PatternKind::Tuple(elems) = &p.inner.kind else {
            panic!()
        };
        assert!(!matches!(
            elems[0].inner.kind,
            PatternKind::Var | PatternKind::Wildcard
        ));
        assert!(matches!(elems[1].inner.kind, PatternKind::Var));
    }

    // ── char & byte-string literal tests ────────────────────────────────────

    #[test]
    fn char_literal_parses_to_i64_num() {
        let parsed = parse_one_expr("'A'");
        let Expr::Num(s, ty) = parsed.inner else {
            panic!("expected Num, got {:?}", parsed.inner);
        };
        assert_eq!(s, "65");
        let Type::Named(ref name) = ty else {
            panic!("expected Named i64");
        };
        assert_eq!(name.inner.0, "i64");
    }

    #[test]
    fn char_literal_newline_parses_to_10() {
        let parsed = parse_one_expr("'\\n'");
        let Expr::Num(s, _) = parsed.inner else {
            panic!("expected Num");
        };
        assert_eq!(s, "10");
    }

    #[test]
    fn char_literal_hex_ff_parses_to_255() {
        let parsed = parse_one_expr("'\\xff'");
        let Expr::Num(s, _) = parsed.inner else {
            panic!("expected Num");
        };
        assert_eq!(s, "255");
    }

    #[test]
    fn char_literal_escaped_single_quote() {
        let parsed = parse_one_expr("'\\''");
        let Expr::Num(s, _) = parsed.inner else {
            panic!("expected Num");
        };
        assert_eq!(s, "39");
    }

    #[test]
    fn unterminated_char_literal_errors() {
        let res = try_parse_one_expr("'A");
        assert!(res.is_err(), "unterminated char should fail without panic");
    }

    #[test]
    fn byte_string_literal_parses_to_buf_string() {
        let parsed = parse_one_expr("b\"abc\"");
        let Expr::String(s, ty) = parsed.inner else {
            panic!("expected String variant, got {:?}", parsed.inner);
        };
        assert_eq!(s, "abc");
        let Type::Named(ref name) = ty else {
            panic!("expected Named buf");
        };
        assert_eq!(name.inner.0, "buf");
    }

    #[test]
    fn byte_string_empty_parses() {
        let parsed = parse_one_expr("b\"\"");
        let Expr::String(s, _) = parsed.inner else {
            panic!("expected String");
        };
        assert_eq!(s, "");
    }

    #[test]
    fn byte_string_escapes_are_raw_until_codegen() {
        // Raw slice carries un-decoded escapes (matches `Token::String`
        // handling).  Backend's `unescape` resolves them at codegen.
        let parsed = parse_one_expr("b\"\\r\\n\"");
        let Expr::String(s, _) = parsed.inner else {
            panic!("expected String");
        };
        assert_eq!(s, "\\r\\n");
    }

    #[test]
    fn byte_string_with_embedded_dquote_escape() {
        let parsed = parse_one_expr("b\"a\\\"b\"");
        let Expr::String(s, _) = parsed.inner else {
            panic!("expected String");
        };
        assert_eq!(s, "a\\\"b");
    }
}
