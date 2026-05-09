use std::collections::HashMap;

use chumsky::span::{SpanWrap, Spanned};

use crate::api::{
    ast::{Branch, Item, Machine, MachineItem, Type},
    expr::Expr,
};

/// True when the parser emitted a placeholder type that the resolver needs to
/// fill in.  `Type::Unit` (`{}` in source) is *not* unknown — it is a legitimate
/// concrete type.
pub(crate) fn is_unknown(ty: &Type<'_>) -> bool {
    matches!(ty, Type::Unknown)
}

/// Walks a parsed Lake AST and fills in `{}` placeholder types on variable
/// references with the actual declared type from the enclosing branch pattern
/// or `let` binding.
///
/// Each `Branch` gets its own scope that is discarded after the branch body is
/// processed, so variable bindings do not leak across branches.
pub struct Resolver<'src> {
    scope: HashMap<&'src str, Type<'src>>,
}

impl Default for Resolver<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'src> Resolver<'src> {
    pub fn new() -> Self {
        Self {
            scope: HashMap::new(),
        }
    }

    fn bind(&mut self, name: &'src str, ty: Type<'src>) {
        self.scope.insert(name, ty);
    }

    fn resolve_var_ty(&self, name: &'src str, ty: Type<'src>) -> Type<'src> {
        if is_unknown(&ty) {
            self.scope.get(name).cloned().unwrap_or(ty)
        } else {
            ty
        }
    }

    fn resolve_expr(&mut self, expr: Spanned<Expr<'src>>) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Var(name, ty) => Expr::Var(name, self.resolve_var_ty(name, ty)),

            Expr::Let { ident, ty, default } => {
                // Bind before resolving the default so the variable is in scope
                // (handles patterns with defaults that reference earlier bindings).
                self.bind(ident.inner.0, ty.inner.clone());
                let default = default.map(|d| Box::new(self.resolve_expr(*d)));
                Expr::Let { ident, ty, default }
            }

            Expr::Jump { ident, args } => {
                let ident = Box::new(self.resolve_expr(*ident));
                let args = args.into_iter().map(|a| self.resolve_expr(a)).collect();
                Expr::Jump { ident, args }
            }

            Expr::Add(l, r) => Expr::Add(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Neg(inner) => Expr::Neg(Box::new(self.resolve_expr(*inner))),

            Expr::When { cond, branches } => {
                let cond = Box::new(self.resolve_expr(*cond));
                let branches = branches
                    .into_iter()
                    .map(|(pat, body)| {
                        let pat = self.resolve_expr(pat);
                        let body = body.into_iter().map(|e| self.resolve_expr(e)).collect();
                        (pat, body)
                    })
                    .collect();
                Expr::When { cond, branches }
            }

            Expr::Wait { handlers } => {
                let handlers = handlers
                    .into_iter()
                    .map(|h| {
                        let span = h.span;
                        self.resolve_branch(h.inner).with_span(span)
                    })
                    .collect();
                Expr::Wait { handlers }
            }

            other => other,
        };
        inner.with_span(span)
    }

    fn resolve_branch(&mut self, branch: Branch<'src>) -> Branch<'src> {
        let saved = self.scope.clone();

        // Bind all non-wildcard, non-literal-guard pattern variables into scope.
        for pat in &branch.patterns {
            if !pat.inner.is_wildcard() && !pat.inner.is_literal_guard() {
                self.bind(pat.inner.ident.inner.0, pat.inner.ty.inner.clone());
            }
        }

        let body = branch
            .body
            .into_iter()
            .map(|e| self.resolve_expr(e))
            .collect();

        self.scope = saved;
        Branch::new(branch.label, branch.patterns, branch.ret_ty, body)
    }

    fn resolve_machine(&mut self, machine: Machine<'src>) -> Machine<'src> {
        let items = machine
            .items
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    MachineItem::Branch(b) => {
                        MachineItem::Branch(self.resolve_branch(b)).with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect();
        Machine::new(
            machine.attrs,
            machine.vis,
            machine.ident,
            machine.generics,
            items,
        )
    }

    /// Resolve types in a complete parsed program.
    pub fn resolve(&mut self, program: Vec<Spanned<Item<'src>>>) -> Vec<Spanned<Item<'src>>> {
        program
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    Item::Machine(m) => {
                        let machine_span = m.span;
                        Item::Machine(self.resolve_machine(m.inner).with_span(machine_span))
                            .with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect()
    }
}

/// Convenience wrapper: resolve types in a parsed Lake program.
pub fn resolve<'src>(program: Vec<Spanned<Item<'src>>>) -> Vec<Spanned<Item<'src>>> {
    Resolver::new().resolve(program)
}

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use super::resolve;
    use crate::{
        api::{
            ast::{Ident, Item, MachineItem, Type},
            expr::Expr,
        },
        lexer::lexer,
        parser::program,
    };

    fn parse_resolve(src: &str) -> Vec<chumsky::span::Spanned<Item<'_>>> {
        let tokens = lexer().parse(src).into_result().expect("lex error");
        let ast = program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse error");
        resolve(ast)
    }

    fn first_branch_body(src: &str) -> Vec<Expr<'_>> {
        let ast = parse_resolve(src);
        let Item::Machine(m) = &ast[0].inner else {
            panic!("expected Machine")
        };
        let MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!("expected Branch")
        };
        b.body.iter().map(|e| e.inner.clone()).collect()
    }

    #[test]
    fn resolves_var_type_from_pattern() {
        // `n` is declared as `i64` in the pattern; the body just returns `n`.
        let body = first_branch_body("counter is { n i64 -> { n } }");
        let Expr::Var(name, ty) = &body[0] else {
            panic!("expected Var")
        };
        assert_eq!(*name, "n");
        assert!(
            matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected i64, got {ty:?}"
        );
    }

    #[test]
    fn resolves_var_type_in_self_call_args() {
        // `self(n)` where `n: i64` — the argument should be resolved to `i64`.
        let body = first_branch_body("counter is { n i64 -> { self(n) } }");
        let Expr::Jump { ident, args } = &body[0] else {
            panic!("expected Jump")
        };
        // callee is `self`
        assert!(matches!(ident.inner, Expr::Var("self", _)));
        // argument type is resolved
        let Expr::Var(_, ty) = &args[0].inner else {
            panic!("expected Var arg")
        };
        assert!(
            matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected i64, got {ty:?}"
        );
    }

    #[test]
    fn resolves_var_type_in_regular_call_args() {
        let body = first_branch_body("counter is { n i64 -> { foo(n) } }");
        let Expr::Jump { args, .. } = &body[0] else {
            panic!("expected Jump")
        };
        let Expr::Var(_, ty) = &args[0].inner else {
            panic!("expected Var arg")
        };
        assert!(matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")));
    }

    #[test]
    fn unknown_var_stays_unknown() {
        // `x` is not declared anywhere; its type stays as `{}`.
        let body = first_branch_body("counter is { _ -> { x } }");
        let Expr::Var(name, ty) = &body[0] else {
            panic!("expected Var")
        };
        assert_eq!(*name, "x");
        assert!(is_unknown_ty(ty), "expected {{}} placeholder, got {ty:?}");
    }

    #[test]
    fn scope_does_not_leak_between_branches() {
        // `n` is in scope for the first branch but not the second.
        let src = "counter is { n i64 -> { n } _ -> { n } }";
        let ast = parse_resolve(src);
        let Item::Machine(m) = &ast[0].inner else {
            panic!()
        };
        // first branch: n should be i64
        let MachineItem::Branch(b0) = &m.inner.items[0].inner else {
            panic!()
        };
        let Expr::Var(_, ty0) = &b0.body[0].inner else {
            panic!()
        };
        assert!(matches!(ty0, Type::Named(i) if i.inner == Ident::new("i64")));

        // second branch: n is not declared, stays {}
        let MachineItem::Branch(b1) = &m.inner.items[1].inner else {
            panic!()
        };
        let Expr::Var(_, ty1) = &b1.body[0].inner else {
            panic!()
        };
        assert!(
            is_unknown_ty(ty1),
            "expected {{}} in second branch, got {ty1:?}"
        );
    }

    fn is_unknown_ty(ty: &Type<'_>) -> bool {
        super::is_unknown(ty)
    }

    #[test]
    fn resolves_var_in_wait_handler() {
        // `n` from the outer branch pattern should be accessible inside wait handler body
        let ast = parse_resolve("counter is { n i64 -> { wait { @inc amount i64 -> { n } } } }");
        let Item::Machine(m) = &ast[0].inner else {
            panic!()
        };
        let MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!()
        };
        let Expr::Wait { handlers } = &b.body[0].inner else {
            panic!("expected Wait")
        };
        // `n` inside handler body should be resolved to i64
        let Expr::Var(_, ty) = &handlers[0].inner.body[0].inner else {
            panic!("expected Var")
        };
        assert!(
            matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected i64, got {ty:?}"
        );
    }
}
