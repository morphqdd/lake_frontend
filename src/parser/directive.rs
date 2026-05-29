use chumsky::{
    IterParser, Parser, error::Rich, extra::Err, input::Input, prelude::just, select_ref,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{ast::Directive, token::Token},
    parser::{
        expr::type_expr::type_expr,
        helpers::{TokenInput, ident_parser},
    },
};

/// Parses a built-in macro attribute: `@rt_st(...)`, `@ffi(...)`,
/// `@cfg(arch="x86_64")`.
///
/// Two argument shapes are accepted inside the parens:
///
///   * `key="string"` pairs — `@cfg(arch="x86_64")`.  Tried first because
///     `arch` would otherwise lex as a bare `Type::Named` and the trailing
///     `="x86_64"` would dangle.
///   * a sequence of type expressions — the historical `@rt(name {…} {…})`
///     form.
///
/// A directive with no parens carries empty args (`@rt_st`).
pub fn directive<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Directive<'src>>, Err<Rich<'t, Token<'src>>>> {
    // `key="string"` — one or more.  Captures the raw string slice so the
    // cfg-filter pass can compare it against the compile target.
    let kv = ident_parser()
        .then_ignore(just(Token::Eq))
        .then(select_ref!(Token::String(s) = e => (*s).with_span(e.span())))
        .map(|(key, val)| (key, val));

    let kv_args = kv
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .nested_in(select_ref!(
            Token::Parens(ts) = e => ts.split_spanned(e.span()),
            Token::TightParens(ts) = e => ts.split_spanned(e.span()),
        ));

    let type_args = type_expr()
        .repeated()
        .collect::<Vec<_>>()
        .nested_in(select_ref!(
            Token::Parens(ts) = e => ts.split_spanned(e.span()),
            Token::TightParens(ts) = e => ts.split_spanned(e.span()),
        ));

    just(Token::At)
        .ignore_then(ident_parser())
        .then(
            kv_args
                .map(DirectiveArgs::Kv)
                .or(type_args.map(DirectiveArgs::Types))
                .or_not(),
        )
        .map(|(name, args)| match args {
            Some(DirectiveArgs::Kv(kv)) => Directive::with_kv(name, kv),
            Some(DirectiveArgs::Types(ts)) => Directive::new(name, ts),
            None => Directive::new(name, Vec::new()),
        })
        .spanned()
}

enum DirectiveArgs<'src> {
    Kv(Vec<(Spanned<crate::api::ast::Ident<'src>>, Spanned<&'src str>)>),
    Types(Vec<Spanned<crate::api::ast::Type<'src>>>),
}
