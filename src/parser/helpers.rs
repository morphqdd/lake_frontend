use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::MappedInput,
    prelude::{choice, just},
    select_ref,
    span::{SimpleSpan, SpanWrap, Spanned},
};

use crate::api::{ast::Ident, token::Token};

pub fn ident_parser<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Ident<'src>>, Err<Rich<'t, Token<'src>>>> + Clone {
    select_ref!(Token::Ident(n) = e => Ident::new(n).with_span(e.span()))
}

pub type TokenInput<'t, 'src> =
    MappedInput<'t, Token<'src>, SimpleSpan, &'t [Spanned<Token<'src>>]>;

/// agent: protocols-146 — flat element of a bound type-parameter list.
/// The bracket body `[T: Eq Ord  U: Show]` lexes to a stream of idents
/// and colons; we parse it flat and fold into params + bounds in Rust to
/// avoid chumsky lookahead gymnastics for the comma-less grammar.
#[derive(Clone)]
pub enum BoundTok<'src> {
    Ident(Spanned<Ident<'src>>),
    Colon,
}

/// Parses the *contents* of a `[...]` generic clause, accepting both bare
/// (`[T U]`) and bound (`[T: Eq Ord]`) forms.  Returns the ordered list of
/// type-parameter names plus a parallel bounds table.
///
/// Folding rule (comma-less): an ident that is immediately followed by a
/// `:` starts a new parameter; idents after the `:` are that parameter's
/// bounds until the next `ident :` pair.  An ident with no following `:`
/// is itself a parameter (unbounded) — unless it is currently being
/// consumed as a bound.
#[allow(clippy::type_complexity)]
pub fn type_param_bounds_body<'t, 'src: 't>() -> impl Parser<
    't,
    TokenInput<'t, 'src>,
    (
        Vec<Spanned<Ident<'src>>>,
        Vec<(Spanned<Ident<'src>>, Vec<Spanned<Ident<'src>>>)>,
    ),
    Err<Rich<'t, Token<'src>>>,
> {
    choice((
        ident_parser().map(BoundTok::Ident),
        just(Token::Colon).to(BoundTok::Colon),
    ))
    .repeated()
    .at_least(1)
    .collect::<Vec<_>>()
    .map(fold_bound_toks)
}

#[allow(clippy::type_complexity)]
fn fold_bound_toks<'src>(
    toks: Vec<BoundTok<'src>>,
) -> (
    Vec<Spanned<Ident<'src>>>,
    Vec<(Spanned<Ident<'src>>, Vec<Spanned<Ident<'src>>>)>,
) {
    let mut params: Vec<Spanned<Ident<'src>>> = Vec::new();
    let mut bounds: Vec<(Spanned<Ident<'src>>, Vec<Spanned<Ident<'src>>>)> = Vec::new();
    // `pending` holds the last seen ident that hasn't been classified as a
    // param yet.  When we hit a `:` it becomes the current param being
    // bounded; otherwise the next ident (or end) flushes it as an
    // unbounded param.
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            BoundTok::Ident(name) => {
                // Is this ident bounded?  (followed by a colon)
                if matches!(toks.get(i + 1), Some(BoundTok::Colon)) {
                    // Collect bounds: idents after the colon, stopping at
                    // the next `ident :` pair or end of stream.
                    let mut b: Vec<Spanned<Ident<'src>>> = Vec::new();
                    let mut j = i + 2;
                    while j < toks.len() {
                        match &toks[j] {
                            BoundTok::Ident(bname) => {
                                // A bound ident followed by `:` is actually
                                // the next parameter — don't consume it.
                                if matches!(toks.get(j + 1), Some(BoundTok::Colon)) {
                                    break;
                                }
                                b.push(bname.clone());
                                j += 1;
                            }
                            BoundTok::Colon => break,
                        }
                    }
                    params.push(name.clone());
                    bounds.push((name.clone(), b));
                    i = j;
                } else {
                    params.push(name.clone());
                    i += 1;
                }
            }
            BoundTok::Colon => {
                // Stray colon (malformed); skip it.
                i += 1;
            }
        }
    }
    (params, bounds)
}
