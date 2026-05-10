use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    prelude::choice,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{ConstDecl, Directive, Item, Machine},
        token::Token,
    },
    parser::{
        const_item::const_item, directive::directive, helpers::TokenInput, import::import,
        machine::machine,
    },
};

mod branch;
mod const_item;
mod directive;
mod expr;
mod helpers;
mod import;
mod machine;
mod pattern;

/// Top-level parser.  Returns a flat list of `Item`s: imports, standalone
/// directives, and machines.
///
/// Directives are parsed as standalone items because they may be separated
/// from the machine they annotate by imports (e.g. in `io.lake`).
/// Attaching directives to machines is left to a later semantic pass.
pub fn program<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Vec<Spanned<Item<'src>>>, Err<Rich<'t, Token<'src>>>> {
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

    choice((import_item, directive_item, const_item_node, machine_item))
        .repeated()
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use crate::{
        api::{
            ast::{Directive, Ident, Item, Pattern, Type},
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
                assert_eq!(fields[0].inner, Ident::new("a"));
                assert_eq!(fields[1].inner, Ident::new("b"));
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
}
