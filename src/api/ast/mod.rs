use chumsky::span::Spanned;

use crate::api::expr::Expr;

#[derive(Debug, PartialEq, PartialOrd)]
pub struct Pattern<'src> {
    ident: Ident<'src>,
    ty: Type<'src>,
    default: Option<Spanned<Expr<'src>>>,
}

impl<'src> Pattern<'src> {
    pub fn new(
        ident: Ident<'src>,
        ty: Type<'src>,
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
    Path(Ident<'src>, Box<Type<'src>>),
    Type(Ident<'src>),
}
