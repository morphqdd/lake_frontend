use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    pratt::{infix, left, prefix},
    prelude::{choice, just, recursive},
    select_ref,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{Branch, Ident, Pattern, Type},
        expr::Expr,
        token::Token,
    },
    parser::{expr::type_expr::type_expr, helpers::TokenInput, helpers::ident_parser},
};

pub mod type_expr;

pub fn expr<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Expr<'src>>, Err<Rich<'t, Token<'src>>>> {
    recursive(|expr| {
        let num = select_ref! {
            Token::Num(n) = e =>
                Expr::Num(n, Type::Named(Ident::new("i64").with_span(e.span())))
        };
        let string_lit = select_ref! {
            Token::String(n) = e =>
                Expr::String(n, Type::Named(Ident::new("str").with_span(e.span())))
        };
        let bool_false = just(Token::False).to(Expr::Bool(false));
        let bool_true = just(Token::True).to(Expr::Bool(true));
        let var = select_ref! {
            Token::Ident(n) = _e => Expr::Var(n, Type::Unknown)
        };
        let self_kw = just(Token::SelfKw)
            .map_with(|_, _e| Expr::Var("self", Type::Unknown));

        let atom = choice((num, string_lit, bool_false, bool_true, self_kw, var));

        let when_branch =
            expr.clone()
                .then_ignore(just(Token::Arrow))
                .then(expr.clone().repeated().collect::<Vec<_>>().nested_in(
                    select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
                ))
                .map(|(cond, body)| (cond, body));

        let when_expr = just(Token::When)
            .ignore_then(expr.clone())
            .then(
                when_branch
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .nested_in(
                        select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
                    ),
            )
            .map(|(cond, branches)| Expr::When {
                cond: Box::new(cond),
                branches,
            });

        // Inline pattern parser for wait handlers (mirrors pattern.rs but avoids
        // mutual recursion with top-level expr()).
        let wait_pattern = {
            let string_guard = select_ref!(
                Token::String(n) = e => Ident::new(n).with_span(e.span()),
            )
            .then(type_expr())
            .map(|(ident, ty)| Pattern::new_string_guard(ident, ty))
            .spanned();

            let normal = select_ref!(
                Token::Ident(n) = e => Ident::new(n).with_span(e.span()),
                Token::Num(n)   = e => Ident::new(n).with_span(e.span()),
            )
            .then(type_expr().or_not())
            .map(|(ident, opt_ty)| {
                let ty = opt_ty.unwrap_or_else(|| Type::Unit.with_span(ident.span));
                Pattern::new(ident, ty)
            })
            .spanned();

            string_guard.or(normal)
        };

        let wait_handler =
            just(Token::At)
                .ignore_then(ident_parser())
                .or_not()
                .then(wait_pattern.repeated().at_least(1).collect::<Vec<_>>())
                .then_ignore(just(Token::Arrow))
                .then(expr.clone().repeated().collect::<Vec<_>>().nested_in(
                    select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
                ))
                .map(|((label, patterns), body)| Branch::new(label, patterns, None, body))
                .spanned();

        let wait_expr = just(Token::Wait)
            .ignore_then(
                wait_handler
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .nested_in(
                        select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
                    ),
            )
            .map(|handlers| Expr::Wait { handlers });

        let let_expr = just(Token::Let)
            .ignore_then(ident_parser())
            .then(type_expr().boxed().or_not())
            .then_ignore(just(Token::Eq))
            .then(expr.clone())
            .map(|((ident, ty), default)| {
                let ty = ty.unwrap_or_else(|| Type::Unit.with_span(ident.span));
                Expr::Let {
                    ident,
                    ty,
                    default: Some(Box::new(default)),
                }
            });

        let base = choice((
            wait_expr.spanned().boxed(),
            let_expr.spanned().boxed(),
            when_expr.spanned(),
            atom.spanned(),
        ));

        // Postfix is represented as a local enum.  We collect them with
        // `repeated()` and fold left into the accumulator.

        enum PostfixOp<'src> {
            /// `(args)` — regular call
            Call(Vec<Spanned<Expr<'src>>>),
            /// `@method(args)` — call via @
            AtCall(Spanned<Ident<'src>>, Vec<Spanned<Expr<'src>>>),
            /// `@field` — access via @ (no call)
            AtAccess(Spanned<Ident<'src>>),
            /// `.{ fields }` — struct init
            DotInit(Vec<Spanned<Expr<'src>>>),
            /// `.field` — dot field access
            DotAccess(Spanned<Ident<'src>>),
        }

        let call_op = expr
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .nested_in(select_ref!(Token::Parens(ts) = e => ts.split_spanned(e.span())))
            .map(PostfixOp::Call);

        let at_op = just(Token::At)
            .ignore_then(ident_parser())
            .then(
                expr.clone()
                    .repeated()
                    .collect::<Vec<_>>()
                    .nested_in(select_ref!(Token::Parens(ts) = e => ts.split_spanned(e.span())))
                    .or_not(),
            )
            .map(|(method, args)| match args {
                Some(args) => PostfixOp::AtCall(method, args),
                None => PostfixOp::AtAccess(method),
            });

        let dot_op = just(Token::Dot).ignore_then(choice((
            expr.clone()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
                .map(PostfixOp::DotInit),
            ident_parser().map(PostfixOp::DotAccess),
        )));

        let postfix = choice((call_op, at_op, dot_op));

        base.then(postfix.spanned().repeated().collect::<Vec<_>>())
            .map(|(base_expr, ops)| {
                ops.into_iter().fold(base_expr, |acc, op| {
                    let span = acc.span;
                    match op.inner {
                        PostfixOp::Call(args) => {
                            let Expr::Var(ident, _ty) = acc.inner else {
                                panic!("Parser bug")
                            };
                            Expr::Jump {
                                ident: Box::new(
                                    Expr::Var(
                                        ident,
                                        Type::Named(Ident::new("pid").with_span(span)),
                                    )
                                    .with_span(span),
                                ),
                                args,
                            }
                        }
                        .with_span(span),
                        PostfixOp::AtCall(method, args) => Expr::MethodCall {
                            receiver: Box::new(acc),
                            method,
                            args,
                        }
                        .with_span(span),
                        PostfixOp::AtAccess(field) => Expr::AtAccess {
                            receiver: Box::new(acc),
                            field,
                        }
                        .with_span(span),
                        PostfixOp::DotInit(fields) => Expr::StructInit {
                            base: Box::new(acc),
                            fields,
                        }
                        .with_span(span),
                        PostfixOp::DotAccess(field) => Expr::DotAccess {
                            receiver: Box::new(acc),
                            field,
                        }
                        .with_span(span),
                    }
                })
            })
            .pratt((
                // Prefix unary minus.  Bind tighter than `*` so `-3 * 2`
                // parses as `(-3) * 2` rather than `-(3 * 2)`.
                prefix(11, just(Token::Minus), |_, x: Spanned<Expr<'src>>, e| {
                    Expr::Neg(Box::new(x)).with_span(e.span())
                }),
                infix(left(10), just(Token::Star), |x, _, y, e| {
                    Expr::Mul(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(10), just(Token::Slash), |x, _, y, e| {
                    Expr::Div(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(9), just(Token::Plus), |x, _, y, e| {
                    Expr::Add(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(9), just(Token::Minus), |x, _, y, e| {
                    Expr::Sub(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(8), just(Token::LessEq), |x, _, y, e| {
                    Expr::Le(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(8), just(Token::GreaterEq), |x, _, y, e| {
                    Expr::Ge(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(8), just(Token::EqEq), |x, _, y, e| {
                    Expr::Eq(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(8), just(Token::Less), |x, _, y, e| {
                    Expr::Lt(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(8), just(Token::Greater), |x, _, y, e| {
                    Expr::Gt(Box::new(x), Box::new(y)).with_span(e.span())
                }),
            ))
    })
}
