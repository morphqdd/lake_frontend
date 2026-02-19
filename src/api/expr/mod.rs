use std::{cell::RefCell, hash::Hash, rc::Rc};

use chumsky::span::Spanned;

use crate::api::ast::{Branch, Directive, Ident, Import, Machine, Type};

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Expr<'src> {
    // ── Literals ──────────────────────────────────────────────────────────
    Num(&'src str, Type<'src>),
    String(&'src str, Type<'src>),
    Bool(bool),
    /// Empty struct literal `{}`
    Unit,

    // ── Variables & paths ─────────────────────────────────────────────────
    Var(&'src str, Type<'src>),

    // ── Binding ───────────────────────────────────────────────────────────
    Let {
        ident: Spanned<Ident<'src>>,
        ty: Spanned<Type<'src>>,
        default: Option<Box<Spanned<Self>>>,
    },

    // ── Calls & access ────────────────────────────────────────────────────
    /// `func(args)` or `self(args)`
    Jump {
        ident: Box<Spanned<Self>>,
        args: Vec<Spanned<Self>>,
    },
    /// `receiver@method(args)` — call via `@`
    MethodCall {
        receiver: Box<Spanned<Self>>,
        method: Spanned<Ident<'src>>,
        args: Vec<Spanned<Self>>,
    },
    /// `receiver@field` — access via `@` without call
    AtAccess {
        receiver: Box<Spanned<Self>>,
        field: Spanned<Ident<'src>>,
    },
    /// `receiver.field` — dot access
    DotAccess {
        receiver: Box<Spanned<Self>>,
        field: Spanned<Ident<'src>>,
    },
    /// `receiver.{ fields }` — struct init
    StructInit {
        base: Box<Spanned<Self>>,
        fields: Vec<Spanned<Self>>,
    },

    // ── Pattern matching ──────────────────────────────────────────────────
    /// `when expr { pat -> { body } ... }`
    When {
        cond: Box<Spanned<Self>>,
        branches: Vec<(Spanned<Self>, Vec<Spanned<Self>>)>,
    },

    // ── Process suspension ──────────────────────────────────────────────
    /// `wait { @label pattern+ -> { body } ... }`
    Wait {
        handlers: Vec<Spanned<Branch<'src>>>,
    },

    // ── Arithmetic ────────────────────────────────────────────────────────
    Mul(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Div(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Add(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Sub(Box<Spanned<Self>>, Box<Spanned<Self>>),

    // ── Comparisons ───────────────────────────────────────────────────────
    Le(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Ge(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Eq(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Lt(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Gt(Box<Spanned<Self>>, Box<Spanned<Self>>),

    // ── Top-level items ───────────────────────────────────────────────────
    Import(Spanned<Rc<RefCell<Import<'src>>>>),
    Machine(Spanned<Machine<'src>>),
    Directive(Spanned<Directive<'src>>),
}

impl Hash for Expr<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}
