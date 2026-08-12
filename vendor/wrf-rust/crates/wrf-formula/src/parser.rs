use crate::ast::{Assignment, BinaryOp, Expr, ExprKind, Program, UnaryOp};
use crate::error::{ErrorKind, FormulaError, FormulaResult, Span};
use crate::lexer::{Token, TokenKind};
use crate::model::ResourceLimits;

pub(crate) fn parse(tokens: Vec<Token>, limits: &ResourceLimits) -> FormulaResult<Program> {
    Parser {
        tokens,
        cursor: 0,
        nesting: 0,
        limits,
    }
    .program()
}

struct Parser<'a> {
    tokens: Vec<Token>,
    cursor: usize,
    nesting: usize,
    limits: &'a ResourceLimits,
}

impl Parser<'_> {
    fn program(mut self) -> FormulaResult<Program> {
        self.skip_semis();
        let mut assignments = Vec::new();
        let mut output = None;
        while !self.is(&TokenKind::Eof) {
            if output.is_some() {
                return Err(FormulaError::new(
                    ErrorKind::Parse,
                    "the final expression must be the last statement",
                )
                .at(self.current().span));
            }
            if self.is_assignment_start() {
                if assignments.len() >= self.limits.max_assignments {
                    return Err(FormulaError::new(
                        ErrorKind::Limit,
                        format!(
                            "formula exceeds assignment limit {}",
                            self.limits.max_assignments
                        ),
                    )
                    .at(self.current().span));
                }
                let name_token = self.advance().clone();
                let name = match name_token.kind {
                    TokenKind::Identifier(value) => value,
                    _ => {
                        return Err(FormulaError::new(
                            ErrorKind::Internal,
                            "assignment parser lost identifier",
                        ));
                    }
                };
                self.advance();
                let value = self.expression()?;
                let span = name_token.span.join(value.span);
                assignments.push(Assignment { name, value, span });
                if !self.is(&TokenKind::Semi) && !self.is(&TokenKind::Eof) {
                    return Err(FormulaError::new(
                        ErrorKind::Parse,
                        "expected a newline or ';' after assignment (implicit multiplication is not supported)",
                    )
                    .at(self.current().span));
                }
            } else {
                output = Some(self.expression()?);
                if !self.is(&TokenKind::Semi) && !self.is(&TokenKind::Eof) {
                    return Err(FormulaError::new(
                        ErrorKind::Parse,
                        "unexpected token after expression (implicit multiplication is not supported)",
                    )
                    .at(self.current().span));
                }
            }
            self.skip_semis();
        }
        let output = output.ok_or_else(|| {
            FormulaError::new(ErrorKind::Parse, "formula requires a final expression")
                .at(self.current().span)
        })?;
        let program = Program {
            assignments,
            output,
        };
        let (nodes, depth) = measure_program(&program);
        if nodes > self.limits.max_ast_nodes {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "formula has {nodes} AST nodes; limit is {}",
                    self.limits.max_ast_nodes
                ),
            ));
        }
        if depth > self.limits.max_ast_depth {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "formula AST depth is {depth}; limit is {}",
                    self.limits.max_ast_depth
                ),
            ));
        }
        Ok(program)
    }

    fn expression(&mut self) -> FormulaResult<Expr> {
        self.logical_or()
    }

    fn logical_or(&mut self) -> FormulaResult<Expr> {
        let mut expr = self.logical_and()?;
        while self.take(&TokenKind::Or).is_some() {
            self.skip_inner_semis();
            let right = self.logical_and()?;
            let span = expr.span.join(right.span);
            expr = self.make_binary(BinaryOp::Or, expr, right, span)?;
        }
        Ok(expr)
    }

    fn logical_and(&mut self) -> FormulaResult<Expr> {
        let mut expr = self.comparison()?;
        while self.take(&TokenKind::And).is_some() {
            self.skip_inner_semis();
            let right = self.comparison()?;
            let span = expr.span.join(right.span);
            expr = self.make_binary(BinaryOp::And, expr, right, span)?;
        }
        Ok(expr)
    }

    fn comparison(&mut self) -> FormulaResult<Expr> {
        let mut expr = self.additive()?;
        let mut compared = false;
        loop {
            let op = if self.take(&TokenKind::Eq).is_some() {
                Some(BinaryOp::Eq)
            } else if self.take(&TokenKind::NotEq).is_some() {
                Some(BinaryOp::NotEq)
            } else if self.take(&TokenKind::LessEq).is_some() {
                Some(BinaryOp::LessEq)
            } else if self.take(&TokenKind::Less).is_some() {
                Some(BinaryOp::Less)
            } else if self.take(&TokenKind::GreaterEq).is_some() {
                Some(BinaryOp::GreaterEq)
            } else if self.take(&TokenKind::Greater).is_some() {
                Some(BinaryOp::Greater)
            } else {
                None
            };
            let Some(op) = op else { break };
            if compared {
                return Err(FormulaError::new(
                    ErrorKind::Parse,
                    "chained comparisons are ambiguous; combine comparisons explicitly with 'and'",
                )
                .at(self.current().span));
            }
            compared = true;
            self.skip_inner_semis();
            let right = self.additive()?;
            let span = expr.span.join(right.span);
            expr = self.make_binary(op, expr, right, span)?;
        }
        Ok(expr)
    }

    fn additive(&mut self) -> FormulaResult<Expr> {
        let mut expr = self.multiplicative()?;
        loop {
            let op = if self.take(&TokenKind::Plus).is_some() {
                Some(BinaryOp::Add)
            } else if self.take(&TokenKind::Minus).is_some() {
                Some(BinaryOp::Sub)
            } else {
                None
            };
            let Some(op) = op else { break };
            self.skip_inner_semis();
            let right = self.multiplicative()?;
            let span = expr.span.join(right.span);
            expr = self.make_binary(op, expr, right, span)?;
        }
        Ok(expr)
    }

    fn multiplicative(&mut self) -> FormulaResult<Expr> {
        let mut expr = self.unary()?;
        loop {
            let op = if self.take(&TokenKind::Star).is_some() {
                Some(BinaryOp::Mul)
            } else if self.take(&TokenKind::Slash).is_some() {
                Some(BinaryOp::Div)
            } else {
                None
            };
            let Some(op) = op else { break };
            self.skip_inner_semis();
            let right = self.unary()?;
            let span = expr.span.join(right.span);
            expr = self.make_binary(op, expr, right, span)?;
        }
        Ok(expr)
    }

    /// Power binds more tightly than unary minus: `-2^2` is `-(2^2)` and
    /// `2^-2` is valid. The canonical plan makes this convention explicit.
    fn unary(&mut self) -> FormulaResult<Expr> {
        let token = self.current().clone();
        let op = if self.take(&TokenKind::Minus).is_some() {
            Some(UnaryOp::Neg)
        } else if self.take(&TokenKind::Plus).is_some() {
            Some(UnaryOp::Pos)
        } else if self.take(&TokenKind::Not).is_some() {
            Some(UnaryOp::Not)
        } else {
            None
        };
        if let Some(op) = op {
            self.enter_nesting(token.span)?;
            self.skip_inner_semis();
            let parsed = self.unary();
            self.nesting -= 1;
            let value = parsed?;
            let span = token.span.join(value.span);
            self.make_unary(op, value, span)
        } else {
            self.power()
        }
    }

    fn power(&mut self) -> FormulaResult<Expr> {
        let left = self.primary()?;
        if self.take(&TokenKind::Caret).is_some() {
            self.enter_nesting(left.span)?;
            self.skip_inner_semis();
            let parsed = self.unary();
            self.nesting -= 1;
            let right = parsed?;
            let span = left.span.join(right.span);
            self.make_binary(BinaryOp::Pow, left, right, span)
        } else {
            Ok(left)
        }
    }

    fn primary(&mut self) -> FormulaResult<Expr> {
        let token = self.advance().clone();
        match token.kind {
            TokenKind::Number(value) => Ok(self.make_leaf(ExprKind::Number(value), token.span)),
            TokenKind::Text(value) => Ok(self.make_leaf(ExprKind::Text(value), token.span)),
            TokenKind::Identifier(name) => {
                if self.take(&TokenKind::LeftParen).is_some() {
                    self.enter_nesting(token.span)?;
                    let result = self.call(name, token.span);
                    self.nesting -= 1;
                    result
                } else {
                    Ok(self.make_leaf(ExprKind::Identifier(name), token.span))
                }
            }
            TokenKind::LeftParen => {
                self.enter_nesting(token.span)?;
                self.skip_inner_semis();
                let parsed = self.expression();
                self.skip_inner_semis();
                self.nesting -= 1;
                let mut expr = parsed?;
                let close =
                    self.expect(&TokenKind::RightParen, "expected ')' to close expression")?;
                expr.span = token.span.join(close.span);
                Ok(expr)
            }
            _ => Err(FormulaError::new(
                ErrorKind::Parse,
                "expected a number, identifier, function call, string, or '('",
            )
            .at(token.span)),
        }
    }

    fn call(&mut self, name: String, start: Span) -> FormulaResult<Expr> {
        let mut args = Vec::new();
        self.skip_inner_semis();
        if !self.is(&TokenKind::RightParen) {
            loop {
                if args.len() >= self.limits.max_function_arity {
                    return Err(FormulaError::new(
                        ErrorKind::Limit,
                        format!(
                            "function call exceeds arity limit {}",
                            self.limits.max_function_arity
                        ),
                    )
                    .at(start));
                }
                args.push(self.expression()?);
                self.skip_inner_semis();
                if self.take(&TokenKind::Comma).is_none() {
                    break;
                }
                self.skip_inner_semis();
            }
        }
        let close = self.expect(
            &TokenKind::RightParen,
            "expected ')' after function arguments",
        )?;
        self.make_call(name, args, start.join(close.span))
    }

    fn is_assignment_start(&self) -> bool {
        matches!(self.current().kind, TokenKind::Identifier(_))
            && self
                .tokens
                .get(self.cursor + 1)
                .is_some_and(|token| matches!(token.kind, TokenKind::Assign))
    }

    fn enter_nesting(&mut self, span: Span) -> FormulaResult<()> {
        self.nesting += 1;
        if self.nesting > self.limits.max_ast_depth {
            self.nesting -= 1;
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!("formula nesting exceeds {}", self.limits.max_ast_depth),
            )
            .at(span));
        }
        Ok(())
    }

    fn make_leaf(&self, kind: ExprKind, span: Span) -> Expr {
        Expr {
            kind,
            span,
            nodes: 1,
            depth: 1,
        }
    }

    fn make_unary(&self, op: UnaryOp, value: Expr, span: Span) -> FormulaResult<Expr> {
        let nodes = value.nodes.checked_add(1).ok_or_else(|| {
            FormulaError::new(ErrorKind::Limit, "AST node count overflow").at(span)
        })?;
        let depth = value
            .depth
            .checked_add(1)
            .ok_or_else(|| FormulaError::new(ErrorKind::Limit, "AST depth overflow").at(span))?;
        self.check_expr_budget(nodes, depth, span)?;
        Ok(Expr {
            kind: ExprKind::Unary {
                op,
                value: Box::new(value),
            },
            span,
            nodes,
            depth,
        })
    }

    fn make_binary(
        &self,
        op: BinaryOp,
        left: Expr,
        right: Expr,
        span: Span,
    ) -> FormulaResult<Expr> {
        let nodes = left
            .nodes
            .checked_add(right.nodes)
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| {
                FormulaError::new(ErrorKind::Limit, "AST node count overflow").at(span)
            })?;
        let depth =
            left.depth.max(right.depth).checked_add(1).ok_or_else(|| {
                FormulaError::new(ErrorKind::Limit, "AST depth overflow").at(span)
            })?;
        self.check_expr_budget(nodes, depth, span)?;
        Ok(Expr {
            kind: ExprKind::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
            span,
            nodes,
            depth,
        })
    }

    fn make_call(&self, name: String, args: Vec<Expr>, span: Span) -> FormulaResult<Expr> {
        let mut nodes = 1_usize;
        let mut depth = 1_usize;
        for arg in &args {
            nodes = nodes.checked_add(arg.nodes).ok_or_else(|| {
                FormulaError::new(ErrorKind::Limit, "AST node count overflow").at(span)
            })?;
            depth = depth.max(arg.depth.checked_add(1).ok_or_else(|| {
                FormulaError::new(ErrorKind::Limit, "AST depth overflow").at(span)
            })?);
        }
        self.check_expr_budget(nodes, depth, span)?;
        Ok(Expr {
            kind: ExprKind::Call { name, args },
            span,
            nodes,
            depth,
        })
    }

    fn check_expr_budget(&self, nodes: usize, depth: usize, span: Span) -> FormulaResult<()> {
        if nodes > self.limits.max_ast_nodes {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!("expression exceeds {} AST nodes", self.limits.max_ast_nodes),
            )
            .at(span));
        }
        if depth > self.limits.max_ast_depth {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!("expression exceeds AST depth {}", self.limits.max_ast_depth),
            )
            .at(span));
        }
        Ok(())
    }

    fn skip_semis(&mut self) {
        while self.take(&TokenKind::Semi).is_some() {}
    }

    fn skip_inner_semis(&mut self) {
        if self.nesting > 0 {
            self.skip_semis();
        }
    }

    fn current(&self) -> &Token {
        // Lexer always supplies EOF, and advance never moves past it.
        &self.tokens[self.cursor]
    }

    fn advance(&mut self) -> &Token {
        let index = self.cursor;
        if !matches!(self.tokens[index].kind, TokenKind::Eof) {
            self.cursor += 1;
        }
        &self.tokens[index]
    }

    fn is(&self, kind: &TokenKind) -> bool {
        same_variant(&self.current().kind, kind)
    }

    fn take(&mut self, kind: &TokenKind) -> Option<Token> {
        if self.is(kind) {
            Some(self.advance().clone())
        } else {
            None
        }
    }

    fn expect(&mut self, kind: &TokenKind, message: &str) -> FormulaResult<Token> {
        self.take(kind)
            .ok_or_else(|| FormulaError::new(ErrorKind::Parse, message).at(self.current().span))
    }
}

fn same_variant(left: &TokenKind, right: &TokenKind) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

pub(crate) fn measure_program(program: &Program) -> (usize, usize) {
    let mut nodes = 1_usize;
    let mut depth = 1_usize;
    for assignment in &program.assignments {
        nodes = nodes
            .saturating_add(assignment.value.nodes)
            .saturating_add(1);
        depth = depth.max(assignment.value.depth.saturating_add(1));
    }
    (
        nodes.saturating_add(program.output.nodes),
        depth.max(program.output.depth.saturating_add(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    #[test]
    fn power_binds_tighter_than_unary_minus() {
        let limits = ResourceLimits::default();
        let program = parse(lex("-2^2", &limits).unwrap(), &limits).unwrap();
        match program.output.kind {
            ExprKind::Unary {
                op: UnaryOp::Neg, ..
            } => {}
            other => panic!("unexpected AST: {other:?}"),
        }
    }

    #[test]
    fn rejects_implicit_multiplication() {
        let limits = ResourceLimits::default();
        assert!(parse(lex("2 x", &limits).unwrap(), &limits).is_err());
    }

    #[test]
    fn parses_assignments_and_final_expression() {
        let limits = ResourceLimits::default();
        let program = parse(lex("a = 2\nb = a + 3\nb * 4", &limits).unwrap(), &limits).unwrap();
        assert_eq!(program.assignments.len(), 2);
    }

    #[test]
    fn rejects_chained_comparison() {
        let limits = ResourceLimits::default();
        assert!(parse(lex("1 < x < 3", &limits).unwrap(), &limits).is_err());
    }

    #[test]
    fn accepts_multiline_calls() {
        let limits = ResourceLimits::default();
        let source = "where(\n x > 0,\n x,\n 0\n)";
        assert!(parse(lex(source, &limits).unwrap(), &limits).is_ok());
    }
}
