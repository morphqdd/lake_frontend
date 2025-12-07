use chumsky::span::Spanned;

#[derive(Clone, Debug)]
pub enum Expr<'src> {
    Num(&'src str),
    String(&'src str),
    Var(&'src str),
    Bool(bool),
    Path(Path<'src>),
    Let {
        ident: Spanned<&'src str>,
        ty: Box<Spanned<Self>>,
        default: Option<Box<Spanned<Self>>>,
    },
    Jump {
        ident: Box<Spanned<Self>>,
        args: Vec<Spanned<Self>>,
    },
    Mul(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Div(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Add(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Sub(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Branch {
        var_pattern: Box<Spanned<Self>>,
        body: Box<Spanned<Self>>,
    },
    Machine {
        ident: Spanned<&'src str>,
        branches: Vec<Self>,
    },
}

#[derive(Clone, Debug)]
pub enum Path<'src> {
    Path(Box<Spanned<Self>>),
    Type(Box<Spanned<Expr<'src>>>),
}
