#[allow(dead_code)]
pub struct Process {
    ident: String,
    branches: Vec<Branch>,
}

#[allow(dead_code)]
pub struct Branch {
    ty: BranchType,
    variables: Vec<Variable>,
}

pub enum BranchType {
    Action,
    Constructor,
}

#[derive(Debug, PartialEq, PartialOrd)]
pub struct Variable {
    ident: Ident,
    ty: Path,
    default: Option<Literal>,
}

impl Variable {
    pub fn new(ident: Ident, ty: Path, default: Option<Literal>) -> Self {
        Self { ident, ty, default }
    }
}

#[derive(Debug, PartialEq, PartialOrd)]
pub enum Literal {
    Number(String),
    String(String),
}

#[derive(Debug, PartialEq, PartialOrd)]
pub struct Ident(String);
impl Ident {
    pub fn new(n: &str) -> Self {
        Self(n.to_string())
    }
}

#[derive(Debug, PartialEq, PartialOrd)]
pub enum Path {
    Path(Box<Path>),
    Type(Ident),
}
