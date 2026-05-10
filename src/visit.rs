//! Read-only AST visitor.
//!
//! Centralises the recursive walk used by `semantic` and (optionally) other
//! analysis passes.  Each `visit_*` method calls into a `walk_*` free function
//! that performs the default recursion; implementors override only the
//! methods relevant to their pass.
//!
//! The mutating/transforming variant (resolver-style) is intentionally not
//! provided here — resolver's walk is short, and a generic fold over an AST
//! that owns its lifetimes is significantly more invasive than the read-only
//! case justifies.

use chumsky::span::Spanned;

use crate::api::{
    ast::{Branch, Item, Machine, MachineItem},
    expr::Expr,
};

/// Walks a Lake AST without modifying it.  All `visit_*` methods have a
/// default implementation that delegates to the corresponding `walk_*`
/// function so implementors only need to override the variants they care
/// about.
pub trait Visit<'src> {
    fn visit_program(&mut self, program: &[Spanned<Item<'src>>]) {
        for item in program {
            self.visit_item(item);
        }
    }

    fn visit_item(&mut self, item: &Spanned<Item<'src>>) {
        walk_item(self, item);
    }

    fn visit_machine(&mut self, machine: &Spanned<Machine<'src>>) {
        walk_machine(self, machine);
    }

    fn visit_branch(&mut self, branch: &Branch<'src>) {
        walk_branch(self, branch);
    }

    fn visit_expr(&mut self, expr: &Spanned<Expr<'src>>) {
        walk_expr(self, expr);
    }
}

pub fn walk_item<'src, V: Visit<'src> + ?Sized>(v: &mut V, item: &Spanned<Item<'src>>) {
    match &item.inner {
        Item::Machine(m) => v.visit_machine(m),
        Item::Import(_) | Item::Directive(_) => {}
    }
}

pub fn walk_machine<'src, V: Visit<'src> + ?Sized>(v: &mut V, machine: &Spanned<Machine<'src>>) {
    for mi in &machine.inner.items {
        if let MachineItem::Branch(b) = &mi.inner {
            v.visit_branch(b);
        }
    }
}

pub fn walk_branch<'src, V: Visit<'src> + ?Sized>(v: &mut V, branch: &Branch<'src>) {
    for e in &branch.body {
        v.visit_expr(e);
    }
}

pub fn walk_expr<'src, V: Visit<'src> + ?Sized>(v: &mut V, expr: &Spanned<Expr<'src>>) {
    match &expr.inner {
        Expr::Let { default, .. } => {
            if let Some(d) = default {
                v.visit_expr(d);
            }
        }
        Expr::Jump { ident, args } => {
            v.visit_expr(ident);
            for a in args {
                v.visit_expr(a);
            }
        }
        Expr::MethodCall { receiver, args, .. } => {
            v.visit_expr(receiver);
            for a in args {
                v.visit_expr(a);
            }
        }
        Expr::AtAccess { receiver, .. } | Expr::DotAccess { receiver, .. } => {
            v.visit_expr(receiver);
        }
        Expr::StructInit { base, fields } => {
            v.visit_expr(base);
            for f in fields {
                v.visit_expr(f);
            }
        }
        Expr::When { cond, branches } => {
            v.visit_expr(cond);
            for (pat, body) in branches {
                v.visit_expr(pat);
                for e in body {
                    v.visit_expr(e);
                }
            }
        }
        Expr::Wait { handlers, filter } => {
            for f in filter {
                v.visit_expr(f);
            }
            for h in handlers {
                v.visit_branch(&h.inner);
            }
        }
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Eq(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r) => {
            v.visit_expr(l);
            v.visit_expr(r);
        }
        Expr::Neg(inner) | Expr::Ret(inner) | Expr::Pin(inner) => v.visit_expr(inner),
        Expr::Var(_, _)
        | Expr::Path(_)
        | Expr::Num(_, _)
        | Expr::String(_, _)
        | Expr::Bool(_)
        | Expr::Unit => {}
    }
}
