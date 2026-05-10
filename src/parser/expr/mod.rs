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
        // A bare ident becomes `Expr::Var`; an ident followed by one or
        // more `:ident` segments becomes `Expr::Path` (module-qualified
        // value).  Single-segment paths collapse to `Var` so the rest of
        // the pipeline (resolver, typeck, codegen) can continue treating
        // bare names as before.
        let var = select_ref! {
            Token::Ident(n) = e => Ident::new(n).with_span(e.span())
        }
        .then(
            just(Token::Colon)
                .ignore_then(select_ref! {
                    Token::Ident(n) = e => Ident::new(n).with_span(e.span())
                })
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map(
            |(head, tail): (Spanned<Ident<'src>>, Vec<Spanned<Ident<'src>>>)| {
                if tail.is_empty() {
                    Expr::Var(head.inner.0, Type::Unknown)
                } else {
                    let mut segments = Vec::with_capacity(tail.len() + 1);
                    segments.push(head);
                    segments.extend(tail);
                    Expr::Path(segments)
                }
            },
        );
        // Bare `self` evaluates to the current actor's own pid — used as a
        // value (e.g. as an argument to other actors so they can send back).
        // `self(args)` continues to mean "state transition into this
        // machine"; the postfix-call pratt branch handles that case by
        // wrapping `self` in a Jump.
        let self_kw = just(Token::SelfKw).map_with(|_, e| {
            Expr::Var(
                "self",
                Type::Named(Ident::new("pid").with_span(e.span())),
            )
        });

        // `:ident` — atom literal.  Compile-time interned tag value used as
        // discriminator in tagged tuples (`{ :ok 42 }`) and patterns.  Lexer
        // emits `Colon` + `Ident`; the leading `Colon` only forms an atom
        // when nothing precedes it inside the same expression slot — module
        // paths (`core:io`) start with an ident, so the `var` arm above
        // claims them first.
        let atom_lit = just(Token::Colon)
            .ignore_then(select_ref! {
                Token::Ident(n) = _e => n,
            })
            .map(|n| Expr::Atom(n));

        // `{ a b c }` — anonymous tuple in expression position.  The same
        // `CurlyBrackets` token also delimits blocks (when/wait handler
        // bodies, branch bodies), but those grammar contexts consume the
        // brackets themselves before falling through to `atom`, so reaching
        // this rule means we are genuinely at a value position.
        let tuple_lit = expr
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
            .map(Expr::Tuple);

        // `(expr)` — parenthesised grouping at expression-start.  The
        // same `Parens` token serves as the postfix `Call(args)`
        // delimiter, so calling `(expr)` ambiguates with using the
        // grouping in an argument list (`f((w) (i-2)*4)` would parse
        // as `f(w(i-2) * 4)` if `(w)` could carry a postfix call).
        // Solution: paren_group lives at the primary level (next to
        // atom-with-postfix), NOT inside `atom`, so it never accrues
        // a postfix `(...)` of its own.  To call a value-of-expression
        // anyway, hoist via `let f_val = (...); f_val(args)`.
        // Grouping accepts both shapes — a parenthesised value is a
        // group whether or not whitespace precedes it (`(x + 1)` at
        // any position).  Only the postfix call rule cares about the
        // distinction, since calls must be tight (no space between
        // callee and `(`).
        let paren_group = expr
            .clone()
            .nested_in(select_ref!(
                Token::Parens(ts) = e => ts.split_spanned(e.span()),
                Token::TightParens(ts) = e => ts.split_spanned(e.span()),
            ))
            .map(|inner: Spanned<Expr<'src>>| inner.inner);

        let atom = choice((
            tuple_lit.boxed(),
            atom_lit.boxed(),
            paren_group.boxed(),
            num.boxed(),
            string_lit.boxed(),
            bool_false.boxed(),
            bool_true.boxed(),
            self_kw.boxed(),
            var.boxed(),
        ));

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

        // Sender-pid filter list — zero or more expressions between
        // `wait` and the handler block.  Each filter expression is
        // expected to evaluate to a pid; the runtime accepts a message
        // only when its first arg matches one of these pids.
        //
        // No surrounding brackets — the handler block's `{` ends the
        // filter list.
        let wait_expr = just(Token::Wait)
            .ignore_then(expr.clone().repeated().collect::<Vec<_>>())
            .then(
                wait_handler
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .nested_in(
                        select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
                    ),
            )
            .map(|(filter, handlers)| Expr::Wait { handlers, filter });

        // `let { a b c } = expr` — positional tuple destructure.
        // Tried before the regular let so the `{` after `let` doesn't
        // get misread as a block.
        let let_destructure = just(Token::Let)
            .ignore_then(
                ident_parser()
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
            )
            .then_ignore(just(Token::Eq))
            .then(expr.clone())
            .map(|(fields, default)| Expr::LetTuple {
                fields,
                default: Box::new(default),
            });

        let let_expr = just(Token::Let)
            .ignore_then(ident_parser())
            .then(type_expr().boxed().or_not())
            .then_ignore(just(Token::Eq))
            .then(expr.clone())
            .map(|((ident, ty), default)| {
                // Missing annotation parses as `Type::Unknown` so the
                // resolver knows to infer from the right-hand side.  An
                // explicit `{}` in source still produces `Type::Unit`.
                let ty = ty.unwrap_or_else(|| Type::Unknown.with_span(ident.span));
                Expr::Let {
                    ident,
                    ty,
                    default: Some(Box::new(default)),
                }
            });

        // `ret <expr>` — early return from a ret-typed branch.  The
        // entire trailing expression is consumed greedily so
        // `ret n + 1` parses as `Ret(Add(n, 1))`.
        let ret_expr = just(Token::Ret)
            .ignore_then(expr.clone())
            .map(|inner| Expr::Ret(Box::new(inner)));

        // `pin <expr>` — sync sugar for a ret-machine call.  Same
        // greedy capture as `ret`: `pin println(s)` parses as
        // `Pin(Jump(println, [s]))`, and the lowering turns it into
        // `let __pin_<id> = println(s)`.
        let pin_expr = just(Token::Pin)
            .ignore_then(expr.clone())
            .map(|inner| Expr::Pin(Box::new(inner)));

        let base = choice((
            ret_expr.spanned().boxed(),
            pin_expr.spanned().boxed(),
            wait_expr.spanned().boxed(),
            let_destructure.spanned().boxed(),
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
            /// `.0`, `.1`, … — tuple positional index
            TupleIdx(usize),
        }

        // Postfix call only attaches when the `(` is tight (no
        // whitespace between callee and parens).  `Token::TightParens`
        // is emitted by the lexer's adjacency post-pass; the plain
        // `Token::Parens` variant means a space-separated `(...)`
        // which is grammar-level "fresh value", not a call.
        let call_op = expr
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .nested_in(select_ref!(Token::TightParens(ts) = e => ts.split_spanned(e.span())))
            .map(PostfixOp::Call);

        // `@method(args)` reuses tight parens for the same reason —
        // `obj@m (args)` would otherwise parse `obj@m` and then drop
        // `(args)` as a stranded paren-group.
        let at_op = just(Token::At)
            .ignore_then(ident_parser())
            .then(
                expr.clone()
                    .repeated()
                    .collect::<Vec<_>>()
                    .nested_in(select_ref!(Token::TightParens(ts) = e => ts.split_spanned(e.span())))
                    .or_not(),
            )
            .map(|(method, args)| match args {
                Some(args) => PostfixOp::AtCall(method, args),
                None => PostfixOp::AtAccess(method),
            });

        // `.<num>` — positional access on a tuple.  Tried BEFORE `.field`
        // because the lexer emits `Token::Num` rather than `Token::Ident`
        // for a digit run, so the ident-only branch would otherwise reject
        // perfectly valid `t.0` syntax.  Non-integer numerics (`t.1.5`)
        // fall through to `parse::<usize>` failure → parse error, which
        // is the right outcome — fractional indices have no meaning.
        let dot_op = just(Token::Dot).ignore_then(choice((
            expr.clone()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
                .map(PostfixOp::DotInit),
            select_ref! { Token::Num(n) = _e => n }
                .try_map(|n: &&str, span| {
                    n.parse::<usize>()
                        .map(PostfixOp::TupleIdx)
                        .map_err(|_| Rich::custom(span, format!("invalid tuple index: {n}")))
                }),
            ident_parser().map(PostfixOp::DotAccess),
        )));

        let postfix = choice((call_op, at_op, dot_op));

        base.then(postfix.spanned().repeated().collect::<Vec<_>>())
            .map(|(base_expr, ops)| {
                ops.into_iter().fold(base_expr, |acc, op| {
                    let span = acc.span;
                    match op.inner {
                        PostfixOp::Call(args) => {
                            // Callee can be either a bare ident (Var) or a
                            // module-qualified path (Path).  In the Var case
                            // we tag the callee with `pid` so downstream
                            // hashing treats it as a process spawn target;
                            // in the Path case we keep the path as-is and
                            // let the resolver follow the module chain.
                            let callee = match acc.inner {
                                Expr::Var(ident, _ty) => Expr::Var(
                                    ident,
                                    Type::Named(Ident::new("pid").with_span(span)),
                                )
                                .with_span(span),
                                Expr::Path(_) => acc,
                                other => panic!(
                                    "call applied to non-callable expression: {other:?}"
                                ),
                            };
                            Expr::Jump {
                                ident: Box::new(callee),
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
                        PostfixOp::TupleIdx(index) => Expr::TupleIndex {
                            receiver: Box::new(acc),
                            index,
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
                // Shifts bind looser than +/- but tighter than the
                // bitwise group below, matching Java/Rust convention so
                // `x + 1 >> 2` is `(x + 1) >> 2`.
                infix(left(8), just(Token::Shl), |x, _, y, e| {
                    Expr::Shl(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(8), just(Token::Shr), |x, _, y, e| {
                    Expr::Shr(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                // Bitwise AND > XOR > OR — stricter than comparisons so
                // `x & 0xff == 0` parses the way crypto code expects.
                infix(left(7), just(Token::BitAnd), |x, _, y, e| {
                    Expr::BAnd(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(6), just(Token::BitXor), |x, _, y, e| {
                    Expr::BXor(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(5), just(Token::BitOr), |x, _, y, e| {
                    Expr::BOr(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(4), just(Token::LessEq), |x, _, y, e| {
                    Expr::Le(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(4), just(Token::GreaterEq), |x, _, y, e| {
                    Expr::Ge(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(4), just(Token::Less), |x, _, y, e| {
                    Expr::Lt(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(4), just(Token::Greater), |x, _, y, e| {
                    Expr::Gt(Box::new(x), Box::new(y)).with_span(e.span())
                }),
                infix(left(3), just(Token::EqEq), |x, _, y, e| {
                    Expr::Eq(Box::new(x), Box::new(y)).with_span(e.span())
                }),
            ))
    })
}
