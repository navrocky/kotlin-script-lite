use crate::ast::*;
use crate::error::{ParseError, Span};
use crate::token::{Token, TokenKind};

pub fn parse(tokens: &[Token]) -> Result<Program, ParseError> {
    let mut p = Parser { tokens, pos: 0, loop_depth: 0 };
    p.parse_program()
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    /// Number of enclosing `while`/`for` bodies at the current parse
    /// position — saved and reset to 0 around a `fun`/lambda body, since
    /// `break`/`continue` can't unwind through a function boundary to a
    /// loop in the *caller*. Checked when parsing `break`/`continue` to
    /// reject them outside any loop, matching real Kotlin's compile-time
    /// rejection (this interpreter has no type/scope checking otherwise,
    /// so this is done at parse time instead).
    loop_depth: u32,
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
        let package = self.parse_package_decl()?;
        let (imports, imports_use) = self.parse_imports()?;
        let mut stmts = Vec::new();
        self.skip_separators();
        while !matches!(self.peek_kind(), TokenKind::Eof) {
            stmts.push(self.parse_stmt()?);
            self.skip_separators();
        }
        Ok(Program { package, imports, imports_use, stmts })
    }

    /// `package a.b`, optional and — if present — the very first thing in
    /// the file (checked before any leading separator is skipped, same as
    /// how the first `@`/`import` token is checked in `parse_imports`).
    fn parse_package_decl(&mut self) -> Result<Option<PackageDecl>, ParseError> {
        if !matches!(self.peek_kind(), TokenKind::Package) {
            return Ok(None);
        }
        let span = self.span();
        self.advance(); // 'package'
        let (path, _) = self.parse_dotted_path()?;
        Ok(Some(PackageDecl { path, span }))
    }

    /// Consumes a leading run of `@file:Import(...)` and `import a.b.c`
    /// declarations, in any order. Stops at the first token that's neither,
    /// so either form appearing later in the file is left for
    /// `parse_stmt`/`parse_primary` to reject as unexpected — both are only
    /// legal before any other statement.
    fn parse_imports(&mut self) -> Result<(Vec<ImportDecl>, Vec<ImportUse>), ParseError> {
        let mut imports = Vec::new();
        let mut imports_use = Vec::new();
        self.skip_separators();
        loop {
            match self.peek_kind() {
                TokenKind::At => imports.push(self.parse_file_import()?),
                TokenKind::Import => imports_use.push(self.parse_import_use()?),
                _ => break,
            }
            self.skip_separators();
        }
        Ok((imports, imports_use))
    }

    /// `import a.b.c` — brings the package-qualified name `a.b.c` into this
    /// file's own unqualified scope. At least 2 segments are required (a
    /// package name plus a member); `import c` alone has no package to
    /// qualify it and is rejected.
    fn parse_import_use(&mut self) -> Result<ImportUse, ParseError> {
        let span = self.span();
        self.advance(); // 'import'
        let (path, path_span) = self.parse_dotted_path()?;
        if path.len() < 2 {
            return Err(ParseError {
                message: "import path must be package-qualified, e.g. 'import a.b.c'".into(),
                span: path_span,
            });
        }
        Ok(ImportUse { path, span: span.merge(&path_span) })
    }

    /// `TypeAnnotation ::= Ident | '(' (TypeAnnotation (',' TypeAnnotation)*)? ')' '->' TypeAnnotation`
    ///
    /// Called only from a type-annotation position (right after the `:` in
    /// a `val`/`var`/parameter annotation or a `fun` return type) — never
    /// from expression position, so a leading `(` here is unambiguously a
    /// function type's parameter list, not a parenthesized expression.
    fn parse_type_annotation(&mut self) -> Result<TypeAnnotation, ParseError> {
        if matches!(self.peek_kind(), TokenKind::LParen) {
            let span = self.span();
            self.advance(); // '('
            let mut params = Vec::new();
            if !matches!(self.peek_kind(), TokenKind::RParen) {
                loop {
                    params.push(self.parse_type_annotation()?);
                    if !self.matches(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RParen, "')'")?;
            self.expect(TokenKind::Arrow, "'->'")?;
            let ret = self.parse_type_annotation()?;
            let full_span = span.merge(&ret.span());
            Ok(TypeAnnotation::Function(params, Box::new(ret), full_span))
        } else {
            let (name, name_span) = self.expect_ident_spanned()?;
            Ok(TypeAnnotation::Named(name, name_span))
        }
    }

    /// `ident (. ident)*` — used by `package`/`import`, which (unlike a
    /// `.`-chain in expression position, parsed as nested
    /// `FieldAccess`/`MethodCall` by `parse_postfix`) always require a
    /// dotted path, never a bare single name.
    fn parse_dotted_path(&mut self) -> Result<(Vec<String>, Span), ParseError> {
        let (first, first_span) = self.expect_ident_spanned()?;
        let mut path = vec![first];
        let mut full_span = first_span;
        while self.matches(&TokenKind::Dot) {
            let (segment, segment_span) = self.expect_ident_spanned()?;
            full_span = full_span.merge(&segment_span);
            path.push(segment);
        }
        Ok((path, full_span))
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
        let (path, path_span) = self.parse_plain_string_literal()?;
        self.expect(TokenKind::RParen, "')'")?;
        Ok(ImportDecl { path, span, path_span })
    }

    /// A `"..."` string literal with no interpolation — used for import
    /// paths, which must be known without evaluating any expression.
    /// Returns the literal's own span (quotes included) alongside its text,
    /// so a caller needing to point *at the string itself* (goto-definition
    /// on an import's file name, see `ksl-lsp`'s `goto_definition.rs`) has
    /// something narrower than the whole enclosing declaration to use.
    fn parse_plain_string_literal(&mut self) -> Result<(String, Span), ParseError> {
        let start = self.span();
        self.expect(TokenKind::StringStart, "'\"'")?;
        let mut s = String::new();
        let end = loop {
            let tok_span = self.span();
            match self.peek_kind().clone() {
                TokenKind::StringText(text) => {
                    self.advance();
                    s.push_str(&text);
                }
                TokenKind::StringEnd => {
                    self.advance();
                    break tok_span;
                }
                TokenKind::StringInterpText(_) | TokenKind::TemplateExprStart => {
                    return Err(self.err(
                        "import path must be a plain string literal (no interpolation)",
                    ));
                }
                other => return Err(self.err(format!("unexpected token in string: {other:?}"))),
            }
        };
        if s.is_empty() {
            return Err(ParseError {
                message: "import path must not be empty".into(),
                span: start,
            });
        }
        Ok((s, start.merge(&end)))
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
                if self.loop_depth == 0 {
                    return Err(ParseError {
                        message: "'break' is only allowed inside a loop".into(),
                        span,
                    });
                }
                Ok(Stmt::new(StmtKind::Break, span))
            }
            TokenKind::Continue => {
                self.advance();
                if self.loop_depth == 0 {
                    return Err(ParseError {
                        message: "'continue' is only allowed inside a loop".into(),
                        span,
                    });
                }
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
        let type_annotation =
            if self.matches(&TokenKind::Colon) { Some(self.parse_type_annotation()?) } else { None };
        self.expect(TokenKind::Assign, "'='")?;
        let init = self.parse_expr()?;
        Ok(Stmt::new(
            StmtKind::ValDecl {
                name,
                name_span,
                type_annotation,
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
            let type_annotation =
                if self.matches(&TokenKind::Colon) { Some(self.parse_type_annotation()?) } else { None };
            let default = if self.matches(&TokenKind::Assign) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            params.push(Param {
                name,
                name_span,
                type_annotation,
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
        let return_type =
            if self.matches(&TokenKind::Colon) { Some(self.parse_type_annotation()?) } else { None };
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let body = if self.matches(&TokenKind::Assign) {
            self.parse_expr().map(|e| FunBody::Expr(Box::new(e)))
        } else {
            self.parse_block_stmts().map(FunBody::Block)
        };
        self.loop_depth = saved_loop_depth;
        Ok(Stmt::new(
            StmtKind::FunDecl { name, name_span, params, return_type, body: body? },
            span,
        ))
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        self.advance(); // while
        self.expect(TokenKind::LParen, "'('")?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen, "')'")?;
        self.loop_depth += 1;
        let body = self.parse_block_stmts();
        self.loop_depth -= 1;
        Ok(Stmt::new(StmtKind::While { cond, body: body? }, span))
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        self.advance(); // for
        self.expect(TokenKind::LParen, "'('")?;
        let (var_name, var_name_span) = self.expect_ident_spanned()?;
        self.expect(TokenKind::In, "'in'")?;
        let iterable = self.parse_expr()?;
        self.expect(TokenKind::RParen, "')'")?;
        self.loop_depth += 1;
        let body = self.parse_block_stmts();
        self.loop_depth -= 1;
        Ok(Stmt::new(
            StmtKind::For {
                var_name,
                var_name_span,
                iterable,
                body: body?,
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
            } else if matches!(self.peek_kind(), TokenKind::Dot) {
                self.advance(); // '.'
                let (member, member_span) = self.expect_ident_spanned()?;
                let full_span = expr.span.merge(&member_span);
                expr = if matches!(self.peek_kind(), TokenKind::LParen) {
                    let args = self.parse_call_args()?;
                    Expr::new(
                        ExprKind::MethodCall {
                            receiver: Box::new(expr),
                            method: member,
                            method_span: member_span,
                            args,
                        },
                        full_span,
                    )
                } else {
                    Expr::new(
                        ExprKind::FieldAccess {
                            receiver: Box::new(expr),
                            field: member,
                            field_span: member_span,
                        },
                        full_span,
                    )
                };
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

        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let body = self.parse_stmts_until_rbrace();
        self.loop_depth = saved_loop_depth;
        Ok(Expr::new(ExprKind::Lambda { params, body: body? }, span))
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
                        type_annotation: None,
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

    #[test]
    fn package_decl_parses() {
        let tokens = tokenize("package a.b.c\nprintln(1)").unwrap();
        let program = parse(&tokens).unwrap();
        assert_eq!(program.package.as_ref().unwrap().path, vec!["a", "b", "c"]);
        assert_eq!(program.stmts.len(), 1);
    }

    #[test]
    fn package_decl_is_optional() {
        let program = parse(&tokenize("println(1)").unwrap()).unwrap();
        assert!(program.package.is_none());
    }

    #[test]
    fn package_decl_after_statement_errors() {
        let err = parse_err("println(1)\npackage a.b");
        assert!(err.message.contains("unexpected") || err.message.contains("Package"), "{}", err.message);
    }

    #[test]
    fn import_use_parses_and_requires_a_qualified_path() {
        let tokens = tokenize("import a.b.greet\nprintln(1)").unwrap();
        let program = parse(&tokens).unwrap();
        assert_eq!(program.imports_use.len(), 1);
        assert_eq!(program.imports_use[0].path, vec!["a", "b", "greet"]);

        let err = parse_err("import greet");
        assert!(err.message.contains("package-qualified"), "{}", err.message);
    }

    #[test]
    fn package_file_import_and_import_use_can_all_appear_together_in_any_order() {
        let tokens = tokenize(
            "package my.pkg\nimport a.b.greet\n@file:Import(\"utils.ksl\")\nimport c.d.helper\nprintln(1)",
        )
        .unwrap();
        let program = parse(&tokens).unwrap();
        assert_eq!(program.package.as_ref().unwrap().path, vec!["my", "pkg"]);
        assert_eq!(program.imports.len(), 1);
        assert_eq!(program.imports_use.len(), 2);
        assert_eq!(program.stmts.len(), 1);
    }

    #[test]
    fn dotted_call_chain_parses_as_nested_method_call() {
        // `a.b.greet(1)` — the parser can't know yet whether this is a
        // package-qualified call or a real method call, so it parses the
        // same way regardless (see `ExprKind::MethodCall`'s doc comment);
        // `a.b` becomes the receiver, itself a `FieldAccess` on `a`.
        let stmts = parse_src("a.b.greet(1)");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::MethodCall { receiver, method, args, .. } => {
                    assert_eq!(method, "greet");
                    assert_eq!(args.len(), 1);
                    match &receiver.kind {
                        ExprKind::FieldAccess { receiver, field, .. } => {
                            assert_eq!(field, "b");
                            assert!(matches!(&receiver.kind, ExprKind::Name(n) if n == "a"));
                        }
                        other => panic!("unexpected {other:?}"),
                    }
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn field_access_without_call_parses() {
        let stmts = parse_src("a.b");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::FieldAccess { receiver, field, .. } => {
                    assert_eq!(field, "b");
                    assert!(matches!(&receiver.kind, ExprKind::Name(n) if n == "a"));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn method_call_on_a_call_result_parses() {
        // `f().g()` — a `.` chain doesn't have to start from a bare name.
        let stmts = parse_src("f().g()");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::MethodCall { receiver, method, .. } => {
                    assert_eq!(method, "g");
                    assert!(matches!(&receiver.kind, ExprKind::Call { .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bare_name_without_dot_stays_a_plain_name() {
        let stmts = parse_src("greet(1)");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Call { callee, .. } => {
                    assert!(matches!(&callee.kind, ExprKind::Name(n) if n == "greet"));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn break_and_continue_inside_loop_are_fine() {
        parse_src("while (true) { break }");
        parse_src("while (true) { continue }");
        parse_src("for (i in 1..10) { if (i == 5) { break } }");
    }

    #[test]
    fn break_outside_loop_is_rejected() {
        let err = parse_err("break");
        assert!(err.message.contains("'break'"), "{}", err.message);
    }

    #[test]
    fn continue_outside_loop_is_rejected() {
        let err = parse_err("continue");
        assert!(err.message.contains("'continue'"), "{}", err.message);
    }

    #[test]
    fn break_inside_fun_body_inside_loop_is_rejected() {
        // A `break` in a nested `fun` can't reach the caller's loop, unlike
        // a bare unwind through `Flow::Break` at runtime — this must be
        // caught even though the `fun` itself is lexically inside a `while`.
        let err = parse_err("while (true) { fun f() { break } }");
        assert!(err.message.contains("'break'"), "{}", err.message);
    }

    #[test]
    fn continue_inside_lambda_body_inside_loop_is_rejected() {
        let err = parse_err("while (true) { val f = { continue } }");
        assert!(err.message.contains("'continue'"), "{}", err.message);
    }

    #[test]
    fn break_inside_loop_inside_fun_body_is_fine() {
        parse_src("fun f() { while (true) { break } }");
    }

    #[test]
    fn function_type_annotation_parses() {
        let stmts = parse_src("fun apply(f: (Int, String) -> Result, x: Int): Int = x");
        match &stmts[0].kind {
            StmtKind::FunDecl { params, .. } => match &params[0].type_annotation {
                Some(TypeAnnotation::Function(param_types, ret, _)) => {
                    assert_eq!(param_types.len(), 2);
                    assert!(matches!(&param_types[0], TypeAnnotation::Named(n, _) if n == "Int"));
                    assert!(matches!(&param_types[1], TypeAnnotation::Named(n, _) if n == "String"));
                    assert!(matches!(ret.as_ref(), TypeAnnotation::Named(n, _) if n == "Result"));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn nested_function_type_annotation_parses() {
        // `((Int) -> Int) -> String` — a function type itself taking a
        // function type as its only parameter.
        let stmts = parse_src("val f: ((Int) -> Int) -> String = g");
        match &stmts[0].kind {
            StmtKind::ValDecl { type_annotation: Some(TypeAnnotation::Function(params, ret, _)), .. } => {
                assert_eq!(params.len(), 1);
                assert!(matches!(&params[0], TypeAnnotation::Function(inner_params, inner_ret, _)
                    if inner_params.len() == 1
                        && matches!(&inner_params[0], TypeAnnotation::Named(n, _) if n == "Int")
                        && matches!(inner_ret.as_ref(), TypeAnnotation::Named(n, _) if n == "Int")));
                assert!(matches!(ret.as_ref(), TypeAnnotation::Named(n, _) if n == "String"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn function_type_with_no_parameters_parses() {
        let stmts = parse_src("val f: () -> Unit = g");
        match &stmts[0].kind {
            StmtKind::ValDecl { type_annotation: Some(TypeAnnotation::Function(params, ret, _)), .. } => {
                assert!(params.is_empty());
                assert!(matches!(ret.as_ref(), TypeAnnotation::Named(n, _) if n == "Unit"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
