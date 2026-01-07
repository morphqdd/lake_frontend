use chumsky::span::Spanned;

use crate::api::expr::Expr;

#[derive(Debug, PartialEq, PartialOrd)]
pub struct Machine<'src> {
    ident: Spanned<Ident<'src>>,
    branches: Vec<Spanned<Branch<'src>>>,
}

impl<'src> Machine<'src> {
    pub fn new(ident: Spanned<Ident<'src>>, branches: Vec<Spanned<Branch<'src>>>) -> Machine<'src> {
        Self { ident, branches }
    }

    pub fn ident(&self) -> Ident<'src> {
        self.ident.inner.clone()
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

#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
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

    pub fn default(&self) -> Option<Expr<'src>> {
        self.default.clone().map(|expr| expr.inner)
    }
}

impl<'a> From<&Pattern<'a>> for Expr<'a> {
    fn from(value: &Pattern<'a>) -> Self {
        Expr::Let {
            ident: value.ident.clone(),
            ty: value.ty.clone(),
            default: value.default.clone().map(|x| Box::new(x)),
        }
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
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

#[derive(Debug, PartialEq, PartialOrd, Clone, Hash)]
pub enum Type<'src> {
    Path(Spanned<Ident<'src>>, Spanned<Box<Type<'src>>>),
    Type(Spanned<Ident<'src>>),
}

impl ToString for Type<'_> {
    fn to_string(&self) -> String {
        match self {
            Type::Path(ident, next) => format!("{}:{}", ident.inner.to_string(), next.to_string()),
            Type::Type(ident) => format!("{}", ident.to_string()),
        }
    }
}
