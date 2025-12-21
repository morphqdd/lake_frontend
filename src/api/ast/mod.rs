use chumsky::span::Spanned;

use crate::api::expr::Expr;

#[derive(Debug, PartialEq, PartialOrd)]
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
}

#[derive(Debug, PartialEq, PartialOrd)]
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
}

#[derive(Debug, PartialEq, PartialOrd)]
pub struct Ident<'src>(&'src str);
impl<'src> Ident<'src> {
    pub fn new(inner: &'src str) -> Ident<'src> {
        Self(inner)
    }
}

#[derive(Debug, PartialEq, PartialOrd)]
pub enum Type<'src> {
    Path(Spanned<Ident<'src>>, Spanned<Box<Type<'src>>>),
    Type(Spanned<Ident<'src>>),
}
