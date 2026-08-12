use crate::error::Span;

#[derive(Debug, Clone)]
pub(crate) struct Program {
    pub assignments: Vec<Assignment>,
    pub output: Expr,
}

#[derive(Debug, Clone)]
pub(crate) struct Assignment {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub(crate) struct Expr {
    pub kind: ExprKind,
    pub span: Span,
    pub nodes: usize,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum ExprKind {
    Number(f64),
    Text(String),
    Identifier(String),
    Unary {
        op: UnaryOp,
        value: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Neg,
    Pos,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    And,
    Or,
}
