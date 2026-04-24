use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    prelude::choice,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{Directive, Machine},
        expr::Expr,
        token::Token,
    },
    parser::{directive::directive, helpers::TokenInput, import::import, machine::machine},
};

mod branch;
mod directive;
mod expr;
mod helpers;
mod import;
mod machine;
mod pattern;

/// Top-level parser.  Returns a flat list of items: imports, standalone
/// directives (`Expr::Directive`), and machines (`Expr::Machine`).
///
/// Directives are parsed as standalone items because they may be separated
/// from the machine they annotate by imports (e.g. in `io.lake`).
/// Attaching directives to machines is left to a later semantic pass.
pub fn program<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Vec<Spanned<Expr<'src>>>, Err<Rich<'t, Token<'src>>>> {
    let machine_item = machine().map(|m: Spanned<Machine>| {
        let span = m.span;
        Expr::Machine(m).with_span(span)
    });

    let directive_item = directive().map(|d: Spanned<Directive>| {
        let span = d.span;
        Expr::Directive(d).with_span(span)
    });

    choice((import(), directive_item, machine_item))
        .repeated()
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use crate::{
        api::{
            ast::{Directive, Ident, Pattern, Type},
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
        let Expr::Machine(m) = &ast[0].inner else {
            panic!("expected Machine")
        };
        let crate::api::ast::MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!("expected Branch")
        };
        f(b.body[0].inner.clone());
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
                let Expr::Wait { handlers } = expr else {
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
                let Expr::Wait { handlers } = expr else {
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
            let Expr::Wait { handlers } = expr else {
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
            assert!(matches!(ty.inner, Type::Unit));
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
}
