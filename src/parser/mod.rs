use std::{cell::RefCell, rc::Rc};

use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::{Input, MappedInput},
    pratt::{infix, left},
    prelude::{choice, just, recursive},
    select_ref,
    span::{SimpleSpan, SpanWrap, Spanned},
};

use crate::api::{
    ast::{Branch, Directive, Field, Ident, Import, Machine, MachineItem, Pattern, Type},
    expr::Expr,
    token::Token,
};

// ─── Type alias for the token input ───────────────────────────────────────────

type TokenInput<'t, 'src> = MappedInput<'t, Token<'src>, SimpleSpan, &'t [Spanned<Token<'src>>]>;

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn ident_parser<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Ident<'src>>, Err<Rich<'t, Token<'src>>>> + Clone {
    select_ref!(Token::Ident(n) = e => Ident::new(n).with_span(e.span()))
}

// ─── type_expr ────────────────────────────────────────────────────────────────

/// Parses a type expression:
/// - `i32`, `str`              → `Type::Named`
/// - `box(T)`, `result({})`   → `Type::Generic`
/// - `{}`                     → `Type::Unit`
/// - `{ i64 usize }`          → `Type::Struct`
fn type_expr<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Type<'src>>, Err<Rich<'t, Token<'src>>>> {
    recursive(|ty| {
        // Struct / unit type inside `{ ... }`
        let struct_ty = ty
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
            .map(|types: Vec<Spanned<Type<'src>>>| {
                if types.is_empty() {
                    Type::Unit
                } else {
                    Type::Struct(types)
                }
            })
            .spanned();

        // Simple ident, optionally followed by generic args `(T U)`
        let named = ident_parser()
            .then(
                ty.clone()
                    .repeated()
                    .collect::<Vec<_>>()
                    .nested_in(select_ref!(Token::Parens(ts) = e => ts.split_spanned(e.span())))
                    .or_not(),
            )
            .map(|(name, args)| match args {
                None => Type::Named(name),
                Some(args) => Type::Generic(name, args),
            })
            .spanned();

        choice((struct_ty, named))
    })
}

// ─── pattern ──────────────────────────────────────────────────────────────────

/// Parses a branch parameter: `n i32`, `n str."hello"`, `_`
///
/// `_` is the only pattern that may omit the type annotation.
pub fn pattern<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Pattern<'src>>, Err<Rich<'t, Token<'src>>>> {
    ident_parser()
        .then(
            type_expr()
                .then(just(Token::Dot).ignore_then(expr()).or_not())
                .or_not(),
        )
        .map(|(ident, opt)| {
            let (ty, default) = match opt {
                Some((ty, default)) => (ty, default),
                None => (Type::Unit.with_span(ident.span), None),
            };
            Pattern::new(ident, ty, default)
        })
        .spanned()
}

/// Parses a full expression with operator precedence and postfix chains.
fn expr<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Expr<'src>>, Err<Rich<'t, Token<'src>>>> {
    recursive(|expr| {
        // ── atoms ────────────────────────────────────────────────────────
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
            Token::Ident(n) = e =>
                Expr::Var(n, Type::Named(Ident::new("{}").with_span(e.span())))
        };

        let atom = choice((num, string_lit, bool_false, bool_true, var));

        // ── when expression ──────────────────────────────────────────────
        //
        // `when cond { pat -> { body } ... }`
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

        // ── base: atom or when ───────────────────────────────────────────
        let base = choice((when_expr.spanned(), atom.spanned()));

        // ── postfix operations ───────────────────────────────────────────
        //
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

        // ── chain base + postfix, then apply pratt ───────────────────────
        base.then(postfix.spanned().repeated().collect::<Vec<_>>())
            .map(|(base_expr, ops)| {
                ops.into_iter().fold(base_expr, |acc, op| {
                    let span = acc.span;
                    match op.inner {
                        PostfixOp::Call(args) => Expr::Jump {
                            ident: Box::new(acc),
                            args,
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
            ))
    })
}

// ─── field_decl ───────────────────────────────────────────────────────────────

/// Parses a field declaration: `@ok.T`, `@.raw_box(T)`, `@.{ str }`
fn field_decl<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Field<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::At)
        .ignore_then(ident_parser().or_not())
        .then_ignore(just(Token::Dot))
        .then(type_expr())
        .map(|(label, ty)| Field::new(label, ty))
        .spanned()
}

// ─── branch ───────────────────────────────────────────────────────────────────

/// Parses a process branch: `[@label] pattern+ -> [ret type] { body }`
fn branch<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Branch<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::At)
        .ignore_then(ident_parser())
        .or_not()
        .then(pattern().repeated().at_least(1).collect::<Vec<_>>())
        .then_ignore(just(Token::Arrow))
        .then(just(Token::Ret).ignore_then(type_expr()).or_not())
        .then(
            expr()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|(((label, patterns), ret_ty), body)| Branch::new(label, patterns, ret_ty, body))
        .spanned()
}

// ─── machine_item ─────────────────────────────────────────────────────────────

/// Tries `field_decl` first (has `.` after `@label`), then falls back to `branch`.
fn machine_item<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<MachineItem<'src>>, Err<Rich<'t, Token<'src>>>> {
    choice((
        field_decl()
            .map(|f: Spanned<Field>| MachineItem::Field(f.inner))
            .spanned(),
        branch()
            .map(|b: Spanned<Branch>| MachineItem::Branch(b.inner))
            .spanned(),
    ))
}

// ─── directive ────────────────────────────────────────────────────────────────

/// Parses a built-in macro attribute: `@rt_st(...)`, `@ffi(...)`.
/// Arguments inside the parens are parsed as a sequence of type expressions.
fn directive<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Directive<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::At)
        .ignore_then(ident_parser())
        .then(
            type_expr()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::Parens(ts) = e => ts.split_spanned(e.span())))
                .or_not()
                .map(|args| args.unwrap_or_default()),
        )
        .map(|(name, args)| Directive::new(name, args))
        .spanned()
}

// ─── machine ──────────────────────────────────────────────────────────────────

/// Parses `[pub] ident[(generics)] is { item* }`
pub fn machine<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Machine<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::Pub)
        .or_not()
        .map(|p| p.is_some())
        .then(ident_parser())
        .then(
            ident_parser()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::Parens(ts) = e => ts.split_spanned(e.span())))
                .or_not()
                .map(|g| g.unwrap_or_default()),
        )
        .then_ignore(just(Token::Is))
        .then(
            machine_item()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|(((vis, ident), generics), items)| Machine::new(vec![], vis, ident, generics, items))
        .spanned()
}

// ─── import ───────────────────────────────────────────────────────────────────

type SpannedImport<'src> = Spanned<
    Vec<(
        Spanned<Rc<RefCell<Import<'src>>>>,
        Option<Spanned<Rc<RefCell<Import<'src>>>>>,
    )>,
>;

fn import<'t, 'src: 't>()
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

// ─── program ──────────────────────────────────────────────────────────────────

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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use crate::{
        api::{
            ast::{Directive, Ident, Pattern, Type},
            expr::Expr,
        },
        lexer::lexer,
        parser::pattern,
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
        assert!(result.inner.default.is_none());
    }

    #[test]
    fn parse_pattern_with_default() {
        let result = parse_pattern("n i32.10").expect("should parse");
        assert_eq!(result.inner.ident.inner, Ident::new("n"));
        assert!(result.inner.default.is_some());
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
    fn parse_string_default_pattern() {
        let result = parse_pattern("n str.\"hello\"").expect("should parse");
        assert!(
            matches!(result.inner.default, Some(ref e) if matches!(e.inner, Expr::String(_, _)))
        );
    }

    // ── directive tests ───────────────────────────────────────────────────────

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
}
