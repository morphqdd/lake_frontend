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

/// A branch parameter: `n i32`, `n str."hello"`, `_`
#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub struct Pattern<'src> {
    pub ident: Spanned<Ident<'src>>,
    pub ty: Spanned<Type<'src>>,
    pub default: Option<Spanned<Expr<'src>>>,
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

impl<'src> Clean<Option<Expr<'src>>> for Pattern<'src> {
    fn clean(&self) -> Option<Expr<'src>> {
        self.default.clone().map(|x| x.inner.clone())
    }
}

impl<'src> Pattern<'src> {
    pub fn new(
        ident: Spanned<Ident<'src>>,
        ty: Spanned<Type<'src>>,
        default: Option<Spanned<Expr<'src>>>,
    ) -> Self {
        Self { ident, ty, default }
    }

    pub fn is_wildcard(&self) -> bool {
        self.ident.inner.0 == "_"
    }
}

impl<'a> From<&Pattern<'a>> for Expr<'a> {
    fn from(value: &Pattern<'a>) -> Self {
        Expr::Let {
            ident: value.ident.clone(),
            ty: value.ty.clone(),
            default: value.default.clone().map(Box::new),
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
    /// `+core.io.writer`
    Import(Spanned<Ident<'src>>, Option<Spanned<Rc<RefCell<Self>>>>),
    /// `+core.{ box.box result.result }`
    MultiImport(Vec<Spanned<Rc<RefCell<Import<'src>>>>>),
}

impl<'a> Import<'a> {
    pub fn set_next(
        &mut self,
        next: Spanned<Rc<RefCell<Import<'a>>>>,
    ) -> Spanned<Rc<RefCell<Import<'a>>>> {
        match self {
            Import::Import(_, slot) => {
                let _ = slot.insert(next.clone());
                next
            }
            Import::MultiImport(_) => panic!("set_next called on MultiImport"),
        }
    }
}

impl Hash for Import<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}
