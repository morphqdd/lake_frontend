use chumsky::span::Spanned;

use crate::api::expr::Expr;

#[derive(Debug, PartialEq, PartialOrd)]
pub struct Process<'src> {
    ident: Spanned<Ident<'src>>,
    branches: Vec<Spanned<Branch<'src>>>,
}

impl<'src> Process<'src> {
    pub fn new(ident: Spanned<Ident<'src>>, branches: Vec<Spanned<Branch<'src>>>) -> Process<'src> {
        Self { ident, branches }
    }

    pub fn branches(&self) -> Vec<Branch<'src>> {
        self.branches.iter().map(|x| x.inner.clone()).collect()
    }
}
#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub struct Branch<'src> {
    patterns: Vec<Spanned<Pattern<'src>>>,
    body: Vec<Spanned<Expr<'src>>>,
}

impl<'src> Branch<'src> {
    pub fn new(
        patterns: Vec<Spanned<Pattern<'src>>>,
        body: Vec<Spanned<Expr<'src>>>,
    ) -> Branch<'src> {
        Self { patterns, body }
    }

    pub fn patterns(&self) -> Vec<Pattern<'src>> {
        self.patterns.iter().map(|x| x.inner.clone()).collect()
    }
    pub fn body(&self) -> Vec<Expr<'src>> {
        self.body.iter().map(|x| x.inner.clone()).collect()
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub struct Pattern<'src> {
    ident: Spanned<Ident<'src>>,
    ty: Spanned<Type<'src>>,
    default: Option<Spanned<Expr<'src>>>,
}

impl<'src> Pattern<'src> {
    pub fn new(
        ident: Spanned<Ident<'src>>,
        ty: Spanned<Type<'src>>,
        default: Option<Spanned<Expr<'src>>>,
    ) -> Pattern<'src> {
        Self { ident, ty, default }
    }

    pub fn has_default(&self) -> bool {
        self.default.is_some()
    }

    pub fn ty(&self) -> Type<'src> {
        self.ty.inner.clone()
    }

    pub fn ident(&self) -> Ident<'src> {
        self.ident.inner.clone()
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub struct Ident<'src>(&'src str);
impl<'src> Ident<'src> {
    pub fn new(inner: &'src str) -> Ident<'src> {
        Self(inner)
    }
}

impl ToString for Ident<'_> {
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum Type<'src> {
    Path(Spanned<Ident<'src>>, Spanned<Box<Type<'src>>>),
    Type(Spanned<Ident<'src>>),
}
