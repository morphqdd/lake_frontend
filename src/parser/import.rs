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
        expr::Expr,
        token::Token,
    },
    parser::helpers::TokenInput,
};

type SpannedImport<'src> = Spanned<
    Vec<(
        Spanned<Rc<RefCell<Import<'src>>>>,
        Option<Spanned<Rc<RefCell<Import<'src>>>>>,
    )>,
>;

pub fn import<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Expr<'src>>, Err<Rich<'t, Token<'src>>>> {
    let single_import = select_ref!(Token::Ident(n) = e =>
        Rc::new(RefCell::new(Import::Import(
            Ident::new(n).with_span(e.span()),
            None,
        )))
    );

    let next_single = just(Token::Dot).ignore_then(single_import.spanned());

    // Multi-import: `{ a.b c.d }`
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
        .repeated()
        .collect::<Vec<_>>()
        .spanned()
        .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
        .map(|v: SpannedImport<'src>| {
            Rc::new(RefCell::new(Import::MultiImport(
                v.inner
                    .into_iter()
                    .map(|(head, tail)| {
                        if let Some(tail) = tail {
                            head.inner.borrow_mut().set_next(tail);
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
        .map(|(head, tail): (Spanned<Rc<RefCell<Import<'src>>>>, _)| {
            if let Some(base) = tail.first() {
                let base = base.clone();
                let mut curr = base.clone();
                for import in tail[1..].iter() {
                    let next = curr.inner.borrow_mut().set_next(import.clone());
                    curr = next;
                }
                head.inner.borrow_mut().set_next(base);
            }
            Expr::Import(head)
        })
        .spanned()
}
