use std::{cell::RefCell, rc::Rc};

use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::just,
    select_ref,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{Ident, Import},
        token::Token,
    },
    parser::{helpers::TokenInput, helpers::ident_parser},
};

/// Each multi-import block entry parses as `((head, tail), alias)`:
/// chumsky's repeated `.then(...)` produces nested tuples, not flat ones.
type MultiEntry<'src> = (
    (
        Spanned<Rc<RefCell<Import<'src>>>>,
        Option<Spanned<Rc<RefCell<Import<'src>>>>>,
    ),
    Option<Spanned<Ident<'src>>>,
);

type SpannedImport<'src> = Spanned<Vec<MultiEntry<'src>>>;

/// Walk the chain rooted at `head` to its leaf node and attach an alias
/// there.  Silently no-ops on `MultiImport` nodes (the caller is
/// responsible for routing entry-level aliases to the right leaves).
fn attach_alias_to_leaf<'src>(
    head: &Spanned<Rc<RefCell<Import<'src>>>>,
    alias: Spanned<Ident<'src>>,
) {
    let mut current = head.clone();
    loop {
        let next_opt = match &*current.inner.borrow() {
            Import::Import(_, next, _) => next.clone(),
            Import::MultiImport(_) => return,
        };
        match next_opt {
            Some(next) => current = next,
            None => break,
        }
    }
    current.inner.borrow_mut().set_alias(alias);
}

pub fn import<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Rc<RefCell<Import<'src>>>>, Err<Rich<'t, Token<'src>>>>
{
    let single_import = select_ref!(Token::Ident(n) = e =>
        Rc::new(RefCell::new(Import::Import(
            Ident::new(n).with_span(e.span()),
            None,
            None,
        )))
    );

    let next_single = just(Token::Dot).ignore_then(single_import.spanned());

    // Optional `as <ident>` suffix.
    let as_alias = just(Token::As).ignore_then(ident_parser()).or_not();

    // Multi-import: `{ a.b c.d as foo  }`
    //
    // Each entry is `path [as alias]` where `path` is `head [.next ...]`.
    let multiimport = single_import
        .spanned()
        .then(next_single.clone().repeated().collect::<Vec<_>>().map(
            |v: Vec<Spanned<Rc<RefCell<Import<'src>>>>>| {
                if v.is_empty() {
                    return None;
                }
                let base = v[0].clone();
                let mut curr = base.clone();
                for import in v[1..].iter() {
                    let next = curr.inner.borrow_mut().set_next(import.clone());
                    curr = next;
                }
                Some(base)
            },
        ))
        .then(as_alias.clone())
        .repeated()
        .collect::<Vec<_>>()
        .spanned()
        .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
        .map(|v: SpannedImport<'src>| {
            Rc::new(RefCell::new(Import::MultiImport(
                v.inner
                    .into_iter()
                    .map(|((head, tail), alias)| {
                        if let Some(tail) = tail {
                            head.inner.borrow_mut().set_next(tail);
                        }
                        if let Some(alias) = alias {
                            attach_alias_to_leaf(&head, alias);
                        }
                        head
                    })
                    .collect(),
            )))
            .with_span(v.span)
        });

    let next = just(Token::Dot).ignore_then(multiimport.or(single_import.spanned()));

    just(Token::Plus)
        .ignore_then(single_import.spanned())
        .then(next.repeated().collect::<Vec<_>>())
        .then(as_alias)
        .map(
            |((head, tail), alias): (
                (
                    Spanned<Rc<RefCell<Import<'src>>>>,
                    Vec<Spanned<Rc<RefCell<Import<'src>>>>>,
                ),
                Option<Spanned<Ident<'src>>>,
            )| {
                if let Some(base) = tail.first() {
                    let base = base.clone();
                    let mut curr = base.clone();
                    for import in tail[1..].iter() {
                        let next = curr.inner.borrow_mut().set_next(import.clone());
                        curr = next;
                    }
                    head.inner.borrow_mut().set_next(base);
                }
                if let Some(alias) = alias {
                    attach_alias_to_leaf(&head, alias);
                }
                head
            },
        )
}
