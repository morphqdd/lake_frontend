use std::hash::Hash;

use chumsky::span::Spanned;

use crate::api::ast::{Branch, Ident, Type};

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
    /// Module-qualified value: `core:io:writer`.  Always at least two
    /// segments (a single ident is `Var`).  Used as a callee in `Jump` for
    /// inline cross-module calls without a prior `+import`.
    Path(Vec<Spanned<Ident<'src>>>),

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

    /// `{ a b c }` — anonymous tuple (heap-alloc, positional access via `.N`).
    /// First element may be an atom for tagged-tuple use: `{ :ok 42 }`.
    /// Pattern matching: `when t { { :ok v } -> ... }`.
    Tuple(Vec<Spanned<Self>>),

    /// `:ident` — atom literal (compile-time interned to a small int).
    /// Used as discriminator tag in tagged tuples and pattern-match guards.
    Atom(&'src str),

    /// `t.0`, `t.1`, … — positional access into a tuple value.
    /// Lowered to `load(fat_ptr.start + index * 8)` at codegen.
    TupleIndex {
        receiver: Box<Spanned<Self>>,
        index: usize,
    },

    // ── Pattern matching ──────────────────────────────────────────────────
    /// `when expr { pat -> { body } ... }`
    When {
        cond: Box<Spanned<Self>>,
        branches: Vec<(Spanned<Self>, Vec<Spanned<Self>>)>,
    },

    // ── Process suspension ──────────────────────────────────────────────
    /// `wait <pid_expr>* { @label pattern+ -> { body } ... }`
    ///
    /// `filter` is a list of expressions that evaluate to pids; when
    /// non-empty, the first argument of every received message must be
    /// a pid found in `filter` (OR logic) for any handler to fire.  An
    /// empty `filter` means "accept any sender" (current default
    /// behaviour).
    ///
    /// Used by the `ret`-machine lowering to make synchronous calls
    /// race-free: the caller filters on the spawned process's pid so a
    /// concurrent message from a different sender can't be mistaken for
    /// the awaited reply.
    Wait {
        handlers: Vec<Spanned<Branch<'src>>>,
        filter: Vec<Spanned<Self>>,
    },

    /// `ret <expr>` — early return from a `-> ret <type>` branch.
    /// Lowered to a call to the implicit `__caller` pid before
    /// codegen sees it; persists in the AST only between parse and
    /// the lowering pass.
    Ret(Box<Spanned<Self>>),

    /// `pin <expr>` — sync sugar for a ret-machine call.  Lowered to
    /// a fresh `let __pin_<id> = <expr>` so the existing
    /// let-with-ret-target rewrite turns it into spawn + wait.
    /// Non-ret callees pass through as ordinary lets.
    Pin(Box<Spanned<Self>>),

    // ── Arithmetic ────────────────────────────────────────────────────────
    Mul(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Div(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Add(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Sub(Box<Spanned<Self>>, Box<Spanned<Self>>),
    /// Unary numeric negation: `-x` or `-7`.
    Neg(Box<Spanned<Self>>),

    // ── Comparisons ───────────────────────────────────────────────────────
    Le(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Ge(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Eq(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Lt(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Gt(Box<Spanned<Self>>, Box<Spanned<Self>>),

}

impl Hash for Expr<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}
