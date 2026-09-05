use std::collections::HashMap;

use crate::ast::*;
use crate::error::{Expected, IdentRole, ParseError, Span};
use crate::token::{Token, TokenKind};

pub fn parse(tokens: &[Token]) -> Result<Program, ParseError> {
    parse_with_docs(tokens, HashMap::new())
}

/// Like `parse`, but also attaches doc comments (see
/// `lexer::Lexer::doc_comments`) to the `val`/`var`/`fun` declarations they
/// immediately precede — `doc_comments` is keyed by source line number,
/// exactly as the lexer produces it.
pub fn parse_with_docs(tokens: &[Token], doc_comments: HashMap<u32, String>) -> Result<Program, ParseError> {
    let mut p = Parser { tokens, pos: 0, loop_depth: 0, doc_comments };
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
    /// Doc-comment text (see `lexer::Lexer::doc_comments`), keyed by the
    /// line a comment immediately precedes. Consulted only in
    /// `parse_val_decl`/`parse_fun_decl` — the only two constructs KDoc
    /// ever attaches to.
    doc_comments: HashMap<u32, String>,
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

    /// The span of the most recently *consumed* token — the real end of
    /// whatever was just parsed, as opposed to `self.span()` (which looks
    /// at the *next*, not-yet-consumed token). Used to widen a statement's
    /// or expression's own span to cover everything it actually consumed
    /// (through a block's closing `}`, say) rather than just its first
    /// token — see `parse_stmt`'s own wrapping, which is what makes this
    /// accurate all the way down through nested blocks: every statement
    /// inside one is itself parsed via `parse_stmt`, recursively.
    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
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
            let mut e = self.err(format!("expected {what}, found {:?}", self.peek_kind()));
            e.expected.push(Expected::Token(kind));
            Err(e)
        }
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError::new(message, self.span())
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek_kind(), TokenKind::Newline | TokenKind::Semicolon) {
            self.advance();
        }
    }

    /// Like `skip_separators`, but `Newline` only — used where the grammar
    /// unconditionally requires a specific continuation next (an `if`/
    /// `while`/`for`/`when` header's body, or a `fun`'s `=`/`{`), so a
    /// newline right there is always just formatting, never a real
    /// statement separator. Deliberately doesn't also skip `Semicolon`,
    /// unlike `skip_separators` — none of this method's call sites are
    /// statement-list positions where a bare `;` would mean anything.
    fn skip_newlines(&mut self) {
        while matches!(self.peek_kind(), TokenKind::Newline) {
            self.advance();
        }
    }

    /// `role` is what grammatical position this identifier fills — see
    /// `error::IdentRole`'s doc comment — recorded on the returned error
    /// (never consulted on success) so a completion caller re-parsing text
    /// truncated at the cursor can tell "expects a type name" apart from
    /// "expects a brand-new binding name", which are both just "an
    /// identifier" at the token level.
    fn expect_ident(&mut self, role: IdentRole) -> Result<String, ParseError> {
        Ok(self.expect_ident_spanned(role)?.0)
    }

    /// Like `expect_ident`, but also returns the identifier token's own
    /// span (not the caller's `self.span()` at some other position) — used
    /// where the exact name span matters for hover/goto-definition.
    fn expect_ident_spanned(&mut self, role: IdentRole) -> Result<(String, Span), ParseError> {
        let span = self.span();
        match self.peek_kind().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok((name, span))
            }
            other => {
                let mut e = self.err(format!("expected identifier, found {other:?}"));
                e.expected.push(Expected::Identifier(role));
                Err(e)
            }
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
    /// `import a.b.c` (single symbol) or `import a.b.*` (wildcard — every
    /// top-level name package `a.b` declares). Can't reuse
    /// `parse_dotted_path` as-is: a trailing `.*` needs its own lookahead,
    /// since `*` is never a legal path segment otherwise (`Star` isn't an
    /// identifier token).
    fn parse_import_use(&mut self) -> Result<ImportUse, ParseError> {
        let span = self.span();
        self.advance(); // 'import'
        let (first, first_span) = self.expect_ident_spanned(IdentRole::QualifiedPathSegment)?;
        let mut path = vec![first];
        let mut full_span = first_span;
        let mut wildcard = false;
        while self.matches(&TokenKind::Dot) {
            if matches!(self.peek_kind(), TokenKind::Star) {
                let star_span = self.span();
                self.advance(); // '*'
                full_span = full_span.merge(&star_span);
                wildcard = true;
                break;
            }
            let (segment, segment_span) = self.expect_ident_spanned(IdentRole::QualifiedPathSegment)?;
            full_span = full_span.merge(&segment_span);
            path.push(segment);
        }
        // A wildcard's `path` is the package name alone (e.g. `["kotlin",
        // "test"]` for `import kotlin.test.*`) — at least 1 segment, always
        // true since `first` is always pushed. A single-symbol import's
        // `path` is package + member — at least 2 segments required, same
        // rule as before this method grew wildcard support.
        if !wildcard && path.len() < 2 {
            return Err(ParseError::new("import path must be package-qualified, e.g. 'import a.b.c'", full_span));
        }
        Ok(ImportUse { path, wildcard, span: span.merge(&full_span) })
    }

    /// `TypeAnnotation ::= Ident | '(' (TypeAnnotation (',' TypeAnnotation)*)? ')' '->' TypeAnnotation`
    ///
    /// Called only from a type-annotation position (right after the `:` in
    /// a `val`/`var`/parameter annotation or a `fun` return type) — never
    /// from expression position, so a leading `(` here is unambiguously a
    /// function type's parameter list, not a parenthesized expression.
    fn parse_type_annotation(&mut self) -> Result<TypeAnnotation, ParseError> {
        let base = if matches!(self.peek_kind(), TokenKind::LParen) {
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
            if matches!(self.peek_kind(), TokenKind::Arrow) {
                self.advance(); // '->'
                let ret = self.parse_type_annotation()?;
                let full_span = span.merge(&ret.span());
                TypeAnnotation::Function(params, Box::new(ret), full_span)
            } else if params.len() == 1 {
                // Grouping parens around a single annotation, not a
                // function-type parameter list — the only real use for
                // this is `((Int) -> String)?`, suffixing `?` onto the
                // whole function type rather than just its return type.
                params.into_iter().next().unwrap()
            } else {
                return Err(self.err("expected '->' after a parenthesized type list".to_string()));
            }
        } else {
            let (name, name_span) = self.expect_ident_spanned(IdentRole::TypeName)?;
            TypeAnnotation::Named(name, name_span)
        };
        if matches!(self.peek_kind(), TokenKind::Question) {
            let q_span = self.span();
            self.advance(); // '?'
            let full_span = base.span().merge(&q_span);
            Ok(TypeAnnotation::Nullable(Box::new(base), full_span))
        } else {
            Ok(base)
        }
    }

    /// `ident (. ident)*` — used by `package`/`import`, which (unlike a
    /// `.`-chain in expression position, parsed as nested
    /// `FieldAccess`/`MethodCall` by `parse_postfix`) always require a
    /// dotted path, never a bare single name.
    fn parse_dotted_path(&mut self) -> Result<(Vec<String>, Span), ParseError> {
        let (first, first_span) = self.expect_ident_spanned(IdentRole::QualifiedPathSegment)?;
        let mut path = vec![first];
        let mut full_span = first_span;
        while self.matches(&TokenKind::Dot) {
            let (segment, segment_span) = self.expect_ident_spanned(IdentRole::QualifiedPathSegment)?;
            full_span = full_span.merge(&segment_span);
            path.push(segment);
        }
        Ok((path, full_span))
    }

    fn parse_file_import(&mut self) -> Result<ImportDecl, ParseError> {
        let span = self.span();
        self.expect(TokenKind::At, "'@'")?;
        let head = self.expect_ident(IdentRole::FileImportHead)?;
        if head != "file" {
            return Err(self.err(format!("expected 'file' after '@', found '{head}'")));
        }
        self.expect(TokenKind::Colon, "':'")?;
        let name = self.expect_ident(IdentRole::FileImportAnnotationName)?;
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
            return Err(ParseError::new("import path must not be empty", start));
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

    /// Either a `{ ... }` block or a single statement, used for if/when
    /// branches — real Kotlin's grammar allows any statement here, not just
    /// an expression (`if (cond) return "x"`, `if (cond) continue`, both
    /// with no braces, are ordinary Kotlin). Delegates to `parse_stmt`
    /// rather than `parse_expr` directly so `return`/`break`/`continue`
    /// work in this position exactly as they do inside a `{ }` block.
    fn parse_branch(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if matches!(self.peek_kind(), TokenKind::LBrace) {
            self.parse_block_stmts()
        } else {
            Ok(vec![self.parse_stmt()?])
        }
    }

    /// Parses one statement and widens its own span to cover everything it
    /// actually consumed — `start` through the last token this statement
    /// swallowed (a block-bodied `fun`/`while`/`for`'s own closing `}`,
    /// say), not just `start`'s single token. This is the one recursive
    /// entry point every statement anywhere in the program (including
    /// every statement nested inside a block/lambda/loop/branch body) goes
    /// through, so the fix applies uniformly, all the way down, rather
    /// than needing a matching fix in each individual `parse_*` function
    /// below — see `prev_span`'s doc comment.
    ///
    /// This is what `resolver::Resolver::open_scope` relies on to bound a
    /// scope by its *own* real extent instead of guessing via the next
    /// sibling statement's start — before this, any block-bodied
    /// construct's span stopped at its own opening keyword, so a scope's
    /// upper bound had to fall back to "wherever the next statement
    /// begins," which incorrectly swallowed the trailing gap (blank
    /// lines/whitespace) between this statement's closing `}` and that
    /// next one into *this* statement's own scope.
    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.span();
        let mut stmt = match self.peek_kind() {
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
                Ok(Stmt::new(StmtKind::Return(value), start))
            }
            TokenKind::Break => {
                self.advance();
                if self.loop_depth == 0 {
                    return Err(ParseError::new("'break' is only allowed inside a loop", start));
                }
                Ok(Stmt::new(StmtKind::Break, start))
            }
            TokenKind::Continue => {
                self.advance();
                if self.loop_depth == 0 {
                    return Err(ParseError::new("'continue' is only allowed inside a loop", start));
                }
                Ok(Stmt::new(StmtKind::Continue, start))
            }
            _ => self.parse_expr_or_assign_stmt(),
        }?;
        let end = self.prev_span();
        stmt.span = Span::with_end(start.line, start.col, end.end_line, end.end_col);
        Ok(stmt)
    }

    fn parse_val_decl(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        let doc_comment = self.doc_comments.remove(&span.line);
        let mutable = matches!(self.peek_kind(), TokenKind::Var);
        self.advance(); // val/var
        let (name, name_span) = self.expect_ident_spanned(IdentRole::NewBindingName)?;
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
                doc_comment,
            },
            span,
        ))
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
        let mut params = Vec::new();
        self.expect(TokenKind::LParen, "'('")?;
        while !matches!(self.peek_kind(), TokenKind::RParen) {
            let (name, name_span) = self.expect_ident_spanned(IdentRole::NewBindingName)?;
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
        let doc_comment = self.doc_comments.remove(&span.line);
        self.advance(); // fun
        let (name, name_span) = self.expect_ident_spanned(IdentRole::NewBindingName)?;
        let params = self.parse_params()?;
        self.skip_newlines();
        let return_type =
            if self.matches(&TokenKind::Colon) { Some(self.parse_type_annotation()?) } else { None };
        self.skip_newlines();
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let body = if self.matches(&TokenKind::Assign) {
            self.parse_expr().map(|e| FunBody::Expr(Box::new(e)))
        } else {
            self.parse_block_stmts().map(FunBody::Block)
        };
        self.loop_depth = saved_loop_depth;
        Ok(Stmt::new(
            StmtKind::FunDecl { name, name_span, params, return_type, body: body?, doc_comment },
            span,
        ))
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        self.advance(); // while
        self.expect(TokenKind::LParen, "'('")?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen, "')'")?;
        self.skip_newlines();
        self.loop_depth += 1;
        // A `{ ... }` block or a single statement — same as `if`/`when`'s
        // own branches (`parse_branch`), matching real Kotlin: `while
        // (cond) break` with no braces is ordinary Kotlin, not special.
        let body = self.parse_branch();
        self.loop_depth -= 1;
        Ok(Stmt::new(StmtKind::While { cond, body: body? }, span))
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let span = self.span();
        self.advance(); // for
        self.expect(TokenKind::LParen, "'('")?;
        let (var_name, var_name_span) = self.expect_ident_spanned(IdentRole::NewBindingName)?;
        self.expect(TokenKind::In, "'in'")?;
        let iterable = self.parse_expr()?;
        self.expect(TokenKind::RParen, "')'")?;
        self.skip_newlines();
        self.loop_depth += 1;
        // Braceless single-statement body allowed too — see `parse_while`.
        let body = self.parse_branch();
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
        let mut lhs = self.parse_elvis()?;
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
            let rhs = self.parse_elvis()?;
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

    /// `?:` — binds looser than `<`/`>`/`==`/`&&`/`||` (see `parse_comparison`,
    /// which calls this instead of `parse_range` directly) but tighter than
    /// `..`/`+`/`-` (`parse_range`, below), matching real Kotlin's actual
    /// precedence table. Right-recursive (not a loop) for right-associativity:
    /// `a ?: b ?: c` groups as `a ?: (b ?: c)`, same as Kotlin.
    fn parse_elvis(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_range()?;
        if matches!(self.peek_kind(), TokenKind::QuestionColon) {
            let span = self.span();
            self.advance();
            let rhs = self.parse_elvis()?;
            return Ok(Expr::new(
                ExprKind::Elvis {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            ));
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
        if let Some(op) = match self.peek_kind() {
            TokenKind::PlusPlus => Some(IncDecOp::Inc),
            TokenKind::MinusMinus => Some(IncDecOp::Dec),
            _ => None,
        } {
            self.advance();
            let (name, name_span) = self.expect_ident_spanned(IdentRole::NewBindingName)?;
            return Ok(Expr::new(
                ExprKind::IncDec { op, name, name_span, prefix: true },
                span,
            ));
        }
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
                let args = self.parse_call_args().map_err(|e| attach_callee_to_call_argument_error(e, &expr))?;
                expr = Expr::new(
                    ExprKind::Call {
                        callee: Box::new(expr),
                        args,
                    },
                    span,
                );
            } else if matches!(self.peek_kind(), TokenKind::Dot | TokenKind::QuestionDot) {
                let safe = matches!(self.peek_kind(), TokenKind::QuestionDot);
                self.advance(); // '.' or '?.'
                let (member, member_span) = self.expect_ident_spanned(IdentRole::Other)?;
                let full_span = expr.span.merge(&member_span);
                expr = if matches!(self.peek_kind(), TokenKind::LParen) {
                    let args = self.parse_call_args()?;
                    Expr::new(
                        ExprKind::MethodCall {
                            receiver: Box::new(expr),
                            method: member,
                            method_span: member_span,
                            args,
                            safe,
                        },
                        full_span,
                    )
                } else {
                    Expr::new(
                        ExprKind::FieldAccess {
                            receiver: Box::new(expr),
                            field: member,
                            field_span: member_span,
                            safe,
                        },
                        full_span,
                    )
                };
            } else if matches!(self.peek_kind(), TokenKind::BangBang) {
                let span = self.span();
                self.advance(); // '!!'
                expr = Expr::new(
                    ExprKind::Unary {
                        op: UnaryOp::NotNullAssert,
                        expr: Box::new(expr),
                    },
                    span,
                );
            } else if matches!(self.peek_kind(), TokenKind::PlusPlus | TokenKind::MinusMinus)
                && matches!(expr.kind, ExprKind::Name(_))
            {
                // Postfix `x++`/`x--` — restricted to a bare name target,
                // same as `StmtKind::Assign`'s target (no settable
                // `.field`/`[i]` exists yet). A receiver that isn't a plain
                // name (e.g. `f()++`) leaves the token unconsumed instead of
                // erroring here — it just isn't a valid postfix-inc/dec
                // target, same as it isn't a valid assignment target.
                let op = if matches!(self.peek_kind(), TokenKind::PlusPlus) { IncDecOp::Inc } else { IncDecOp::Dec };
                let span = self.span();
                self.advance();
                let name_span = expr.span;
                let name = match expr.kind {
                    ExprKind::Name(name) => name,
                    _ => unreachable!("matched above"),
                };
                expr = Expr::new(ExprKind::IncDec { op, name, name_span, prefix: false }, span);
            } else {
                break;
            }
        }
        Ok(expr)
    }

    /// Parses a call's own `(...)` argument list. Every fallible step below
    /// funnels through `parse_call_args_body`, which enriches any failure
    /// with `Expected::CallArgument` (`callee` left `None` here — this
    /// function has no way to know which expression it's the arguments
    /// *of* — filled in by `parse_postfix`, the only caller that does) so
    /// a completion caller re-parsing text truncated mid-call (`test(a =
    /// 1, b`) still gets a useful answer, not just a bare `Expected::
    /// Token(RParen)`/`Token(Comma)` — there's no `Call`/`MethodCall` AST
    /// node yet to read the callee/already-used-names off of otherwise,
    /// since the call itself hasn't finished parsing.
    fn parse_call_args(&mut self) -> Result<Vec<Arg>, ParseError> {
        self.expect(TokenKind::LParen, "'('")?;
        let mut args = Vec::new();
        if let Err(e) = self.parse_call_args_body(&mut args) {
            let already_named = args.iter().filter_map(|a| a.name.clone()).collect();
            let mut e = e;
            e.expected.push(Expected::CallArgument { callee: None, already_named });
            return Err(e);
        }
        Ok(args)
    }

    fn parse_call_args_body(&mut self, args: &mut Vec<Arg>) -> Result<(), ParseError> {
        while !matches!(self.peek_kind(), TokenKind::RParen) {
            let (name, name_span) = if matches!(self.peek_kind(), TokenKind::Ident(_))
                && matches!(self.peek_at(1), TokenKind::Assign)
            {
                let (n, span) = self.expect_ident_spanned(IdentRole::Other)?;
                self.advance(); // '='
                (Some(n), Some(span))
            } else {
                (None, None)
            };
            let value = self.parse_expr()?;
            args.push(Arg { name, name_span, value });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen, "')'")?;
        Ok(())
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
            TokenKind::Null => {
                self.advance();
                Ok(Expr::new(ExprKind::NullLit, span))
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
        let body = body?;
        // Widened to the real closing `}`, matching `parse_stmt`'s own
        // widening — see its doc comment for why (this is what
        // `resolver::Resolver::open_scope` uses to bound the lambda's own
        // scope).
        let end = self.prev_span();
        Ok(Expr::new(ExprKind::Lambda { params, body }, Span::with_end(span.line, span.col, end.end_line, end.end_col)))
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
        self.skip_newlines();
        let then_branch = self.parse_branch()?;
        self.skip_separators_if_else_follows();
        let else_branch = if self.matches(&TokenKind::Else) {
            Some(self.parse_branch()?)
        } else {
            None
        };
        // Widened to whichever branch was consumed last (`else_branch` if
        // present, `then_branch` otherwise) — same reasoning as
        // `parse_lambda`'s own widening.
        let end = self.prev_span();
        Ok(Expr::new(
            ExprKind::If {
                cond: Box::new(cond),
                then_branch,
                else_branch,
            },
            Span::with_end(span.line, span.col, end.end_line, end.end_col),
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
        self.skip_newlines();
        self.expect(TokenKind::LBrace, "'{'")?;
        self.skip_separators();
        let mut arms = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace) {
            arms.push(self.parse_when_arm(subject.is_some())?);
            self.skip_separators();
        }
        self.expect(TokenKind::RBrace, "'}'")?;
        // Widened to the real closing `}`, same reasoning as
        // `parse_lambda`'s own widening.
        let end = self.prev_span();
        Ok(Expr::new(ExprKind::When { subject, arms }, Span::with_end(span.line, span.col, end.end_line, end.end_col)))
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

/// Fills in every `Expected::CallArgument`'s `callee` on `e` from
/// `callee_expr` — `parse_call_args`/`parse_call_args_body` already know
/// which names were used so far but not what they're arguments *of*
/// (they're only ever called after the callee expression has already been
/// parsed), so `parse_postfix` — the only caller that has both — patches
/// it in here once `parse_call_args` returns. A non-`Name` callee (`f()()`,
/// calling the result of another call) leaves `callee` as the `None`
/// `parse_call_args` already set it to; there's no bare name for a
/// completion caller to look parameters up by there anyway.
fn attach_callee_to_call_argument_error(mut e: ParseError, callee_expr: &Expr) -> ParseError {
    if let ExprKind::Name(name) = &callee_expr.kind {
        for exp in &mut e.expected {
            if let Expected::CallArgument { callee, .. } = exp {
                *callee = Some(name.clone());
            }
        }
    }
    e
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

    // ---- ParseError::expected (see error::Expected's doc comment) ----

    #[test]
    fn expect_populates_expected_token_on_failure() {
        let e = parse_err("while (true");
        assert!(e.expected.contains(&Expected::Token(TokenKind::RParen)), "{:?}", e.expected);
    }

    #[test]
    fn expect_ident_spanned_reports_type_name_role_in_a_type_position() {
        let e = parse_err("val x: ");
        assert!(
            e.expected.contains(&Expected::Identifier(IdentRole::TypeName)),
            "{:?}",
            e.expected
        );
    }

    #[test]
    fn expect_ident_spanned_reports_new_binding_name_role_after_val() {
        let e = parse_err("val ");
        assert!(
            e.expected.contains(&Expected::Identifier(IdentRole::NewBindingName)),
            "{:?}",
            e.expected
        );
    }

    #[test]
    fn expect_ident_spanned_reports_qualified_path_segment_role_after_import_dot() {
        let e = parse_err("import a.");
        assert!(
            e.expected.contains(&Expected::Identifier(IdentRole::QualifiedPathSegment)),
            "{:?}",
            e.expected
        );
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
    fn nullable_type_annotation_parses() {
        let stmts = parse_src("val x: String? = null");
        match &stmts[0].kind {
            StmtKind::ValDecl { type_annotation: Some(TypeAnnotation::Nullable(inner, _)), init, .. } => {
                assert!(matches!(inner.as_ref(), TypeAnnotation::Named(n, _) if n == "String"));
                assert!(matches!(init.kind, ExprKind::NullLit));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn nullable_function_type_annotations_both_shapes() {
        // `(Int) -> String?` — nullable return type, no wrapping parens needed.
        let stmts = parse_src("val f: (Int) -> String? = g");
        match &stmts[0].kind {
            StmtKind::ValDecl { type_annotation: Some(TypeAnnotation::Function(_, ret, _)), .. } => {
                assert!(matches!(ret.as_ref(), TypeAnnotation::Nullable(inner, _) if matches!(inner.as_ref(), TypeAnnotation::Named(n, _) if n == "String")));
            }
            other => panic!("unexpected {other:?}"),
        }

        // `((Int) -> String)?` — the whole function type is nullable.
        let stmts = parse_src("val f: ((Int) -> String)? = g");
        match &stmts[0].kind {
            StmtKind::ValDecl { type_annotation: Some(TypeAnnotation::Nullable(inner, _)), .. } => {
                assert!(matches!(inner.as_ref(), TypeAnnotation::Function(..)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn null_literal_parses() {
        let stmts = parse_src("null");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => assert!(matches!(e.kind, ExprKind::NullLit)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn elvis_binds_tighter_than_comparison_and_looser_than_range() {
        // a < b ?: c  =>  a < (b ?: c)
        let stmts = parse_src("a < b ?: c");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Binary { op: BinaryOp::Lt, rhs, .. } => {
                    assert!(matches!(rhs.kind, ExprKind::Elvis { .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }

        // a ?: b..c  =>  a ?: (b..c)
        let stmts = parse_src("a ?: b..c");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Elvis { rhs, .. } => {
                    assert!(matches!(rhs.kind, ExprKind::Range { .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn elvis_is_right_associative() {
        // a ?: b ?: c  =>  a ?: (b ?: c)
        let stmts = parse_src("a ?: b ?: c");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Elvis { lhs, rhs } => {
                    assert!(matches!(lhs.kind, ExprKind::Name(ref n) if n == "a"));
                    assert!(matches!(rhs.kind, ExprKind::Elvis { .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn safe_call_chain_nests_left_to_right() {
        let stmts = parse_src("a?.b?.c");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::FieldAccess { receiver, field, safe: true, .. } => {
                    assert_eq!(field, "c");
                    match &receiver.kind {
                        ExprKind::FieldAccess { receiver, field, safe: true, .. } => {
                            assert_eq!(field, "b");
                            assert!(matches!(receiver.kind, ExprKind::Name(ref n) if n == "a"));
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
    fn not_null_assert_parses_as_postfix_unary() {
        let stmts = parse_src("x!!");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Unary { op: UnaryOp::NotNullAssert, expr } => {
                    assert!(matches!(expr.kind, ExprKind::Name(ref n) if n == "x"));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn not_null_assert_composes_with_method_call() {
        let stmts = parse_src("x!!.length()");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::MethodCall { receiver, method, safe: false, .. } => {
                    assert_eq!(method, "length");
                    assert!(matches!(receiver.kind, ExprKind::Unary { op: UnaryOp::NotNullAssert, .. }));
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
    fn braceless_single_statement_while_and_for_bodies_parse() {
        // Same as `if`/`when`'s own branches (`parse_branch`) -- a loop's
        // body doesn't have to be a `{ ... }` block either.
        let stmts = parse_src("for (i in 1..10) println(i)");
        match &stmts[0].kind {
            StmtKind::For { body, .. } => assert_eq!(body.len(), 1),
            other => panic!("unexpected {other:?}"),
        }

        let stmts2 = parse_src("while (x < 10) x = x + 1");
        match &stmts2[0].kind {
            StmtKind::While { body, .. } => assert_eq!(body.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn header_followed_by_a_newline_before_the_body_parses() {
        // A newline right after a header's closing `)` (or, for `fun`,
        // its params/return type) is always just formatting, never a
        // statement separator, since the grammar unconditionally requires
        // a body/`=`/`{` to follow — see `skip_newlines`.
        parse_src("if (true)\n1");
        parse_src("if (true)\n{ 1 }");
        parse_src("while (false)\n{ 1 }");
        parse_src("for (i in 1..1)\n{ println(i) }");
        parse_src("when (1)\n{ 1 -> \"one\" else -> \"other\" }");
        parse_src("fun f(): Int\n= 1");
        parse_src("fun f()\n{ }");
    }

    #[test]
    fn if_condition_may_span_multiple_lines() {
        // Newline *inside* the condition's parens (before the closing
        // `)`, after an `Ident`) — suppressed at the lexer level, see
        // `lexer.rs`'s `bracket_stack`.
        let stmts = parse_src("if (\n  x\n) { 1 }");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => assert!(matches!(e.kind, ExprKind::If { .. })),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn call_with_a_newline_before_the_closing_paren_parses() {
        let stmts = parse_src("f(\n  1,\n  2\n)");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => match &e.kind {
                ExprKind::Call { args, .. } => assert_eq!(args.len(), 2),
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
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
    fn wildcard_import_parses() {
        let tokens = tokenize("import kotlin.test.*\nprintln(1)").unwrap();
        let program = parse(&tokens).unwrap();
        assert_eq!(program.imports_use.len(), 1);
        assert!(program.imports_use[0].wildcard);
        // Unlike a single-symbol import, `path` holds only the package
        // segments — the `*` isn't a member name to store.
        assert_eq!(program.imports_use[0].path, vec!["kotlin", "test"]);
    }

    #[test]
    fn single_symbol_import_is_not_wildcard() {
        let tokens = tokenize("import a.b.greet\nprintln(1)").unwrap();
        let program = parse(&tokens).unwrap();
        assert!(!program.imports_use[0].wildcard);
    }

    #[test]
    fn wildcard_import_of_a_single_segment_package_parses() {
        // `import kotlin.*` — one package segment (`kotlin`) before `*`,
        // still legal (unlike a single-symbol import, a wildcard has no
        // 2-segment minimum: the member position is `*`, not a name).
        let tokens = tokenize("import kotlin.*\nprintln(1)").unwrap();
        let program = parse(&tokens).unwrap();
        assert!(program.imports_use[0].wildcard);
        assert_eq!(program.imports_use[0].path, vec!["kotlin"]);
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

    #[test]
    fn braceless_if_branch_accepts_a_return_statement() {
        // `if (cond) return "x"` with no braces — a bare-statement branch,
        // not just a bare-expression one (real Kotlin grammar: an if/when
        // branch is a `Statement`, and `return`/`break`/`continue` are
        // statements, not expressions, in ksl).
        let stmts = parse_src("fun f(): String {\nif (true) return \"x\"\nreturn \"y\"\n}\n");
        match &stmts[0].kind {
            StmtKind::FunDecl { body: FunBody::Block(body), .. } => match &body[0].kind {
                StmtKind::ExprStmt(e) => match &e.kind {
                    ExprKind::If { then_branch, else_branch: None, .. } => {
                        assert!(matches!(&then_branch[0].kind, StmtKind::Return(Some(_))));
                    }
                    other => panic!("unexpected {other:?}"),
                },
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn braceless_if_branch_accepts_a_continue_statement() {
        let stmts = parse_src("while (true) {\nif (true) continue\n}\n");
        match &stmts[0].kind {
            StmtKind::While { body, .. } => match &body[0].kind {
                StmtKind::ExprStmt(e) => match &e.kind {
                    ExprKind::If { then_branch, .. } => {
                        assert!(matches!(&then_branch[0].kind, StmtKind::Continue));
                    }
                    other => panic!("unexpected {other:?}"),
                },
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn postfix_increment_and_decrement_parse() {
        let stmts = parse_src("x++\ny--");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => assert!(
                matches!(&e.kind, ExprKind::IncDec { op: IncDecOp::Inc, name, prefix: false, .. } if name == "x")
            ),
            other => panic!("unexpected {other:?}"),
        }
        match &stmts[1].kind {
            StmtKind::ExprStmt(e) => assert!(
                matches!(&e.kind, ExprKind::IncDec { op: IncDecOp::Dec, name, prefix: false, .. } if name == "y")
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn prefix_increment_and_decrement_parse() {
        let stmts = parse_src("++x\n--y");
        match &stmts[0].kind {
            StmtKind::ExprStmt(e) => assert!(
                matches!(&e.kind, ExprKind::IncDec { op: IncDecOp::Inc, name, prefix: true, .. } if name == "x")
            ),
            other => panic!("unexpected {other:?}"),
        }
        match &stmts[1].kind {
            StmtKind::ExprStmt(e) => assert!(
                matches!(&e.kind, ExprKind::IncDec { op: IncDecOp::Dec, name, prefix: true, .. } if name == "y")
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn postfix_increment_inside_a_condition_parses() {
        // `x++ < 5` — postfix binds tighter than `<`, so this is
        // `(x++) < 5`, not a parse error.
        let stmts = parse_src("val ok = x++ < 5");
        match &stmts[0].kind {
            StmtKind::ValDecl { init, .. } => match &init.kind {
                ExprKind::Binary { op: BinaryOp::Lt, lhs, .. } => {
                    assert!(matches!(&lhs.kind, ExprKind::IncDec { prefix: false, .. }));
                }
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    // ---- span widening (see `Parser::prev_span`/`parse_stmt`) ----

    #[test]
    fn a_block_bodied_funs_span_covers_its_own_closing_brace() {
        // Regression test: `stmt.span` used to stop at just the `fun`
        // keyword itself -- `resolver::Resolver::open_scope` relies on the
        // real end here to bound the function's own scope tightly (see
        // `resolver`'s `symbols_in_scope_does_not_leak_into_the_blank_gap_
        // after_its_own_closing_brace`).
        let stmts = parse_src("fun f(x: Int): Int {\n    return x\n}\n");
        let span = stmts[0].span;
        assert_eq!((span.line, span.col), (1, 1), "should still start at 'fun'");
        assert_eq!((span.end_line, span.end_col), (3, 2), "should now end right after the closing '}}'");
    }

    #[test]
    fn an_expr_bodied_funs_span_covers_its_own_expression() {
        let stmts = parse_src("fun f(x: Int): Int = x + 1\n");
        let span = stmts[0].span;
        assert_eq!((span.end_line, span.end_col), (1, 27), "should end right after '1'");
    }

    #[test]
    fn a_whiles_span_covers_its_own_closing_brace() {
        let stmts = parse_src("while (true) {\n    break\n}\n");
        let span = stmts[0].span;
        assert_eq!((span.end_line, span.end_col), (3, 2));
    }

    #[test]
    fn a_fors_span_covers_its_own_closing_brace() {
        let stmts = parse_src("for (i in 1..3) {\n    println(i)\n}\n");
        let span = stmts[0].span;
        assert_eq!((span.end_line, span.end_col), (3, 2));
    }

    #[test]
    fn an_ifs_span_covers_its_else_branch_when_present() {
        let stmts = parse_src("if (true) {\n    1\n} else {\n    2\n}\n");
        let span = stmts[0].span;
        assert_eq!((span.end_line, span.end_col), (5, 2));
    }

    #[test]
    fn an_ifs_span_covers_just_its_then_branch_with_no_else() {
        let stmts = parse_src("if (true) {\n    1\n}\n");
        let span = stmts[0].span;
        assert_eq!((span.end_line, span.end_col), (3, 2));
    }

    #[test]
    fn a_whens_span_covers_its_own_closing_brace() {
        let stmts = parse_src("when (x) {\n    1 -> \"one\"\n    else -> \"other\"\n}\n");
        let span = stmts[0].span;
        assert_eq!((span.end_line, span.end_col), (4, 2));
    }

    #[test]
    fn a_lambdas_span_covers_its_own_closing_brace() {
        let stmts = parse_src("val f = { x ->\n    x + 1\n}\n");
        match &stmts[0].kind {
            StmtKind::ValDecl { init, .. } => {
                assert_eq!((init.span.end_line, init.span.end_col), (3, 2));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
