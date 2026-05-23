use std::{cell::RefCell, hash::Hash, rc::Rc};

use chumsky::span::Spanned;

use crate::api::expr::Expr;

pub trait Clean<T> {
    fn clean(&self) -> T;
}

#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct Ident<'src>(pub &'src str);

impl<'src> Ident<'src> {
    pub fn new(inner: &'src str) -> Self {
        Self(inner)
    }

    pub fn as_str(&self) -> &'src str {
        self.0
    }
}

impl std::fmt::Display for Ident<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub enum Type<'src> {
    /// Simple named type: `i32`, `str`
    Named(Spanned<Ident<'src>>),
    /// Generic type: `box(T)`, `result({})`
    Generic(Spanned<Ident<'src>>, Vec<Spanned<Type<'src>>>),
    /// Module-qualified type: `core:io:writer`
    Path(Spanned<Ident<'src>>, Spanned<Box<Type<'src>>>),
    /// Empty struct / unit: `{}`
    Unit,
    /// Anonymous struct: `{ str }`, `{ i64 usize }`
    Struct(Vec<Spanned<Type<'src>>>),
    /// Placeholder used by the parser for variable-reference types that the
    /// resolver has not yet filled in.  Distinct from [`Type::Unit`], which
    /// represents an explicit `{}` in source.
    Unknown,
}

impl<'src> Clean<Ident<'src>> for Type<'src> {
    fn clean(&self) -> Ident<'src> {
        match self {
            Type::Named(spanned) => spanned.inner.clone(),
            _ => panic!("Type is not a Named!"),
        }
    }
}

impl<'src> Clean<(Ident<'src>, Vec<Type<'src>>)> for Type<'src> {
    fn clean(&self) -> (Ident<'src>, Vec<Type<'src>>) {
        match self {
            Type::Generic(ident, generics) => (
                ident.inner.clone(),
                generics.iter().map(|x| x.inner.clone()).collect(),
            ),
            _ => panic!("Type is not a Generic!"),
        }
    }
}

impl<'src> Clean<(Ident<'src>, Box<Type<'src>>)> for Type<'src> {
    fn clean(&self) -> (Ident<'src>, Box<Type<'src>>) {
        match self {
            Type::Path(ident, ty) => (ident.inner.clone(), ty.inner.clone()),
            _ => panic!("Type not a Named!"),
        }
    }
}

impl<'src> Clean<Vec<Type<'src>>> for Type<'src> {
    fn clean(&self) -> Vec<Type<'src>> {
        match self {
            Type::Struct(vec) => vec.iter().map(|x| x.inner.clone()).collect(),
            _ => panic!("Type not a Named!"),
        }
    }
}

impl std::fmt::Display for Type<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Named(ident) => write!(f, "{}", ident.inner),
            Type::Generic(ident, args) => {
                write!(f, "{}(", ident.inner)?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}", arg.inner)?;
                }
                write!(f, ")")
            }
            Type::Path(ident, next) => write!(f, "{}:{}", ident.inner, next.inner),
            Type::Unit => write!(f, "{{}}"),
            Type::Unknown => write!(f, "?"),
            Type::Struct(fields) => {
                write!(f, "{{ ")?;
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}", field.inner)?;
                }
                write!(f, " }}")
            }
        }
    }
}

/// What a [`Pattern`] *means*.  Computed once at construction time so
/// downstream passes never have to re-parse the ident string.
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub enum PatternKind<'src> {
    /// `n i32` — a regular variable binding.
    Var,
    /// `_` — wildcard parameter.
    Wildcard,
    /// `0 i64`, `42 i64` — numeric literal guard.
    NumGuard(i64),
    /// `"hello" str` — string literal guard.  Carries the raw (unquoted)
    /// content; needed because string content is indistinguishable from a
    /// normal ident once the surrounding quotes are stripped.
    StrGuard(&'src str),
}

/// A branch parameter: `n i32`, `_`, `0 i64`, `"hello" str`
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct Pattern<'src> {
    pub ident: Spanned<Ident<'src>>,
    pub ty: Spanned<Type<'src>>,
    pub kind: PatternKind<'src>,
}

impl<'src> Clean<Ident<'src>> for Pattern<'src> {
    fn clean(&self) -> Ident<'src> {
        self.ident.inner.clone()
    }
}

impl<'src> Clean<Type<'src>> for Pattern<'src> {
    fn clean(&self) -> Type<'src> {
        self.ty.inner.clone()
    }
}

impl<'src> Pattern<'src> {
    /// Build a pattern from an ident token.  Auto-detects wildcard `_` and
    /// numeric literal guards by inspecting `ident.inner.0` once; the result
    /// is cached in `kind`.
    pub fn new(ident: Spanned<Ident<'src>>, ty: Spanned<Type<'src>>) -> Self {
        let kind = if ident.inner.0 == "_" {
            PatternKind::Wildcard
        } else if let Ok(n) = crate::api::expr::parse_int_literal(ident.inner.0) {
            PatternKind::NumGuard(n)
        } else {
            PatternKind::Var
        };
        Self { ident, ty, kind }
    }

    /// Build a string-literal guard pattern (the parser uses this when the
    /// pattern came from a `Token::String`).
    pub fn new_string_guard(ident: Spanned<Ident<'src>>, ty: Spanned<Type<'src>>) -> Self {
        let s = ident.inner.0;
        Self {
            ident,
            ty,
            kind: PatternKind::StrGuard(s),
        }
    }

    pub fn is_wildcard(&self) -> bool {
        matches!(self.kind, PatternKind::Wildcard)
    }

    /// Returns true when the pattern is a literal guard (numeric or string).
    pub fn is_literal_guard(&self) -> bool {
        matches!(self.kind, PatternKind::NumGuard(_) | PatternKind::StrGuard(_))
    }

    /// Returns true when this is a string literal guard (e.g. `"hello" str`).
    pub fn is_string_guard(&self) -> bool {
        matches!(self.kind, PatternKind::StrGuard(_))
    }

    /// Returns the i64 guard value if this is a numeric literal guard.
    pub fn guard_i64(&self) -> Option<i64> {
        if let PatternKind::NumGuard(n) = self.kind {
            Some(n)
        } else {
            None
        }
    }

    /// Returns the string guard value if this is a string literal guard.
    pub fn guard_str(&self) -> Option<&'src str> {
        if let PatternKind::StrGuard(s) = self.kind {
            Some(s)
        } else {
            None
        }
    }
}

impl<'a> From<&Pattern<'a>> for Expr<'a> {
    fn from(value: &Pattern<'a>) -> Self {
        Expr::Let {
            ident: value.ident.clone(),
            ty: value.ty.clone(),
            default: None,
        }
    }
}

/// A data field declaration inside a machine: `@ok.T`, `@.raw_box(T)`, `@.{ str }`
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct Field<'src> {
    /// `None` for anonymous fields (`@.type`)
    pub label: Option<Spanned<Ident<'src>>>,
    pub ty: Spanned<Type<'src>>,
}

impl<'src> Field<'src> {
    pub fn new(label: Option<Spanned<Ident<'src>>>, ty: Spanned<Type<'src>>) -> Self {
        Self { label, ty }
    }
}

impl<'src> Clean<Option<Ident<'src>>> for Field<'src> {
    fn clean(&self) -> Option<Ident<'src>> {
        self.label.clone().map(|x| x.inner.clone())
    }
}

impl<'src> Clean<Type<'src>> for Field<'src> {
    fn clean(&self) -> Type<'src> {
        self.ty.inner.clone()
    }
}

/// A process branch inside a machine: `@init _ -> { ... }`
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct Branch<'src> {
    /// Optional `@name` label
    pub label: Option<Spanned<Ident<'src>>>,
    pub patterns: Vec<Spanned<Pattern<'src>>>,
    /// Optional `ret Type` annotation
    pub ret_ty: Option<Spanned<Type<'src>>>,
    pub body: Vec<Spanned<Expr<'src>>>,
}

impl<'src> Clean<Option<Ident<'src>>> for Branch<'src> {
    fn clean(&self) -> Option<Ident<'src>> {
        self.label.clone().map(|x| x.inner.clone())
    }
}

impl<'src> Clean<Vec<Pattern<'src>>> for Branch<'src> {
    fn clean(&self) -> Vec<Pattern<'src>> {
        self.patterns.iter().map(|x| x.inner.clone()).collect()
    }
}

impl<'src> Clean<Option<Type<'src>>> for Branch<'src> {
    fn clean(&self) -> Option<Type<'src>> {
        self.ret_ty.clone().map(|x| x.inner.clone())
    }
}

impl<'src> Clean<Vec<Expr<'src>>> for Branch<'src> {
    fn clean(&self) -> Vec<Expr<'src>> {
        self.body.iter().map(|x| x.inner.clone()).collect()
    }
}
impl<'src> Branch<'src> {
    pub fn new(
        label: Option<Spanned<Ident<'src>>>,
        patterns: Vec<Spanned<Pattern<'src>>>,
        ret_ty: Option<Spanned<Type<'src>>>,
        body: Vec<Spanned<Expr<'src>>>,
    ) -> Self {
        Self {
            label,
            patterns,
            ret_ty,
            body,
        }
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub enum MachineItem<'src> {
    Field(Field<'src>),
    Branch(Branch<'src>),
}

/// Built-in macro attribute: `@rt_st(...)`, `@ffi(...)`, `@rt(...)`
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct Directive<'src> {
    pub name: Spanned<Ident<'src>>,
    pub args: Vec<Spanned<Type<'src>>>,
}

impl<'src> Directive<'src> {
    pub fn new(name: Spanned<Ident<'src>>, args: Vec<Spanned<Type<'src>>>) -> Self {
        Self { name, args }
    }
}

/// Named field in a record declaration: `name Type`.
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct RecordField<'src> {
    pub name: Spanned<Ident<'src>>,
    pub ty: Spanned<Type<'src>>,
}

/// `[pub] record Name { field Type ... }` — nominal record type declaration.
// See docs/state/features/058_record_via_is.md
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct RecordDecl<'src> {
    pub vis: bool,
    pub ident: Spanned<Ident<'src>>,
    pub fields: Vec<Spanned<RecordField<'src>>>,
}

/// A top-level item in a Lake program.  These constructs only appear at the
/// program root, never as values inside an expression body.
#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum Item<'src> {
    /// `+core.io.writer` or `+core.{ a b }`
    Import(Spanned<Rc<RefCell<Import<'src>>>>),
    /// `@rt(name)` / `@ffi(...)` — built-in macro attribute
    Directive(Spanned<Directive<'src>>),
    /// `[pub] name [(generics)] is { ... }` — process / function definition
    Machine(Spanned<Machine<'src>>),
    /// `[pub] const NAME = <literal>` — module-level compile-time constant.
    /// RHS is restricted to literal expressions (Num/String/Bool); inlined at
    /// every use-site, no runtime cost.
    Const(Spanned<ConstDecl<'src>>),
    /// `[pub] record Name { field Type ... }` — nominal record type.
    Record(Spanned<RecordDecl<'src>>),
}

/// `[pub] const NAME = <literal>`.
///
/// `value` is the unevaluated literal expression.  Type is derived from
/// the literal kind (Num → i64, String → str, Bool → bool).  Resolver and
/// typeck refuse non-literal RHS.
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct ConstDecl<'src> {
    pub vis: bool,
    pub ident: Spanned<Ident<'src>>,
    pub value: Spanned<crate::api::expr::Expr<'src>>,
}

impl<'src> ConstDecl<'src> {
    pub fn new(
        vis: bool,
        ident: Spanned<Ident<'src>>,
        value: Spanned<crate::api::expr::Expr<'src>>,
    ) -> Self {
        Self { vis, ident, value }
    }
}

impl Hash for Item<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct Machine<'src> {
    /// Attributes like `@rt`, `@ffi` that precede the machine
    pub attrs: Vec<Spanned<Directive<'src>>>,
    pub vis: bool,
    pub ident: Spanned<Ident<'src>>,
    /// Generic type parameters: `result(T)` → `["T"]`
    pub generics: Vec<Spanned<Ident<'src>>>,
    pub items: Vec<Spanned<MachineItem<'src>>>,
}

impl<'src> Machine<'src> {
    pub fn new(
        attrs: Vec<Spanned<Directive<'src>>>,
        vis: bool,
        ident: Spanned<Ident<'src>>,
        generics: Vec<Spanned<Ident<'src>>>,
        items: Vec<Spanned<MachineItem<'src>>>,
    ) -> Self {
        Self {
            attrs,
            vis,
            ident,
            generics,
            items,
        }
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum Import<'src> {
    /// `+core.io.writer` or `+core.io.writer as wr`.
    ///
    /// `alias` is only meaningful when `next` is `None` (this node is the
    /// leaf — the actual item being imported).  Attaching an alias to a
    /// non-leaf segment is a parse-time check that will surface as a
    /// diagnostic when the resolver populates the registry.
    Import(
        Spanned<Ident<'src>>,
        Option<Spanned<Rc<RefCell<Self>>>>,
        Option<Spanned<Ident<'src>>>,
    ),
    /// `+core.{ box.box result.result }`
    MultiImport(Vec<Spanned<Rc<RefCell<Import<'src>>>>>),
}

impl<'a> Import<'a> {
    pub fn set_next(
        &mut self,
        next: Spanned<Rc<RefCell<Import<'a>>>>,
    ) -> Spanned<Rc<RefCell<Import<'a>>>> {
        match self {
            Import::Import(_, slot, _) => {
                let _ = slot.insert(next.clone());
                next
            }
            Import::MultiImport(_) => panic!("set_next called on MultiImport"),
        }
    }

    /// Attach an alias to this node.  Caller is responsible for only doing
    /// so on a leaf (`next == None`); the resolver enforces this.
    pub fn set_alias(&mut self, alias: Spanned<Ident<'a>>) {
        match self {
            Import::Import(_, _, slot) => {
                *slot = Some(alias);
            }
            Import::MultiImport(_) => panic!("set_alias called on MultiImport"),
        }
    }
}

impl Hash for Import<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}
