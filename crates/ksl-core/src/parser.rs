use crate::ast::*;
use crate::error::{ParseError, Span};
use crate::token::{Token, TokenKind};

pub fn parse(tokens: &[Token]) -> Result<Program, ParseError> {
    let mut p = Parser { tokens, pos: 0 };
    p.parse_program()
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        self.tokens
            .get(self.pos + offset)
            .map(|t| &t.kind)
            .unwrap_or(&TokenKind::Eof)
    }

    fn span(&self) -> Span {
        self.peek().span
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(self.peek_kind()) == std::mem::discriminant(kind)
    }

    fn matches(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<Token, ParseError> {
        if self.check(&kind) {
            Ok(self.advance())
        } else {
            Err(self.err(format!(
                "expected {what}, found {:?}",
                self.peek_kind()
            )))
        }
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            span: self.span(),
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek_kind(), TokenKind::Newline | TokenKind::Semicolon) {
            self.advance();
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        Ok(self.expect_ident_spanned()?.0)
    }

    /// Like `expect_ident`, but also returns the identifier token's own
    /// span (not the caller's `self.span()` at some other position) — used
    /// where the exact name span matters for hover/goto-definition.
    fn expect_ident_spanned(&mut self) -> Result<(String, Span), ParseError> {
        let span = self.span();
        match self.peek_kind().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok((name, span))
            }
            other => Err(self.err(format!("expected identifier, found {other:?}"))),
        }
    }

    // ---- program / statements ----

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let imports = self.parse_imports()?;
        let mut stmts = Vec::new();
        self.skip_separators();
        while !matches!(self.peek_kind(), TokenKind::Eof) {
            stmts.push(self.parse_stmt()?);
            self.skip_separators();
        }
        Ok(Program { imports, stmts })
    }

    /// Consumes a leading run of `@file:Import(...)` declarations. Stops at
    /// the first non-`@` token, so any `@` appearing later in the file is
    /// left for `parse_stmt`/`parse_primary` to reject as an unexpected
    /// token — file annotations are only legal before any other statement.
    fn parse_imports(&mut self) -> Result<Vec<ImportDecl>, ParseError> {
        let mut imports = Vec::new();
        self.skip_separators();
        while matches!(self.peek_kind(), TokenKind::At) {
            imports.push(self.parse_file_import()?);
            self.skip_separators();
        }
        Ok(imports)
    }

    fn parse_file_import(&mut self) -> Result<ImportDecl, ParseError> {
        let span = self.span();
        self.expect(TokenKind::At, "'@'")?;
        let head = self.expect_ident()?;
        if head != "file" {
            return Err(self.err(format!("expected 'file' after '@', found '{head}'")));
        }
        self.expect(TokenKind::Colon, "':'")?;
        let name = self.expect_ident()?;
        if name != "Import" {
            return Err(self.err(format!(
                "unknown file annotation '@file:{name}' (only 'Import' is supported)"
            )));
        }
        self.expect(TokenKind::LParen, "'('")?;
        let path = self.parse_plain_string_literal()?;
        self.expect(TokenKind::RParen, "')'")?;
        Ok(ImportDecl { path, span })
    }

    /// A `"..."` string literal with no interpolation — used for import
    /// paths, which must be known without evaluating any expression.
    fn parse_plain_string_literal(&mut self) -> Result<String, ParseError> {
        let start = self.span();
        self.expect(TokenKind::StringStart, "'\"'")?;
        let mut s = String::new();
        loop {
            match self.peek_kind().clone() {
                TokenKind::StringText(text) => {
                    self.advance();
                    s.push_str(&text);
                }
                TokenKind::StringEnd => {
                    self.advance();
                    break;
                }
                TokenKind::StringInterpText(_) | TokenKind::TemplateExprStart => {
                    return Err(self.err(
                        "import path must be a plain string literal (no interpolation)",
                    ));
                }
                other => return Err(self.err(format!("unexpected token in string: {other:?}"))),
            }
        }
        if s.is_empty() {
            return Err(ParseError {
                message: "import path must not be empty".into(),
                span: start,
            });
        }
        Ok(s)
    }

    fn parse_stmts_until_rbrace(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();
        self.skip_separators();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            stmts.push(self.parse_stmt()?);
            self.skip_separators();
        }
        self.expect(TokenKind::RBrace, "'}'")?;
        Ok(stmts)
    }

    fn parse_block_stmts(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect(TokenKind::LBrace, "'{'")?;
        self.parse_stmts_until_rbrace()
    }

    /// Either a `{ ... }` block or a single expression, used for if/when branches.
    fn parse_branch(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if matches!(self.peek_kind(), TokenKind::LBrace) {
            self.parse_block_stmts()
        } else {
            let span = self.span();
            let expr = self.parse_expr()?;
            Ok(vec![Stmt::new(StmtKind::ExprStmt(expr), span)])
        }
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        match self.peek_kind() {
            TokenKind::Val | TokenKind::Var => self.parse_val_decl(),
            TokenKind::Fun => self.parse_fun_decl(),
            TokenKind::While => self.parse_while(),
            TokenKind::For => self.parse_for(),
            TokenKind::Return => {
                self.advance();
                let value = if matches!(
                    self.peek_kind(),
                    TokenKind::Newline | TokenKind::Semicolon | TokenKind::RBrace | TokenKind::Eof
                ) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                Ok(Stmt::new(StmtKind::Return(value), span))
            }
            TokenKind::Break => {
                self.advance();
                Ok(Stmt::new(StmtKind::Break, span))
            }
            TokenKind::Continue => {
                self.advance();
                Ok(Stmt::new(StmtKind::Continue, span))
            }
            _ => self.parse_expr_or_assign_stmt(),
        }
    }

    fn parse_val_decl(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        let mutable = matches!(self.peek_kind(), TokenKind::Var);
        self.advance(); // val/var
        let (name, name_span) = self.expect_ident_spanned()?;
        let type_hint = if self.matches(&TokenKind::Colon) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(TokenKind::Assign, "'='")?;
        let init = self.parse_expr()?;
        Ok(Stmt::new(
            StmtKind::ValDecl {
                name,
                name_span,
                type_hint,
                mutable,
                init,
            },
            span,
        ))
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
        let mut params = Vec::new();
        self.expect(TokenKind::LParen, "'('")?;
        while !matches!(self.peek_kind(), TokenKind::RParen) {
            let (name, name_span) = self.expect_ident_spanned()?;
            let type_hint = if self.matches(&TokenKind::Colon) {
                Some(self.expect_ident()?)
            } else {
                None
            };
            let default = if self.matches(&TokenKind::Assign) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            params.push(Param {
                name,
                name_span,
                type_hint,
                default,
            });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen, "')'")?;
        Ok(params)
    }

    fn parse_fun_decl(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        self.advance(); // fun
        let (name, name_span) = self.expect_ident_spanned()?;
        let params = self.parse_params()?;
        if self.matches(&TokenKind::Colon) {
            self.expect_ident()?; // return type, parsed but not checked in the MVP
        }
        let body = if self.matches(&TokenKind::Assign) {
            FunBody::Expr(Box::new(self.parse_expr()?))
        } else {
            FunBody::Block(self.parse_block_stmts()?)
        };
        Ok(Stmt::new(StmtKind::FunDecl { name, name_span, params, body }, span))
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        self.advance(); // while
        self.expect(TokenKind::LParen, "'('")?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen, "')'")?;
        let body = self.parse_block_stmts()?;
        Ok(Stmt::new(StmtKind::While { cond, body }, span))
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        self.advance(); // for
        self.expect(TokenKind::LParen, "'('")?;
        let (var_name, var_name_span) = self.expect_ident_spanned()?;
        self.expect(TokenKind::In, "'in'")?;
        let iterable = self.parse_expr()?;
        self.expect(TokenKind::RParen, "')'")?;
        let body = self.parse_block_stmts()?;
        Ok(Stmt::new(
            StmtKind::For {
                var_name,
                var_name_span,
                iterable,
                body,
            },
            span,
        ))
    }

    fn parse_expr_or_assign_stmt(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        let expr = self.parse_expr()?;
        let op = match self.peek_kind() {
            TokenKind::Assign => Some(AssignOp::Set),
            TokenKind::PlusAssign => Some(AssignOp::Add),
            TokenKind::MinusAssign => Some(AssignOp::Sub),
            TokenKind::StarAssign => Some(AssignOp::Mul),
            TokenKind::SlashAssign => Some(AssignOp::Div),
            TokenKind::PercentAssign => Some(AssignOp::Rem),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let name = match expr.kind {
                ExprKind::Name(name) => name,
                _ => return Err(self.err("invalid assignment target")),
            };
            let value = self.parse_expr()?;
            Ok(Stmt::new(StmtKind::Assign { name, op, value }, span))
        } else {
            Ok(Stmt::new(StmtKind::ExprStmt(expr), span))
        }
    }

    // ---- expressions (Pratt / precedence climbing) ----

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek_kind(), TokenKind::OrOr) {
            let span = self.span();
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::new(
                ExprKind::Binary {
                    op: BinaryOp::Or,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_equality()?;
        while matches!(self.peek_kind(), TokenKind::AndAnd) {
            let span = self.span();
            self.advance();
            let rhs = self.parse_equality()?;
            lhs = Expr::new(
                ExprKind::Binary {
                    op: BinaryOp::And,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Eq => BinaryOp::Eq,
                TokenKind::NotEq => BinaryOp::NotEq,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_comparison()?;
            lhs = Expr::new(
                ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_range()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Lt => BinaryOp::Lt,
                TokenKind::Gt => BinaryOp::Gt,
                TokenKind::LtEq => BinaryOp::LtEq,
                TokenKind::GtEq => BinaryOp::GtEq,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_range()?;
            lhs = Expr::new(
                ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn parse_range(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_additive()?;
        if matches!(self.peek_kind(), TokenKind::Range) {
            let span = self.span();
            self.advance();
            let rhs = self.parse_additive()?;
            return Ok(Expr::new(
                ExprKind::Range {
                    start: Box::new(lhs),
                    end: Box::new(rhs),
                },
                span,
            ));
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::new(
                ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Rem,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::new(
                ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        let span = self.span();
        let op = match self.peek_kind() {
            TokenKind::Minus => Some(UnaryOp::Neg),
            TokenKind::Plus => Some(UnaryOp::Pos),
            TokenKind::Not => Some(UnaryOp::Not),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let expr = self.parse_unary()?;
            Ok(Expr::new(
                ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                span,
            ))
        } else {
            self.parse_postfix()
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_primary()?;
        loop {
            if matches!(self.peek_kind(), TokenKind::LParen) {
                let span = self.span();
                let args = self.parse_call_args()?;
                expr = Expr::new(
                    ExprKind::Call {
                        callee: Box::new(expr),
                        args,
                    },
                    span,
                );
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_call_args(&mut self) -> Result<Vec<Arg>, ParseError> {
        self.expect(TokenKind::LParen, "'('")?;
        let mut args = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RParen) {
            let name = if matches!(self.peek_kind(), TokenKind::Ident(_))
                && matches!(self.peek_at(1), TokenKind::Assign)
            {
                let n = self.expect_ident()?;
                self.advance(); // '='
                Some(n)
            } else {
                None
            };
            let value = self.parse_expr()?;
            args.push(Arg { name, value });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen, "')'")?;
        Ok(args)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let span = self.span();
        match self.peek_kind().clone() {
            TokenKind::Int(v) => {
                self.advance();
                Ok(Expr::new(ExprKind::IntLit(v), span))
            }
            TokenKind::Double(v) => {
                self.advance();
                Ok(Expr::new(ExprKind::DoubleLit(v), span))
            }
            TokenKind::Bool(v) => {
                self.advance();
                Ok(Expr::new(ExprKind::BoolLit(v), span))
            }
            TokenKind::Ident(name) => {
                self.advance();
                Ok(Expr::new(ExprKind::Name(name), span))
            }
            TokenKind::StringStart => self.parse_string_template(),
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                self.expect(TokenKind::RParen, "')'")?;
                Ok(inner)
            }
            TokenKind::LBrace => self.parse_lambda(),
            TokenKind::If => self.parse_if_expr(),
            TokenKind::When => self.parse_when_expr(),
            other => Err(self.err(format!("unexpected token {other:?}"))),
        }
    }

    fn parse_string_template(&mut self) -> Result<Expr, ParseError> {
        let span = self.span();
        self.expect(TokenKind::StringStart, "'\"'")?;
        let mut parts = Vec::new();
        loop {
            match self.peek_kind().clone() {
                TokenKind::StringText(text) => {
                    self.advance();
                    parts.push(StringPart::Text(text));
                }
                TokenKind::StringInterpText(name) => {
                    let s = self.span();
                    self.advance();
                    parts.push(StringPart::Expr(Box::new(Expr::new(ExprKind::Name(name), s))));
                }
                TokenKind::TemplateExprStart => {
                    self.advance();
                    let inner = self.parse_expr()?;
                    self.expect(TokenKind::TemplateExprEnd, "'}'")?;
                    parts.push(StringPart::Expr(Box::new(inner)));
                }
                TokenKind::StringEnd => {
                    self.advance();
                    break;
                }
                other => return Err(self.err(format!("unexpected token in string: {other:?}"))),
            }
        }
        Ok(Expr::new(ExprKind::StringTemplate(parts), span))
    }

    fn parse_lambda(&mut self) -> Result<Expr, ParseError> {
        let span = self.span();
        self.expect(TokenKind::LBrace, "'{'")?;

        let params = self.try_parse_lambda_params();

        let body = self.parse_stmts_until_rbrace()?;
        Ok(Expr::new(ExprKind::Lambda { params, body }, span))
    }

    /// Speculatively parses a `ident (, ident)* ->` parameter list at the
    /// start of a lambda body. Restores position and returns an empty list
    /// if the pattern doesn't match (the lambda then uses the implicit `it`).
    fn try_parse_lambda_params(&mut self) -> Vec<Param> {
        let start = self.pos;
        let mut params = Vec::new();
        loop {
            let name_span = self.span();
            match self.peek_kind().clone() {
                TokenKind::Ident(name) => {
                    self.advance();
                    params.push(Param {
                        name,
                        name_span,
                        type_hint: None,
                        default: None,
                    });
                }
                _ => {
                    self.pos = start;
                    return Vec::new();
                }
            }
            if self.matches(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        if self.matches(&TokenKind::Arrow) {
            params
        } else {
            self.pos = start;
            Vec::new()
        }
    }

    fn parse_if_expr(&mut self) -> Result<Expr, ParseError> {
        let span = self.span();
        self.advance(); // if
        self.expect(TokenKind::LParen, "'('")?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen, "')'")?;
        let then_branch = self.parse_branch()?;
        self.skip_separators_if_else_follows();
        let else_branch = if self.matches(&TokenKind::Else) {
            Some(self.parse_branch()?)
        } else {
            None
        };
        Ok(Expr::new(
            ExprKind::If {
                cond: Box::new(cond),
                then_branch,
                else_branch,
            },
            span,
        ))
    }

    /// Allows `if (..) { .. }\nelse { .. }` by tolerating a newline before `else`.
    fn skip_separators_if_else_follows(&mut self) {
        let start = self.pos;
        let mut pos = self.pos;
        while matches!(self.tokens.get(pos).map(|t| &t.kind), Some(TokenKind::Newline)) {
            pos += 1;
        }
        if matches!(self.tokens.get(pos).map(|t| &t.kind), Some(TokenKind::Else)) {
            self.pos = pos;
        } else {
            self.pos = start;
        }
    }

    fn parse_when_expr(&mut self) -> Result<Expr, ParseError> {
        let span = self.span();
        self.advance(); // when
        let subject = if self.matches(&TokenKind::LParen) {
            let e = self.parse_expr()?;
            self.expect(TokenKind::RParen, "')'")?;
            Some(Box::new(e))
        } else {
            None
        };
        self.expect(TokenKind::LBrace, "'{'")?;
        self.skip_separators();
        let mut arms = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace) {
            arms.push(self.parse_when_arm(subject.is_some())?);
            self.skip_separators();
        }
        self.expect(TokenKind::RBrace, "'}'")?;
        Ok(Expr::new(ExprKind::When { subject, arms }, span))
    }

    fn parse_when_arm(&mut self, has_subject: bool) -> Result<WhenArm, ParseError> {
        if matches!(self.peek_kind(), TokenKind::Else) {
            self.advance();
            self.expect(TokenKind::Arrow, "'->'")?;
            let body = self.parse_branch()?;
            return Ok(WhenArm {
                conditions: Vec::new(),
                is_else: true,
                body,
            });
        }
        let mut conditions = Vec::new();
        loop {
            let e = self.parse_expr()?;
            conditions.push(if has_subject {
                WhenCondition::Equals(e)
            } else {
                WhenCondition::Bool(e)
            });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::Arrow, "'->'")?;
        let body = self.parse_branch()?;
        Ok(WhenArm {
            conditions,
            is_else: false,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_src(src: &str) -> Vec<Stmt> {
        let tokens = tokenize(src).unwrap();
        parse(&tokens).unwrap().stmts
    }

    fn parse_err(src: &str) -> ParseError {
        let tokens = tokenize(src).unwrap();
        parse(&tokens).unwrap_err()
    }

    #[test]
    fn precedence_arith() {
        let stmts = parse_src("1 + 2 * 3");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Binary { op: BinaryOp::Add, lhs, rhs } => {
                    assert!(matches!(lhs.kind, ExprKind::IntLit(1)));
                    assert!(matches!(rhs.kind, ExprKind::Binary { op: BinaryOp::Mul, .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn range_vs_additive_precedence() {
        // 1..n-1 should parse as 1..(n-1)
        let stmts = parse_src("1..n-1");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Range { start, end } => {
                    assert!(matches!(start.kind, ExprKind::IntLit(1)));
                    assert!(matches!(end.kind, ExprKind::Binary { op: BinaryOp::Sub, .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn logical_precedence() {
        // a && b || c  =>  (a && b) || c
        let stmts = parse_src("a && b || c");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Binary { op: BinaryOp::Or, lhs, .. } => {
                    assert!(matches!(lhs.kind, ExprKind::Binary { op: BinaryOp::And, .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn val_decl_and_fun_decl_with_defaults() {
        let stmts = parse_src("fun add(a: Int, b: Int = 10): Int = a + b");
        match &stmts[0].kind {
            StmtKind::FunDecl { name, params, body, .. } => {
                assert_eq!(name, "add");
                assert_eq!(params.len(), 2);
                assert!(params[1].default.is_some());
                assert!(matches!(body, FunBody::Expr(_)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn call_with_named_args() {
        let stmts = parse_src("f(1, b = 2)");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Call { args, .. } => {
                    assert_eq!(args.len(), 2);
                    assert!(args[0].name.is_none());
                    assert_eq!(args[1].name.as_deref(), Some("b"));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn if_when_for_while() {
        parse_src("if (true) { 1 } else { 2 }");
        parse_src("when (x) { 1 -> \"one\" else -> \"other\" }");
        parse_src("for (i in 1..10) { println(i) }");
        parse_src("while (x < 10) { x = x + 1 }");
    }

    #[test]
    fn lambda_with_and_without_params() {
        let stmts = parse_src("val f = { x -> x * 2 }");
        match &stmts[0].kind {
            StmtKind::ValDecl { init, .. } => match &init.kind {
                ExprKind::Lambda { params, .. } => assert_eq!(params.len(), 1),
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }

        let stmts = parse_src("val f = { it * 2 }");
        match &stmts[0].kind {
            StmtKind::ValDecl { init, .. } => match &init.kind {
                ExprKind::Lambda { params, .. } => assert!(params.is_empty()),
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn file_import_parses() {
        let tokens = tokenize("@file:Import(\"utils.ksl\")\nprintln(1)").unwrap();
        let program = parse(&tokens).unwrap();
        assert_eq!(program.imports.len(), 1);
        assert_eq!(program.imports[0].path, "utils.ksl");
        assert_eq!(program.stmts.len(), 1);
    }

    #[test]
    fn file_import_multiple() {
        let tokens =
            tokenize("@file:Import(\"a.ksl\")\n@file:Import(\"b.ksl\")\nprintln(1)").unwrap();
        let program = parse(&tokens).unwrap();
        assert_eq!(program.imports.len(), 2);
        assert_eq!(program.imports[0].path, "a.ksl");
        assert_eq!(program.imports[1].path, "b.ksl");
    }

    #[test]
    fn file_import_unknown_annotation_errors() {
        let err = parse_err("@file:Bar(\"x.ksl\")");
        assert!(err.message.contains("unknown file annotation"), "{}", err.message);
    }

    #[test]
    fn file_import_bad_head_errors() {
        let err = parse_err("@notfile:Import(\"x.ksl\")");
        assert!(err.message.contains("expected 'file'"), "{}", err.message);
    }

    #[test]
    fn file_import_interpolated_path_errors() {
        let err = parse_err("@file:Import(\"$name.ksl\")");
        assert!(err.message.contains("plain string literal"), "{}", err.message);
    }

    #[test]
    fn file_import_after_statement_errors() {
        let err = parse_err("println(1)\n@file:Import(\"x.ksl\")");
        assert!(err.message.contains("unexpected") || err.message.contains("At"), "{}", err.message);
    }
}
