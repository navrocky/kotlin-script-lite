//! Static, single-file symbol resolution over an already-parsed [`Program`]
//! — no execution, no I/O. This mirrors the *scoping rules* the tree-walking
//! `interpreter` applies at runtime (see the `env.child()` call sites there)
//! but does it once, statically, over the AST, so an editor can answer
//! "what does this name refer to" and "what's declared here" without
//! running the script.
//!
//! `@file:Import`ed files are not indexed — this is single-file only; a
//! name that resolves to something declared in an imported file is simply
//! left unresolved rather than reported as an error (stdlib functions like
//! `println` are the common case of this: they're never declared anywhere
//! in the AST at all).

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::error::{RuntimeError, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Val,
    Var,
    Fun,
    Param,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Span of just the name at its declaration site.
    pub def_span: Span,
    pub mutable: bool,
    /// A short one-line description for hover, e.g. `fun add(a, b)` or
    /// `val x: Int`. Built at definition time, where the declaration's
    /// params/type hint are directly at hand.
    pub signature: String,
}

struct ScopeData {
    parent: Option<usize>,
    depth: u32,
    /// Bounding box over every span (definitions, uses, and nested scopes)
    /// encountered while walking this scope's contents. This is an
    /// approximation of the scope's true source extent: since neither
    /// `Stmt::span` nor `Expr::span` cover a whole construct (they anchor
    /// its *start* only — see AGENTS.md's "Span on every Expr/Stmt"), there
    /// is no existing span to read the real brace-to-brace extent from.
    /// The bounding box is exact at its corners (every span merged in is a
    /// real position) but can be tighter than the true scope on the inside
    /// edges — e.g. an empty `while (true) {}` body has no bounding box at
    /// all. Fine for `symbol_at` (unaffected) and good enough for
    /// `symbols_in_scope`'s current use (not wired to any LSP feature
    /// yet); revisit if that changes.
    bounds: Option<Span>,
    /// name -> index into `SymbolTable::symbols`. A later definition of the
    /// same name in the same scope overwrites the entry (shadowing within
    /// one scope is flagged as a diagnostic at definition time, see
    /// `Resolver::define`), matching last-write-wins the way a second
    /// `env.define` with the same name would at runtime.
    names: HashMap<String, usize>,
}

pub struct SymbolTable {
    scopes: Vec<ScopeData>,
    symbols: Vec<Symbol>,
    /// Every identifier occurrence — both declaration sites and uses —
    /// mapped to the symbol it refers to. Declaration-site spans are also
    /// occurrences of their own symbol, so hovering a declaration and
    /// hovering a use both go through `symbol_at`.
    occurrences: Vec<(Span, usize)>,
}

impl SymbolTable {
    pub fn symbol_at(&self, line: u32, col: u32) -> Option<&Symbol> {
        self.occurrence_at(line, col).map(|(_, s)| s)
    }

    /// Like `symbol_at`, but also returns the exact span of the occurrence
    /// under `(line, col)` — the specific use or declaration token there,
    /// as opposed to `Symbol::def_span` which always points at the
    /// declaration regardless of which occurrence was queried. Needed to
    /// highlight what's actually under the cursor (e.g. for a hover
    /// range), not where it was declared.
    pub fn occurrence_at(&self, line: u32, col: u32) -> Option<(Span, &Symbol)> {
        self.occurrences
            .iter()
            .find(|(span, _)| span.contains_line_col(line, col))
            .map(|(span, idx)| (*span, &self.symbols[*idx]))
    }

    /// Every symbol visible at `(line, col)`: those declared in the
    /// innermost enclosing scope plus every ancestor scope, nearest first,
    /// deduplicated by name (a nearer declaration shadows a same-named
    /// outer one).
    pub fn symbols_in_scope(&self, line: u32, col: u32) -> Vec<&Symbol> {
        let Some(mut scope_idx) = self.innermost_scope(line, col) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        loop {
            let scope = &self.scopes[scope_idx];
            for (name, &sym_idx) in &scope.names {
                if seen.insert(name.clone()) {
                    out.push(&self.symbols[sym_idx]);
                }
            }
            match scope.parent {
                Some(p) => scope_idx = p,
                None => break,
            }
        }
        out
    }

    fn innermost_scope(&self, line: u32, col: u32) -> Option<usize> {
        self.scopes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.bounds.is_some_and(|b| b.contains_line_col(line, col)))
            .max_by_key(|(_, s)| s.depth)
            .map(|(i, _)| i)
    }
}

pub struct ResolveResult {
    pub table: SymbolTable,
    pub diagnostics: Vec<RuntimeError>,
}

pub fn resolve(program: &Program) -> ResolveResult {
    let mut r = Resolver {
        table: SymbolTable { scopes: Vec::new(), symbols: Vec::new(), occurrences: Vec::new() },
        diagnostics: Vec::new(),
    };
    let global = r.push_scope(None, 0);
    r.walk_stmts(&program.stmts, global);
    ResolveResult { table: r.table, diagnostics: r.diagnostics }
}

struct Resolver {
    table: SymbolTable,
    diagnostics: Vec<RuntimeError>,
}

impl Resolver {
    fn push_scope(&mut self, parent: Option<usize>, depth: u32) -> usize {
        self.table.scopes.push(ScopeData { parent, depth, bounds: None, names: HashMap::new() });
        self.table.scopes.len() - 1
    }

    /// Expands `scope` and every ancestor's bounding box to include `span`
    /// — a child scope's contents are always within its parent's, so the
    /// expansion has to propagate up the whole chain to stay consistent.
    fn touch(&mut self, scope: usize, span: Span) {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            let scope = &mut self.table.scopes[s];
            scope.bounds = Some(match scope.bounds {
                Some(b) => b.merge(&span),
                None => span,
            });
            cur = scope.parent;
        }
    }

    fn define(
        &mut self,
        scope: usize,
        name: &str,
        kind: SymbolKind,
        def_span: Span,
        mutable: bool,
        signature: String,
    ) {
        if self.table.scopes[scope].names.contains_key(name) {
            self.diagnostics.push(RuntimeError::new(
                format!("'{name}' is already declared in this scope"),
                def_span,
            ));
        }
        let idx = self.table.symbols.len();
        self.table.symbols.push(Symbol { name: name.to_string(), kind, def_span, mutable, signature });
        self.table.scopes[scope].names.insert(name.to_string(), idx);
        self.table.occurrences.push((def_span, idx));
        self.touch(scope, def_span);
    }

    /// Resolves a use of `name` by walking up the scope chain from `scope`.
    /// Left silently unresolved if nothing matches — an undefined name
    /// isn't necessarily an error here the way it is at runtime (stdlib
    /// functions are never declared in the AST at all), so this doesn't
    /// emit a diagnostic.
    fn resolve_name(&mut self, scope: usize, name: &str, use_span: Span) {
        self.touch(scope, use_span);
        let mut cur = Some(scope);
        while let Some(s) = cur {
            if let Some(&idx) = self.table.scopes[s].names.get(name) {
                self.table.occurrences.push((use_span, idx));
                return;
            }
            cur = self.table.scopes[s].parent;
        }
    }

    fn walk_stmts(&mut self, stmts: &[Stmt], scope: usize) {
        for s in stmts {
            self.walk_stmt(s, scope);
        }
    }

    /// A new child scope for a construct that runs in its own `env.child()`
    /// at runtime (loop/branch/block bodies — see the module doc comment).
    fn walk_block(&mut self, stmts: &[Stmt], parent: usize) {
        let depth = self.table.scopes[parent].depth + 1;
        let child = self.push_scope(Some(parent), depth);
        self.walk_stmts(stmts, child);
    }

    fn walk_stmt(&mut self, stmt: &Stmt, scope: usize) {
        self.touch(scope, stmt.span);
        match &stmt.kind {
            StmtKind::ExprStmt(e) => self.walk_expr(e, scope),
            StmtKind::ValDecl { name, name_span, init, mutable, type_hint } => {
                self.walk_expr(init, scope);
                let kind = if *mutable { SymbolKind::Var } else { SymbolKind::Val };
                let keyword = if *mutable { "var" } else { "val" };
                let sig = match type_hint {
                    Some(t) => format!("{keyword} {name}: {t}"),
                    None => format!("{keyword} {name}"),
                };
                self.define(scope, name, kind, *name_span, *mutable, sig);
            }
            StmtKind::Assign { name, value, .. } => {
                self.walk_expr(value, scope);
                self.resolve_name(scope, name, stmt.span);
            }
            StmtKind::FunDecl { name, name_span, params, body } => {
                // Defined in the *outer* scope (not the function's own
                // body scope) so both callers and recursive calls from
                // within the body resolve it — matches how `FunDecl`'s
                // runtime handler defines the function's name on the same
                // `env` its closure captures, mutated after capture.
                let sig = format!("fun {name}({})", param_list(params));
                self.define(scope, name, SymbolKind::Fun, *name_span, false, sig);
                let depth = self.table.scopes[scope].depth + 1;
                let fn_scope = self.push_scope(Some(scope), depth);
                self.walk_params(params, fn_scope);
                match body {
                    FunBody::Expr(e) => self.walk_expr(e, fn_scope),
                    FunBody::Block(stmts) => self.walk_stmts(stmts, fn_scope),
                }
            }
            StmtKind::While { cond, body } => {
                self.walk_expr(cond, scope);
                self.walk_block(body, scope);
            }
            StmtKind::For { var_name, var_name_span, iterable, body } => {
                self.walk_expr(iterable, scope);
                let depth = self.table.scopes[scope].depth + 1;
                let for_scope = self.push_scope(Some(scope), depth);
                let sig = format!("val {var_name} (loop variable)");
                self.define(for_scope, var_name, SymbolKind::Val, *var_name_span, false, sig);
                self.walk_stmts(body, for_scope);
            }
            StmtKind::Return(Some(e)) => self.walk_expr(e, scope),
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    /// Params share one scope with the function/lambda body (not a nested
    /// child of it) — matches `call_function`, which defines each param
    /// directly into `call_env` and then runs the body in that same env.
    /// A default value expression is walked before its own param is
    /// defined but after earlier params are, mirroring the order
    /// `call_function` evaluates defaults in.
    fn walk_params(&mut self, params: &[Param], fn_scope: usize) {
        for p in params {
            if let Some(default) = &p.default {
                self.walk_expr(default, fn_scope);
            }
            let sig = match &p.type_hint {
                Some(t) => format!("param {}: {t}", p.name),
                None => format!("param {}", p.name),
            };
            self.define(fn_scope, &p.name, SymbolKind::Param, p.name_span, false, sig);
        }
    }

    fn walk_expr(&mut self, expr: &Expr, scope: usize) {
        self.touch(scope, expr.span);
        match &expr.kind {
            ExprKind::IntLit(_) | ExprKind::DoubleLit(_) | ExprKind::BoolLit(_) => {}
            ExprKind::StringTemplate(parts) => {
                for part in parts {
                    if let StringPart::Expr(e) = part {
                        self.walk_expr(e, scope);
                    }
                }
            }
            ExprKind::Name(name) => self.resolve_name(scope, name, expr.span),
            ExprKind::Unary { expr: inner, .. } => self.walk_expr(inner, scope),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs, scope);
                self.walk_expr(rhs, scope);
            }
            ExprKind::Range { start, end } => {
                self.walk_expr(start, scope);
                self.walk_expr(end, scope);
            }
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee, scope);
                for a in args {
                    self.walk_expr(&a.value, scope);
                }
            }
            ExprKind::Lambda { params, body } => {
                let depth = self.table.scopes[scope].depth + 1;
                let lambda_scope = self.push_scope(Some(scope), depth);
                self.walk_params(params, lambda_scope);
                if params.is_empty() {
                    // Implicit `it` — synthesized at call time with no
                    // token of its own to point at, so its "declaration"
                    // is the lambda itself.
                    self.define(
                        lambda_scope,
                        "it",
                        SymbolKind::Param,
                        expr.span,
                        false,
                        "param it (implicit lambda parameter)".to_string(),
                    );
                }
                self.walk_stmts(body, lambda_scope);
            }
            ExprKind::If { cond, then_branch, else_branch } => {
                self.walk_expr(cond, scope);
                self.walk_block(then_branch, scope);
                if let Some(eb) = else_branch {
                    self.walk_block(eb, scope);
                }
            }
            ExprKind::When { subject, arms } => {
                if let Some(s) = subject {
                    self.walk_expr(s, scope);
                }
                for arm in arms {
                    for cond in &arm.conditions {
                        match cond {
                            WhenCondition::Equals(e) | WhenCondition::Bool(e) => self.walk_expr(e, scope),
                        }
                    }
                    self.walk_block(&arm.body, scope);
                }
            }
            ExprKind::Block(stmts) => self.walk_block(stmts, scope),
        }
    }
}

fn param_list(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| match &p.type_hint {
            Some(t) => format!("{}: {t}", p.name),
            None => p.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser};

    fn resolve_src(src: &str) -> ResolveResult {
        let tokens = lexer::tokenize(src).unwrap();
        let program = parser::parse(&tokens).unwrap();
        resolve(&program)
    }

    fn symbol_names_at(table: &SymbolTable, line: u32, col: u32) -> Option<(&str, SymbolKind)> {
        table.symbol_at(line, col).map(|s| (s.name.as_str(), s.kind))
    }

    #[test]
    fn resolves_a_use_back_to_its_val_declaration() {
        // line 1: val x = 1
        // line 2: println(x)
        let result = resolve_src("val x = 1\nprintln(x)\n");
        assert!(result.diagnostics.is_empty());
        // "x" used on line 2 at column 9 ("println(" is 9 chars, x is the 9th).
        let (name, kind) = symbol_names_at(&result.table, 2, 9).expect("x should resolve");
        assert_eq!(name, "x");
        assert_eq!(kind, SymbolKind::Val);
        // and it should resolve to the SAME symbol as the declaration on line 1.
        let decl_symbol = result.table.symbol_at(1, 5).unwrap();
        let use_symbol = result.table.symbol_at(2, 9).unwrap();
        assert!(std::ptr::eq(decl_symbol, use_symbol));
    }

    #[test]
    fn function_is_visible_to_its_own_recursive_call() {
        let result = resolve_src(
            "fun fact(n: Int): Int {\n    return n\n}\n",
        );
        assert!(result.diagnostics.is_empty());
        let (name, kind) = symbol_names_at(&result.table, 1, 5).expect("fact should resolve");
        assert_eq!(name, "fact");
        assert_eq!(kind, SymbolKind::Fun);
        assert_eq!(result.table.symbol_at(1, 5).unwrap().signature, "fun fact(n: Int)");
        let (pname, pkind) = symbol_names_at(&result.table, 1, 10).expect("n param should resolve");
        assert_eq!(pname, "n");
        assert_eq!(pkind, SymbolKind::Param);
        // the `n` used inside the body (line 2) should resolve to the param.
        let param_symbol = result.table.symbol_at(1, 10).unwrap();
        let use_symbol = result.table.symbol_at(2, 12).unwrap();
        assert!(std::ptr::eq(param_symbol, use_symbol));
    }

    #[test]
    fn nested_scopes_shadow_correctly_val_in_if_block_vs_outer() {
        let src = "val x = 1\nif (true) {\n    val x = 2\n    println(x)\n}\nprintln(x)\n";
        let result = resolve_src(src);
        assert!(result.diagnostics.is_empty());
        let outer_x = result.table.symbol_at(1, 5).unwrap();
        let inner_x = result.table.symbol_at(3, 9).unwrap();
        assert!(!std::ptr::eq(outer_x, inner_x), "inner val x should be a distinct symbol");

        let use_inside_if = result.table.symbol_at(4, 13).unwrap();
        assert!(std::ptr::eq(use_inside_if, inner_x), "use inside the if-block should resolve to the shadowing decl");

        let use_after_if = result.table.symbol_at(6, 9).unwrap();
        assert!(std::ptr::eq(use_after_if, outer_x), "use after the if-block should resolve back to the outer decl");
    }

    #[test]
    fn for_loop_variable_is_scoped_to_the_loop() {
        let result = resolve_src("for (i in 1..3) {\n    println(i)\n}\n");
        assert!(result.diagnostics.is_empty());
        let (name, kind) = symbol_names_at(&result.table, 1, 6).expect("i should resolve");
        assert_eq!(name, "i");
        assert_eq!(kind, SymbolKind::Val);
        assert!(result.table.symbol_at(2, 13).is_some(), "use of i inside the loop should resolve");
    }

    #[test]
    fn duplicate_declaration_in_same_scope_is_a_diagnostic() {
        let result = resolve_src("val x = 1\nval x = 2\n");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].message.contains('x'));
    }

    #[test]
    fn undefined_name_is_left_unresolved_not_an_error() {
        // println isn't declared anywhere in the AST (it's a stdlib native).
        let result = resolve_src("println(nope)\n");
        assert!(result.diagnostics.is_empty());
        assert!(result.table.symbol_at(1, 9).is_none());
    }

    #[test]
    fn dollar_shorthand_interpolation_resolves_like_a_name_use() {
        let result = resolve_src("val name = \"world\"\nprintln(\"hi $name\")\n");
        assert!(result.diagnostics.is_empty());
        let decl = result.table.symbol_at(1, 5).unwrap();
        // "hi $name" -> `$` is at column 14 on line 2 ('println("hi ' is 13 chars).
        let use_ = result.table.symbol_at(2, 14).expect("$name should resolve");
        assert!(std::ptr::eq(decl, use_));
    }

    #[test]
    fn symbols_in_scope_includes_locals_and_outer_scope() {
        let result = resolve_src("val outer = 1\nfun f(p: Int): Int {\n    val inner = 2\n    return inner\n}\n");
        // Query a position inside f's body (line 4, where `return inner` is).
        let names: HashSet<&str> =
            result.table.symbols_in_scope(4, 5).into_iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains("inner"));
        assert!(names.contains("p"));
        assert!(names.contains("f"));
        assert!(names.contains("outer"));
    }
}
