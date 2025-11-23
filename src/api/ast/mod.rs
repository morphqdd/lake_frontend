pub struct Process {
    ident: String,
    branches: Vec<Branch>,
}

pub struct Branch {
    ty: BranchType,
    variables: Vec<Variable>,
}

pub enum BranchType {
    Action,
    Constructor,
}

pub struct Variable {
    ident: Ident,
    ty: Path,
    default: Option<Value>,
}

pub enum Value {
    Int(i32),
    String(String),
    Bool(bool),
}

#[derive(Debug, PartialEq, PartialOrd)]
pub struct Ident(String);
impl Ident {
    pub fn new(n: &str) -> Self {
        Self(n.to_string())
    }
}
pub enum Path {
    Path(Box<Path>),
    Type { ty_ident: String },
}
