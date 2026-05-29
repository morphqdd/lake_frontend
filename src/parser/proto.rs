//! Parsers for `proto` declarations and `X is Eq Show` impl assertions.
//!
//! See docs/state/features/146_protos.md.  Phase 1 lands AST + registry;
//! bound verification + the `[]`→`index` desugar live in the mono pass.

use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::just,
    select_ref,
    span::Spanned,
};

use crate::{
    api::{
        ast::{ImplDecl, MethodSig, ProtoDecl},
        token::Token,
    },
    parser::{
        expr::type_expr::type_expr,
        helpers::{TokenInput, ident_parser},
    },
};

/// Match an `Ident("proto")` token — `proto` is not a reserved keyword so
/// the lexer hands it back as a bare ident (mirrors `enum`).
fn proto_kw<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, (), Err<Rich<'t, Token<'src>>>> + Clone {
    select_ref!(Token::Ident(n) = _e if *n == "proto" => ())
}

/// One method signature inside a proto body: `eq is { Self Self -> bool }`.
/// The body is a brace group containing param types, optional `-> ret`.
fn method_sig<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<MethodSig<'src>>, Err<Rich<'t, Token<'src>>>> {
    // Inside the braces: `Type+ ( -> Type )?`.
    let body = type_expr()
        .repeated()
        .collect::<Vec<_>>()
        .then(just(Token::Arrow).ignore_then(type_expr()).or_not());

    ident_parser()
        .then_ignore(just(Token::Is))
        .then(
            body.nested_in(
                select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
            ),
        )
        .map(|(name, (params, ret))| MethodSig { name, params, ret })
        .spanned()
}

/// Parses `IDENT 'is' 'proto' ('+' IDENT)* '{' method_sig* '}'`.
///
/// Must be tried BEFORE `record()` / `enum()` / `machine()` in the
/// top-level `choice` — all share the `IDENT 'is' '{'` prefix.
pub fn proto_decl<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<ProtoDecl<'src>>, Err<Rich<'t, Token<'src>>>> {
    let parents = just(Token::Plus)
        .ignore_then(ident_parser())
        .repeated()
        .collect::<Vec<_>>();

    ident_parser()
        .then_ignore(just(Token::Is))
        .then_ignore(proto_kw())
        .then(parents)
        .then(
            method_sig()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|((name, parents), methods)| ProtoDecl {
            name,
            parents,
            methods,
        })
        .spanned()
}

/// Parses `IDENT 'is' IDENT+` — an explicit proto-implementation
/// assertion (`HTTPMethod is Eq Show`).  Distinguished from a record /
/// enum / machine by the *absence* of a brace / `enum` / `proto` after
/// `is`: the body is a bare list of ≥1 proto names.
///
/// Tried AFTER record / enum / proto / machine in the top-level choice so
/// those brace-bearing forms win first; this catches the remaining
/// `Name is A B ...` shape.
pub fn impl_decl<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<ImplDecl<'src>>, Err<Rich<'t, Token<'src>>>> {
    ident_parser()
        .then_ignore(just(Token::Is))
        .then(
            ident_parser()
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|(ty, protos)| ImplDecl { ty, protos })
        .spanned()
}

/// `proto` / `enum` are bare idents; `impl_decl` would happily swallow
/// `Foo is enum` as `ty=Foo protos=[enum]`.  This guard rejects an impl
/// whose first "proto" is actually the `proto`/`enum` keyword so the
/// dedicated decl parsers get the item instead.  Used via `choice`
/// ordering rather than here, but kept for clarity if reused.
#[allow(dead_code)]
fn is_reserved_decl_kw(name: &str) -> bool {
    matches!(name, "enum" | "proto")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::ast::Item, lexer::lexer};
    use chumsky::input::Input;

    fn parse_top(src: &str) -> Result<Vec<Spanned<Item<'_>>>, String> {
        let tokens = lexer()
            .parse(src)
            .into_result()
            .map_err(|e| format!("lex error: {e:?}"))?;
        crate::parser::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .map_err(|e| format!("parse error: {e:?}"))
    }

    #[test]
    fn parses_simple_proto() {
        let src = "Eq is proto { eq is { Self Self -> bool } }";
        let items = parse_top(src).expect("parses");
        assert_eq!(items.len(), 1);
        let Item::Proto(p) = &items[0].inner else {
            panic!("expected Proto, got {:?}", items[0].inner)
        };
        assert_eq!(p.inner.name.inner.0, "Eq");
        assert!(p.inner.parents.is_empty());
        assert_eq!(p.inner.methods.len(), 1);
        let m = &p.inner.methods[0].inner;
        assert_eq!(m.name.inner.0, "eq");
        assert_eq!(m.params.len(), 2);
        assert!(m.ret.is_some());
    }

    #[test]
    fn parses_proto_with_parent() {
        let src = "Ord is proto +Eq { cmp is { Self Self -> i64 } }";
        let items = parse_top(src).expect("parses");
        let Item::Proto(p) = &items[0].inner else {
            panic!("expected Proto, got {:?}", items[0].inner)
        };
        assert_eq!(p.inner.name.inner.0, "Ord");
        assert_eq!(p.inner.parents.len(), 1);
        assert_eq!(p.inner.parents[0].inner.0, "Eq");
        assert_eq!(p.inner.methods.len(), 1);
        assert_eq!(p.inner.methods[0].inner.name.inner.0, "cmp");
    }

    #[test]
    fn parses_impl_assertion() {
        let src = "HTTPMethod is Eq Show";
        let items = parse_top(src).expect("parses");
        let Item::Impl(d) = &items[0].inner else {
            panic!("expected Impl, got {:?}", items[0].inner)
        };
        assert_eq!(d.inner.ty.inner.0, "HTTPMethod");
        assert_eq!(d.inner.protos.len(), 2);
        assert_eq!(d.inner.protos[0].inner.0, "Eq");
        assert_eq!(d.inner.protos[1].inner.0, "Show");
    }
}
