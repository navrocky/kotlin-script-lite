//! Static, single-file symbol resolution over an already-parsed [`Program`]
//! — no execution, no I/O. This mirrors the *scoping rules* the tree-walking
//! `interpreter` applies at runtime (see the `env.child()` call sites there)
//! but does it once, statically, over the AST, so an editor can answer
//! "what does this name refer to" and "what's declared here" without
//! running the script.
//!
//! `@file:Import`ed/`package`-declaring files are not indexed — this is
//! single-file only, so a name actually declared elsewhere is never found
//! by the scope-chain walk in `resolve_name`. Three different cases fall
//! out of that, each handled differently before a name is flagged as
//! undefined (see `resolve_name`):
//! - Stdlib functions like `println` are never declared in the AST at all
//!   (they're registered directly into the runtime `Env` by
//!   `stdlib::populate_globals`), so they're checked against
//!   `stdlib::STDLIB_SIGNATURES` instead.
//! - A name brought in by this file's own `import a.b.c` is known
//!   precisely — `resolve` reads `Program::imports_use` up front — so it's
//!   never flagged.
//! - A name that could only have come from a plain `@file:Import` (an
//!   *unknown* set of names from a file this resolver never reads) can't
//!   be told apart from a genuine typo, so `resolve_name` suppresses the
//!   diagnostic for *any* unresolved name, file-wide, whenever the file has
//!   at least one `@file:Import` — a real loss of precision, but better
//!   than flagging ordinary, correct multi-file code as broken.

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

/// A `following` hint (see `Resolver::walk_stmt`) meaning "there's no
/// concrete next sibling — extends to the end of the file." A scope's
/// upper bound is taken from `following`'s *start* fields (see
/// `Resolver::open_scope`: it's the position where the next construct
/// begins), so this sentinel's own start fields sit at "infinity" rather
/// than at the top of the file.
const FILE_END: Span = Span { line: u32::MAX, col: u32::MAX, end_line: u32::MAX, end_col: u32::MAX };

/// The global scope's own extent: the whole file.
const FILE_SPAN: Span = Span { line: 1, col: 1, end_line: u32::MAX, end_col: u32::MAX };

struct ScopeData {
    parent: Option<usize>,
    depth: u32,
    /// This scope's source extent — where a cursor position has to fall
    /// for `symbols_in_scope` to consider this scope active. Set directly
    /// at scope creation (see `Resolver::open_scope`) as "from this
    /// construct's own start, up to wherever the next sibling construct
    /// begins" (or end-of-file, if there is no next sibling) — necessary
    /// because neither `Stmt::span` nor `Expr::span` cover a whole
    /// construct (they anchor its *start* only, see AGENTS.md's "Span on
    /// every Expr/Stmt"), so there's no direct way to read a real
    /// brace-to-brace extent. Using the next sibling's start as the
    /// boundary is deliberately generous on the inside edge (e.g. an empty
    /// `while (true) { }` body's scope technically extends slightly past
    /// its closing brace, into whatever follows the loop) — safe for
    /// `symbols_in_scope`, whose worst case is an occasional over-broad
    /// suggestion, never a missing one. `symbol_at` is unaffected either
    /// way, since it matches on `occurrences`, not scope bounds.
    bounds: Span,
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
    /// Every `a.b.c` qualified-name expression's span and path. This
    /// resolver doesn't know what `a.b` resolves to (it's single-file, see
    /// the module doc comment) — recording *where* one appears is as far
    /// as it goes; resolving it against other files is a caller's job
    /// (`ksl-lsp`'s `package_index` module does this for hover/goto-def).
    qualified_refs: Vec<(Span, Vec<String>)>,
    /// Every `MethodCall`'s method-name span and name, recorded
    /// unconditionally (unlike `qualified_refs`, which is only populated
    /// when the receiver isn't a real local) — lets a caller offer hover
    /// text for a builtin method call on a genuine local receiver (e.g.
    /// `s.uppercase()`, where `s` resolves via `occurrences` but
    /// `uppercase` has no declaration site of its own to point at).
    method_refs: Vec<(Span, String)>,
}

impl SymbolTable {
    pub fn symbol_at(&self, line: u32, col: u32) -> Option<&Symbol> {
        self.occurrence_at(line, col).map(|(_, s)| s)
    }

    /// The `a.b.c` qualified-name expression (if any) covering
    /// `(line, col)`, with its full span and path segments.
    pub fn qualified_ref_at(&self, line: u32, col: u32) -> Option<(Span, &[String])> {
        self.qualified_refs
            .iter()
            .find(|(span, _)| span.contains_line_col(line, col))
            .map(|(span, path)| (*span, path.as_slice()))
    }

    /// The method name (if any) whose own name-span covers `(line, col)` —
    /// e.g. `uppercase` in `s.uppercase()`, regardless of whether `s` is a
    /// real local or turned out to be a package-qualified reference
    /// instead (see `method_refs`'s doc comment).
    pub fn method_ref_at(&self, line: u32, col: u32) -> Option<&str> {
        self.method_refs
            .iter()
            .find(|(span, _)| span.contains_line_col(line, col))
            .map(|(_, name)| name.as_str())
    }

    /// Every symbol declared directly at this file's top level (scope 0,
    /// always the global scope — see `resolve`). Used by callers indexing
    /// a `package`-declaring file's exports (`ksl-lsp`'s `package_index`);
    /// unrelated to `symbols_in_scope`, which is scoped to a cursor
    /// position within *this* file, not "what does this file export".
    pub fn top_level_symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.scopes[0].names.values().map(|&idx| &self.symbols[idx])
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
            .filter(|(_, s)| s.bounds.contains_line_col(line, col))
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
        table: SymbolTable {
            scopes: Vec::new(),
            symbols: Vec::new(),
            occurrences: Vec::new(),
            qualified_refs: Vec::new(),
            method_refs: Vec::new(),
        },
        diagnostics: Vec::new(),
        imported_names: program.imports_use.iter().map(|u| u.path.last().unwrap().clone()).collect(),
        has_file_imports: !program.imports.is_empty(),
    };
    let global = r.push_scope(None, 0, FILE_SPAN);
    r.walk_stmts(&program.stmts, global, FILE_END);
    ResolveResult { table: r.table, diagnostics: r.diagnostics }
}

struct Resolver {
    table: SymbolTable,
    diagnostics: Vec<RuntimeError>,
    /// Bare names brought in by this file's own `import a.b.c` (the last
    /// segment of each path) — never flagged as undefined, since exactly
    /// which names these introduce is known precisely (unlike plain
    /// `@file:Import`, see `has_file_imports`).
    imported_names: HashSet<String>,
    /// True if this file has any `@file:Import(...)` at all. Unlike
    /// `import`, `@file:Import` dumps an *unknown* set of names into scope
    /// (every top-level `fun`/`val` of a file this resolver never reads —
    /// see the module doc comment), so there's no way to tell a name it
    /// legitimately provides from a real typo. Blanket-suppresses the
    /// undefined-name diagnostic file-wide rather than risk false
    /// positives on every multi-file script — a real loss of precision
    /// (a genuine typo of a name unrelated to any import also goes
    /// unflagged), but a undefined-name check that's wrong on ordinary,
    /// correct multi-file code is worse than one that's merely incomplete.
    has_file_imports: bool,
}

impl Resolver {
    fn push_scope(&mut self, parent: Option<usize>, depth: u32, bounds: Span) -> usize {
        self.table.scopes.push(ScopeData { parent, depth, bounds, names: HashMap::new() });
        self.table.scopes.len() - 1
    }

    /// Creates a child scope for a construct that runs in its own
    /// `env.child()` at runtime (function/lambda bodies, loop/branch/block
    /// bodies — see the module doc comment), with its extent set up front:
    /// from `own_start` (this construct's own anchor span — the `fun`/
    /// `while`/`for`/`if` keyword, a lambda's `{`, etc.) to `following`
    /// (wherever whatever comes next, textually, begins).
    fn open_scope(&mut self, parent: usize, own_start: Span, following: Span) -> usize {
        let depth = self.table.scopes[parent].depth + 1;
        let bounds = Span::with_end(own_start.line, own_start.col, following.line, following.col);
        self.push_scope(Some(parent), depth, bounds)
    }

    /// Widens `scope`'s bounds to also include `span` — used for content
    /// that can legitimately fall outside the up-front `own_start..following`
    /// estimate (e.g. `resolve_name`'s widening for a use elsewhere isn't
    /// needed in practice, but stays defensive/harmless if ever called with
    /// something outside the initial estimate).
    fn touch(&mut self, scope: usize, span: Span) {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            let scope = &mut self.table.scopes[s];
            scope.bounds = scope.bounds.merge(&span);
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
    /// If nothing matches there, it's still not necessarily an error the
    /// way it would be at runtime — stdlib functions like `println` are
    /// never declared in the AST at all — so it's checked against the
    /// known stdlib names too before being flagged as a diagnostic.
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
        if crate::stdlib::is_known(name) || self.imported_names.contains(name) || self.has_file_imports {
            return;
        }
        self.diagnostics.push(RuntimeError::new(format!("undefined name '{name}'"), use_span));
    }

    /// If `expr` is a chain of bare-identifier `.` access (`a`, `a.b`,
    /// `a.b.c`, ...) whose base name does *not* resolve to a real local
    /// declaration in `scope`, returns the flattened dotted segments —
    /// mirrors `Interpreter::dotted_path_if_unresolved`'s runtime fallback
    /// so hover/goto-def still resolve a genuine package reference like
    /// `util.math.square`, while a receiver that *is* a real local (`s` in
    /// `s.uppercase()`) is left for the caller to walk normally instead, so
    /// hovering `s` still finds its declaration. Returns `None` for
    /// anything else too, including a chain containing a call partway
    /// through (`f().g`).
    fn dotted_path_if_not_local(&self, expr: &Expr, scope: usize) -> Option<Vec<String>> {
        match &expr.kind {
            ExprKind::Name(name) if !self.resolves_locally(scope, name) => Some(vec![name.clone()]),
            ExprKind::FieldAccess { receiver, field, .. } => {
                let mut path = self.dotted_path_if_not_local(receiver, scope)?;
                path.push(field.clone());
                Some(path)
            }
            _ => None,
        }
    }

    /// Read-only scope-chain lookup — like the first half of `resolve_name`,
    /// but without recording an occurrence or a diagnostic on failure (used
    /// by `dotted_path_if_not_local`, which needs to *ask* whether a name
    /// is local without committing to either interpretation yet).
    fn resolves_locally(&self, scope: usize, name: &str) -> bool {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            if self.table.scopes[s].names.contains_key(name) {
                return true;
            }
            cur = self.table.scopes[s].parent;
        }
        false
    }

    /// `following` is the span of whatever comes next, textually, after
    /// this whole statement list — used only to give the *last* statement
    /// in the list (if it opens a scope) a real upper bound; every earlier
    /// statement's own `following` is simply the next one in the list.
    fn walk_stmts(&mut self, stmts: &[Stmt], scope: usize, following: Span) {
        for (i, s) in stmts.iter().enumerate() {
            let next = stmts.get(i + 1).map(|s| s.span).unwrap_or(following);
            self.walk_stmt(s, scope, next);
        }
    }

    /// A block that opens its own child scope but declares nothing of its
    /// own beyond its statements (loop/branch/block bodies — contrast with
    /// `FunDecl`/`For`/`Lambda`, which also bind params/a loop variable
    /// into the new scope before walking its body).
    fn walk_block(&mut self, stmts: &[Stmt], parent: usize, open_at: Span, following: Span) {
        let child = self.open_scope(parent, open_at, following);
        self.walk_stmts(stmts, child, following);
    }

    fn walk_stmt(&mut self, stmt: &Stmt, scope: usize, following: Span) {
        self.touch(scope, stmt.span);
        match &stmt.kind {
            StmtKind::ExprStmt(e) => self.walk_expr(e, scope, following),
            StmtKind::ValDecl { name, name_span, init, mutable, type_hint } => {
                self.walk_expr(init, scope, following);
                let kind = if *mutable { SymbolKind::Var } else { SymbolKind::Val };
                let keyword = if *mutable { "var" } else { "val" };
                let sig = match type_hint {
                    Some(t) => format!("{keyword} {name}: {t}"),
                    None => format!("{keyword} {name}"),
                };
                self.define(scope, name, kind, *name_span, *mutable, sig);
            }
            StmtKind::Assign { name, value, .. } => {
                self.walk_expr(value, scope, following);
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
                let fn_scope = self.open_scope(scope, stmt.span, following);
                self.walk_params(params, fn_scope, following);
                match body {
                    FunBody::Expr(e) => self.walk_expr(e, fn_scope, following),
                    FunBody::Block(stmts) => self.walk_stmts(stmts, fn_scope, following),
                }
            }
            StmtKind::While { cond, body } => {
                self.walk_expr(cond, scope, following);
                self.walk_block(body, scope, stmt.span, following);
            }
            StmtKind::For { var_name, var_name_span, iterable, body } => {
                self.walk_expr(iterable, scope, following);
                let for_scope = self.open_scope(scope, stmt.span, following);
                let sig = format!("val {var_name} (loop variable)");
                self.define(for_scope, var_name, SymbolKind::Val, *var_name_span, false, sig);
                self.walk_stmts(body, for_scope, following);
            }
            StmtKind::Return(Some(e)) => self.walk_expr(e, scope, following),
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    /// Params share one scope with the function/lambda body (not a nested
    /// child of it) — matches `call_function`, which defines each param
    /// directly into `call_env` and then runs the body in that same env.
    /// A default value expression is walked before its own param is
    /// defined but after earlier params are, mirroring the order
    /// `call_function` evaluates defaults in.
    fn walk_params(&mut self, params: &[Param], fn_scope: usize, following: Span) {
        for p in params {
            if let Some(default) = &p.default {
                self.walk_expr(default, fn_scope, following);
            }
            let sig = match &p.type_hint {
                Some(t) => format!("param {}: {t}", p.name),
                None => format!("param {}", p.name),
            };
            self.define(fn_scope, &p.name, SymbolKind::Param, p.name_span, false, sig);
        }
    }

    fn walk_expr(&mut self, expr: &Expr, scope: usize, following: Span) {
        self.touch(scope, expr.span);
        match &expr.kind {
            ExprKind::IntLit(_) | ExprKind::DoubleLit(_) | ExprKind::BoolLit(_) => {}
            ExprKind::StringTemplate(parts) => {
                for part in parts {
                    if let StringPart::Expr(e) = part {
                        self.walk_expr(e, scope, following);
                    }
                }
            }
            ExprKind::Name(name) => self.resolve_name(scope, name, expr.span),
            // `receiver.field` — this resolver has no notion of packages or
            // (pre-Phase-5) object fields, so it can't tell a local field
            // access apart from a package-qualified reference any more
            // than the interpreter can at parse time (see
            // `ExprKind::FieldAccess`'s doc comment). Mirrors
            // `Interpreter::dotted_path_if_unresolved`'s approach: if the
            // whole chain is bare identifiers and its base name does *not*
            // resolve to a real local declaration, record it in
            // `qualified_refs` (never flagged as undefined — deferred to
            // `ksl-lsp`'s cross-file package index, or to the interpreter
            // at runtime) instead of walking it as an ordinary expression.
            ExprKind::FieldAccess { receiver, .. } => match self.dotted_path_if_not_local(expr, scope) {
                Some(path) => self.table.qualified_refs.push((expr.span, path)),
                None => self.walk_expr(receiver, scope, following),
            },
            ExprKind::MethodCall { receiver, method, method_span, args } => {
                self.table.method_refs.push((*method_span, method.clone()));
                match self.dotted_path_if_not_local(receiver, scope) {
                    Some(mut path) => {
                        path.push(method.clone());
                        self.table.qualified_refs.push((expr.span, path));
                    }
                    None => self.walk_expr(receiver, scope, following),
                }
                for a in args {
                    self.walk_expr(&a.value, scope, following);
                }
            }
            ExprKind::Unary { expr: inner, .. } => self.walk_expr(inner, scope, following),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs, scope, following);
                self.walk_expr(rhs, scope, following);
            }
            ExprKind::Range { start, end } => {
                self.walk_expr(start, scope, following);
                self.walk_expr(end, scope, following);
            }
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee, scope, following);
                for a in args {
                    self.walk_expr(&a.value, scope, following);
                }
            }
            ExprKind::Lambda { params, body } => {
                let lambda_scope = self.open_scope(scope, expr.span, following);
                self.walk_params(params, lambda_scope, following);
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
                self.walk_stmts(body, lambda_scope, following);
            }
            ExprKind::If { cond, then_branch, else_branch } => {
                self.walk_expr(cond, scope, following);
                self.walk_block(then_branch, scope, expr.span, following);
                if let Some(eb) = else_branch {
                    self.walk_block(eb, scope, expr.span, following);
                }
            }
            ExprKind::When { subject, arms } => {
                if let Some(s) = subject {
                    self.walk_expr(s, scope, following);
                }
                for arm in arms {
                    for cond in &arm.conditions {
                        match cond {
                            WhenCondition::Equals(e) | WhenCondition::Bool(e) => {
                                self.walk_expr(e, scope, following)
                            }
                        }
                    }
                    // `WhenArm` has no span of its own; the whole `when`
                    // expression's is the best available anchor.
                    self.walk_block(&arm.body, scope, expr.span, following);
                }
            }
            ExprKind::Block(stmts) => self.walk_block(stmts, scope, expr.span, following),
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
    fn stdlib_call_is_not_flagged_as_undefined() {
        // println isn't declared anywhere in the AST (it's a stdlib native,
        // registered directly into the runtime Env) — must not be flagged.
        let result = resolve_src("println(1)\n");
        assert!(result.diagnostics.is_empty());
        assert!(result.table.symbol_at(1, 1).is_none(), "println has no AST declaration to point at");
    }

    #[test]
    fn genuinely_undefined_name_is_flagged() {
        let result = resolve_src("println(nope)\n");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].message.contains("nope"));
        assert!(result.table.symbol_at(1, 9).is_none());
    }

    #[test]
    fn name_brought_in_by_import_is_not_flagged() {
        // `square` is never declared in this file's own AST — it only
        // exists via `import util.math.square` — so it must NOT be flagged,
        // unlike a real typo (the bug this regression-tests: reported live
        // after `package_basic.ksl` incorrectly flagged `square`).
        let result = resolve_src("import util.math.square\nprintln(square(5))\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn unresolved_name_is_suppressed_file_wide_when_file_has_any_file_import() {
        // `@file:Import` dumps an unknown set of names into scope (this
        // resolver never reads the imported file) — there's no way to tell
        // a name it legitimately provides from a genuine typo, so the
        // undefined-name check is suppressed for the whole file rather than
        // risk flagging correct code. This is the same bug the previous
        // test covers, but for plain @file:Import (no explicit `import`)
        // — the form that was already broken before package/import existed.
        let result = resolve_src("@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn import_alone_does_not_suppress_other_genuine_typos() {
        // Only @file:Import blanket-suppresses (its provided names are
        // truly unknowable here); `import` alone only whitelists the
        // specific name(s) it names, so an unrelated typo elsewhere in a
        // file that uses only `import` (no @file:Import) is still flagged.
        let result = resolve_src("import util.math.square\nprintln(totallyNotAThing)\n");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].message.contains("totallyNotAThing"));
    }

    #[test]
    fn qualified_ref_at_finds_a_qualified_name_expression() {
        let result = resolve_src("util.math.square(5)\n");
        let (span, path) = result.table.qualified_ref_at(1, 3).expect("should find the qualified ref");
        assert_eq!(path, &["util".to_string(), "math".to_string(), "square".to_string()]);
        assert_eq!(span.line, 1);
        assert_eq!(span.col, 1);
        // Not present outside the qualified-name expression's own span.
        assert!(result.table.qualified_ref_at(1, 25).is_none());
    }

    #[test]
    fn qualified_ref_at_finds_a_bare_field_access_too() {
        // `a.b` with no call at all — `ExprKind::FieldAccess`, not `MethodCall`.
        let result = resolve_src("println(util.math)\n");
        let (_, path) = result.table.qualified_ref_at(1, 10).expect("should find the qualified ref");
        assert_eq!(path, &["util".to_string(), "math".to_string()]);
    }

    #[test]
    fn method_call_on_a_real_local_resolves_the_receiver_not_a_qualified_name() {
        // `s` is a genuine local `val` — `s.uppercase()` must resolve `s`
        // back to its declaration (for hover/goto-def), not be treated as
        // a possible package-qualified reference the way `util.math.square`
        // would be (this resolver can't otherwise tell them apart, since
        // neither `s` nor `util` is declared any differently in the AST).
        let result = resolve_src("val s = \"hi\"\nprintln(s.uppercase())\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let decl = result.table.symbol_at(1, 5).unwrap();
        let use_ = result.table.symbol_at(2, 9).expect("s should resolve as a local use");
        assert!(std::ptr::eq(decl, use_));
        assert!(result.table.qualified_ref_at(2, 9).is_none());
    }

    #[test]
    fn top_level_symbols_lists_only_the_files_own_top_level_declarations() {
        let src = "val a = 1\nfun f(): Int {\n    val local = 2\n    return local\n}\n";
        let result = resolve_src(src);
        let names: HashSet<&str> = result.table.top_level_symbols().map(|s| s.name.as_str()).collect();
        assert_eq!(names, HashSet::from(["a", "f"]));
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

    #[test]
    fn symbols_in_scope_works_right_after_a_keyword_with_nothing_typed_yet() {
        // Regression test: this is the everyday completion-trigger shape
        // (cursor right after "return ", nothing typed after it yet) that
        // exposed the original bounding-box approach's gap — no span had
        // ever been "touched" at or past that trailing-whitespace position.
        let src = "fun f(p: Int): Int {\n    val inner = 2\n    return \n}\n";
        let result = resolve_src(src);
        // column 12 = right after "    return " (4 spaces + "return" + 1 space).
        let names: HashSet<&str> =
            result.table.symbols_in_scope(3, 12).into_iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains("inner"), "{names:?}");
        assert!(names.contains("p"), "{names:?}");
    }

    #[test]
    fn symbols_in_scope_does_not_leak_into_a_later_sibling_function() {
        // A position genuinely inside function b's body must not see
        // function a's locals — the "next sibling" boundary (function b's
        // own start) has to actually stop function a's scope somewhere.
        // (The blank/gap line *between* the two functions is a separate,
        // documented imprecision — see `ScopeData::bounds` — since there's
        // no real anchor for a's own closing brace to bound against; this
        // test only asserts the part that's actually guaranteed.)
        let src = "fun a() {\n    val onlyInA = 1\n}\n\nfun b() {\n    val onlyInB = 1\n}\n";
        let result = resolve_src(src);
        let names: HashSet<&str> =
            result.table.symbols_in_scope(6, 5).into_iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains("onlyInB"), "{names:?}");
        assert!(!names.contains("onlyInA"), "{names:?}");
    }
}
