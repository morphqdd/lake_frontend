//! Parser for `Name is enum { variant ... }` declarations.
//!
//! See docs/state/features/145_enums.md for the full design.  Phase 1 lands
//! the AST shape and registry entry; resolver / typeck / lowering follow in
//! later phases.

use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::{choice, just},
    select_ref,
    span::Spanned,
};

use crate::{
    api::{
        ast::{EnumDecl, Ident, Type, Variant, VariantPayload},
        token::Token,
    },
    parser::{
        expr::type_expr::type_expr,
        helpers::{TokenInput, ident_parser},
    },
};

/// One named field inside a record-style variant payload: `from i64`.
fn variant_record_field<'t, 'src: 't>() -> impl Parser<
    't,
    TokenInput<'t, 'src>,
    (Spanned<Ident<'src>>, Spanned<Type<'src>>),
    Err<Rich<'t, Token<'src>>>,
> {
    ident_parser().then(type_expr())
}

/// A variant payload: `(types ...)` or `{ fields ... }`, or absent.
fn variant_payload<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, VariantPayload<'src>, Err<Rich<'t, Token<'src>>>> {
    // Tuple-style: `Post(buf)`, `Bounded(i64 i64)`.  Accept both `Parens`
    // (space before `(`) and `TightParens` (adjacent `(`).  The variant
    // name precedes the payload directly so we get `TightParens` in
    // typical source, but we accept either to stay friendly.
    let tuple = type_expr()
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .nested_in(select_ref!(
            Token::Parens(ts) = e => ts.split_spanned(e.span()),
            Token::TightParens(ts) = e => ts.split_spanned(e.span()),
        ))
        .map(VariantPayload::Tuple);

    // Record-style: `Open { from i64 }`.
    let record = variant_record_field()
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
        .map(VariantPayload::Record);

    choice((tuple, record))
}

/// One variant: `IDENT [payload]`.
fn variant<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Variant<'src>>, Err<Rich<'t, Token<'src>>>> {
    ident_parser()
        .then(variant_payload().or_not())
        .map(|(name, payload)| Variant {
            name,
            payload: payload.unwrap_or(VariantPayload::None),
        })
        .spanned()
}

/// Match an `Ident("enum")` token — `enum` is not a reserved keyword so
/// the lexer hands it back as a bare ident.
fn enum_kw<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, (), Err<Rich<'t, Token<'src>>>> + Clone {
    select_ref!(Token::Ident(n) = _e if *n == "enum" => ())
}

/// Parses `IDENT [[T ...]] 'is' 'enum' '{' variant* '}'`.
///
/// Must be tried BEFORE `record()` in the top-level `choice` — both start
/// with `IDENT 'is' '{` and `record()` would happily eat `enum { ... }`
/// as a `field type` pair.
///
/// agent: generics-142 phase 4 — the optional `[T U ...]` clause after
/// the enum name mirrors the record syntax landed in phase 1.
pub fn enum_decl<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<EnumDecl<'src>>, Err<Rich<'t, Token<'src>>>> {
    let type_param_list = ident_parser()
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .nested_in(select_ref!(
            Token::SquareBrackets(ts) = e => ts.split_spanned(e.span()),
            Token::TightSquareBrackets(ts) = e => ts.split_spanned(e.span()),
        ));

    ident_parser()
        .then(type_param_list.or_not().map(|p| p.unwrap_or_default()))
        .then_ignore(just(Token::Is))
        .then_ignore(enum_kw())
        .then(
            variant()
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|((name, type_params), variants)| EnumDecl {
            name,
            type_params,
            variants,
        })
        .spanned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chumsky::input::Input;
    use crate::{
        api::{ast::Item, token::Token as Tok},
        lexer::lexer,
    };

    fn parse_enum(src: &str) -> Result<Spanned<EnumDecl<'_>>, String> {
        let tokens: Vec<Spanned<Tok<'_>>> = lexer()
            .parse(src)
            .into_result()
            .map_err(|e| format!("lex error: {e:?}"))?;
        enum_decl()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .map_err(|e| format!("parse error: {e:?}"))
    }

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
    fn parses_nullary_only_enum() {
        let src = "Color is enum { Red Green Blue }";
        let decl = parse_enum(src).expect("parses");
        assert_eq!(decl.inner.name.inner.0, "Color");
        assert_eq!(decl.inner.variants.len(), 3);
        for v in &decl.inner.variants {
            assert!(matches!(v.inner.payload, VariantPayload::None));
        }
        assert_eq!(decl.inner.variants[0].inner.name.inner.0, "Red");
        assert_eq!(decl.inner.variants[1].inner.name.inner.0, "Green");
        assert_eq!(decl.inner.variants[2].inner.name.inner.0, "Blue");
    }

    #[test]
    fn parses_mixed_variant_payloads() {
        let src = "HTTPMethod is enum {
          Get
          Post(buf)
          Put(buf)
          Delete
          Patch(buf)
        }";
        let decl = parse_enum(src).expect("parses");
        assert_eq!(decl.inner.variants.len(), 5);

        let v = &decl.inner.variants;
        assert_eq!(v[0].inner.name.inner.0, "Get");
        assert!(matches!(v[0].inner.payload, VariantPayload::None));

        assert_eq!(v[1].inner.name.inner.0, "Post");
        let VariantPayload::Tuple(ref types) = v[1].inner.payload else {
            panic!("Post should be tuple, got {:?}", v[1].inner.payload);
        };
        assert_eq!(types.len(), 1);
        let Type::Named(ref n) = types[0].inner else { panic!() };
        assert_eq!(n.inner.0, "buf");

        assert_eq!(v[3].inner.name.inner.0, "Delete");
        assert!(matches!(v[3].inner.payload, VariantPayload::None));
    }

    #[test]
    fn parses_tuple_payload_with_multiple_types() {
        let src = "Range is enum {
          Empty
          Bounded(i64 i64)
        }";
        let decl = parse_enum(src).expect("parses");
        assert_eq!(decl.inner.variants.len(), 2);
        let VariantPayload::Tuple(ref types) = decl.inner.variants[1].inner.payload else {
            panic!("Bounded should be tuple");
        };
        assert_eq!(types.len(), 2);
    }

    #[test]
    fn parses_record_payload_variant() {
        let src = "Range is enum {
          Empty
          Open { from i64 }
        }";
        let decl = parse_enum(src).expect("parses");
        assert_eq!(decl.inner.variants.len(), 2);
        let VariantPayload::Record(ref fields) = decl.inner.variants[1].inner.payload else {
            panic!("Open should be record-style payload");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0.inner.0, "from");
        let Type::Named(ref ty) = fields[0].1.inner else { panic!() };
        assert_eq!(ty.inner.0, "i64");
    }

    #[test]
    fn rejects_enum_without_body() {
        let src = "Empty is enum";
        let res = parse_enum(src);
        assert!(res.is_err(), "expected parse error, got {res:?}");
    }

    #[test]
    fn rejects_enum_with_empty_body() {
        // `at_least(1)` on variants — an empty `{}` must NOT parse as an
        // enum declaration.
        let src = "Empty is enum { }";
        let res = parse_enum(src);
        assert!(res.is_err(), "empty enum body should not parse, got {res:?}");
    }

    #[test]
    fn enum_parses_as_top_level_item() {
        let src = "Color is enum { Red Green Blue }";
        let items = parse_top(src).expect("parses");
        assert_eq!(items.len(), 1);
        let Item::Enum(decl) = &items[0].inner else {
            panic!("expected Item::Enum, got {:?}", items[0].inner)
        };
        assert_eq!(decl.inner.variants.len(), 3);
    }

    #[test]
    fn parses_generic_enum_single_param() {
        let src = "Option[T] is enum { Some(T) None }";
        let decl = parse_enum(src).expect("parses");
        assert_eq!(decl.inner.name.inner.0, "Option");
        assert_eq!(decl.inner.type_params.len(), 1);
        assert_eq!(decl.inner.type_params[0].inner.0, "T");
        assert_eq!(decl.inner.variants.len(), 2);
    }

    #[test]
    fn parses_generic_enum_two_params() {
        let src = "Result[T E] is enum { Ok(T) Err(E) }";
        let decl = parse_enum(src).expect("parses");
        assert_eq!(decl.inner.type_params.len(), 2);
        assert_eq!(decl.inner.type_params[0].inner.0, "T");
        assert_eq!(decl.inner.type_params[1].inner.0, "E");
    }

    #[test]
    fn generic_enum_top_level_parses() {
        let src = "Option[T] is enum { Some(T) None }";
        let items = parse_top(src).expect("parses");
        assert_eq!(items.len(), 1);
        let Item::Enum(decl) = &items[0].inner else {
            panic!("expected Item::Enum, got {:?}", items[0].inner)
        };
        assert_eq!(decl.inner.type_params.len(), 1);
    }

    #[test]
    fn record_still_parses_when_enum_added() {
        // Regression: ensure adding enum to the top-level choice didn't
        // shadow record parsing.
        let src = "Request is { method buf  path buf }";
        let items = parse_top(src).expect("parses");
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].inner, Item::Record(_)));
    }
}
