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
use std::rc::Rc;

use crate::ast::*;
use crate::error::{RuntimeError, Span};
use crate::types::{self, Type};

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
    /// Statically inferred/declared type of a plain reference to this
    /// symbol — from an explicit type-hint annotation if there is one,
    /// else inferred from the initializer, matching Kotlin: `var x = 5`
    /// infers `Int` just as surely as `var x: Int = 5` does, and a later
    /// `x = "hi"` is a type error either way (see `walk_stmt`'s `Assign`
    /// arm, which enforces this whenever `ty != Unknown`, with no
    /// distinction between "explicitly annotated" and "merely inferred").
    /// For `Fun`, this is always `Type::Function` (consistent with
    /// `Type::from_value`'s collapse of a callable value) — the function's
    /// own return type is `return_type`, not this field.
    pub ty: Type,
    /// Only meaningful for `SymbolKind::Fun`: the function's declared
    /// return type (`Type::Unknown` if unannotated). `Type::Unknown` for
    /// every other kind.
    pub return_type: Type,
    /// Only meaningful for `SymbolKind::Fun`: one resolved type per
    /// declared parameter, in order. Empty for every other kind.
    pub param_types: Vec<Type>,
    /// Only meaningful for `SymbolKind::Fun`: one parameter name per entry
    /// of `param_types`, same order — lets a named-argument call
    /// (`f(a = 1)`) be checked by name instead of position. Empty for
    /// every other kind.
    pub param_names: Vec<String>,
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
    /// Every `MethodCall`'s method-name span paired with its receiver's
    /// statically inferred type — populated only when the receiver isn't a
    /// package-qualified reference (i.e. exactly when `qualified_refs`
    /// *isn't* populated for that same call), so a caller can additionally
    /// require the receiver type to match a candidate builtin method,
    /// rather than matching by method name alone.
    method_receiver_types: Vec<(Span, Type)>,
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

    /// The statically known receiver type (if any) of the `MethodCall`
    /// whose method-name span covers `(line, col)` — see
    /// `method_receiver_types`'s doc comment.
    pub fn method_receiver_type_at(&self, line: u32, col: u32) -> Option<&Type> {
        self.method_receiver_types
            .iter()
            .find(|(span, _)| span.contains_line_col(line, col))
            .map(|(_, ty)| ty)
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
    /// Type-compatibility problems found by the static type checker
    /// (initializer-vs-hint, reassignment-vs-declared-type, call-argument-
    /// vs-param-type, return-vs-declared-return-type) — kept *separate*
    /// from `diagnostics` (undefined-name, duplicate-declaration) so a
    /// caller (e.g. `ksl-cli`) can treat type problems as fatal without
    /// changing behavior for the pre-existing, deliberately-imprecise
    /// diagnostics.
    pub type_errors: Vec<RuntimeError>,
}

pub fn resolve(program: &Program) -> ResolveResult {
    let mut r = Resolver {
        table: SymbolTable {
            scopes: Vec::new(),
            symbols: Vec::new(),
            occurrences: Vec::new(),
            qualified_refs: Vec::new(),
            method_refs: Vec::new(),
            method_receiver_types: Vec::new(),
        },
        diagnostics: Vec::new(),
        type_errors: Vec::new(),
        imported_names: program.imports_use.iter().map(|u| u.path.last().unwrap().clone()).collect(),
        has_file_imports: !program.imports.is_empty(),
        generic_params: Vec::new(),
        known_classes: HashSet::new(),
        return_type_stack: Vec::new(),
    };
    let global = r.push_scope(None, 0, FILE_SPAN);
    r.walk_stmts(&program.stmts, global, FILE_END);
    ResolveResult { table: r.table, diagnostics: r.diagnostics, type_errors: r.type_errors }
}

struct Resolver {
    table: SymbolTable,
    diagnostics: Vec<RuntimeError>,
    type_errors: Vec<RuntimeError>,
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
    /// Names of generic type parameters in scope (from an enclosing
    /// declaration's own `<T, ...>` list) — always empty today, since no
    /// declaration has a `type_params` list yet; see
    /// `types::resolve_type_hint`'s doc comment for why this is threaded
    /// through now rather than added later.
    generic_params: Vec<String>,
    /// Class names known so far — always empty today, since no `class`
    /// declaration is parsed yet; see `types::resolve_type_hint`'s doc
    /// comment.
    known_classes: HashSet<Rc<str>>,
    /// Stack of enclosing functions'/lambdas' return-type contexts,
    /// pushed/popped around walking a body, so `Return` checks/collects
    /// against the right one even when nested. See `ReturnTypeContext`.
    return_type_stack: Vec<ReturnTypeContext>,
}

/// What to do with a `return`'s type, for the innermost enclosing fun/
/// lambda body currently being walked.
#[derive(Clone)]
enum ReturnTypeContext {
    /// An explicit return-type annotation exists — check each `return`
    /// against it immediately (existing behavior).
    Expected(Type),
    /// No annotation — collect every `return`'s type here instead;
    /// reconciled into one inferred type once the whole body has been
    /// walked (see `reconcile_types`).
    Collect(Vec<Type>),
}

/// The result of reconciling a set of candidate return types (every
/// `return` in one fun/lambda body, plus — when nothing in the body
/// unconditionally returns — its tail expression) into one.
enum Reconciled {
    /// No candidate had a concretely known type — nothing to infer from.
    Nothing,
    /// Every candidate that had a concrete type agreed on this one.
    Agreed(Type),
    /// At least two candidates disagreed.
    Disagreement(Type, Type),
}

/// `Type::Unknown` entries are never evidence for or against — they're
/// simply not counted. Used for both function and lambda return-type
/// inference (see `ReturnTypeContext::Collect`).
fn reconcile_types(candidates: &[Type]) -> Reconciled {
    let mut agreed: Option<Type> = None;
    for t in candidates {
        if *t == Type::Unknown {
            continue;
        }
        match &agreed {
            None => agreed = Some(t.clone()),
            Some(a) if a == t => {}
            Some(a) => return Reconciled::Disagreement(a.clone(), t.clone()),
        }
    }
    match agreed {
        Some(t) => Reconciled::Agreed(t),
        None => Reconciled::Nothing,
    }
}

/// Best-effort, single-level "does this statement always exit via
/// `return`" check — used only to decide whether a fun/lambda body's tail
/// expression is a *real* fallthrough candidate for return-type inference
/// or a `return`-shadowed no-op (see `ReturnTypeContext::Collect`'s
/// reconciliation). Not full reachability analysis — only recognizes the
/// single most common idiom (`if`/`when` where every branch's own last
/// statement is `return`); anything deeper (nested blocks, loops) is
/// conservatively treated as "might fall through", which only risks an
/// occasional spurious "returns both X and Unit" diagnostic in an unusual
/// shape, never a wrong inferred type silently accepted — an explicit
/// return-type annotation always sidesteps this entirely.
fn always_returns(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(_) => true,
        StmtKind::ExprStmt(e) => match &e.kind {
            ExprKind::If { then_branch, else_branch: Some(eb), .. } => {
                then_branch.last().map(always_returns).unwrap_or(false)
                    && eb.last().map(always_returns).unwrap_or(false)
            }
            ExprKind::When { arms, .. } => {
                arms.iter().any(|a| a.is_else)
                    && arms.iter().all(|a| a.body.last().map(always_returns).unwrap_or(false))
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Double)
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

    #[allow(clippy::too_many_arguments)]
    fn define(
        &mut self,
        scope: usize,
        name: &str,
        kind: SymbolKind,
        def_span: Span,
        mutable: bool,
        signature: String,
        ty: Type,
        return_type: Type,
        param_types: Vec<Type>,
        param_names: Vec<String>,
    ) -> usize {
        if self.table.scopes[scope].names.contains_key(name) {
            self.diagnostics.push(RuntimeError::new(
                format!("'{name}' is already declared in this scope"),
                def_span,
            ));
        }
        let idx = self.table.symbols.len();
        self.table.symbols.push(Symbol {
            name: name.to_string(),
            kind,
            def_span,
            mutable,
            signature,
            ty,
            return_type,
            param_types,
            param_names,
        });
        self.table.scopes[scope].names.insert(name.to_string(), idx);
        self.table.occurrences.push((def_span, idx));
        self.touch(scope, def_span);
        idx
    }

    /// Resolves a raw type-hint string (or `None`, meaning no annotation)
    /// to a `Type` using this resolver's current generic-parameter/known-
    /// class context (both always empty today — see `Resolver::generic_params`/
    /// `known_classes`'s doc comments).
    ///
    /// An **absent** hint (`None`) is genuinely dynamic and never an error.
    /// An explicitly-**given** hint that still resolves to `Unknown` — a
    /// typo, or a name that would be a real type if a later phase (classes,
    /// generics) had landed — has no legitimate meaning today, so it pushes
    /// an "unknown type" diagnostic at `hint_span` (expected to be `Some`
    /// exactly when `hint` is, since the parser always captures them
    /// together — see `Param::type_hint_span`).
    fn resolve_type_hint_here(&mut self, hint: Option<&str>, hint_span: Option<Span>) -> Type {
        let Some(h) = hint else { return Type::Unknown };
        let ty = types::resolve_type_hint(h, &self.generic_params, &self.known_classes);
        if ty == Type::Unknown {
            self.type_errors.push(RuntimeError::new(
                format!("unknown type '{h}'"),
                hint_span.unwrap_or_default(),
            ));
        }
        ty
    }

    /// Resolves a use of `name` by walking up the scope chain from `scope`,
    /// returning its statically known `Type` (`Unknown` if the name can't be
    /// resolved at all — including the "suppressed" cases below, where it's
    /// still not necessarily a real error). If nothing matches there, it's
    /// still not necessarily an error the way it would be at runtime —
    /// stdlib functions like `println` are never declared in the AST at all
    /// — so it's checked against the known stdlib names too before being
    /// flagged as a diagnostic.
    fn resolve_name(&mut self, scope: usize, name: &str, use_span: Span) -> Type {
        self.touch(scope, use_span);
        if let Some(idx) = self.resolve_symbol_index(scope, name) {
            self.table.occurrences.push((use_span, idx));
            return self.table.symbols[idx].ty.clone();
        }
        if crate::stdlib::is_known(name) || self.imported_names.contains(name) || self.has_file_imports {
            return Type::Unknown;
        }
        self.diagnostics.push(RuntimeError::new(format!("undefined name '{name}'"), use_span));
        Type::Unknown
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
            ExprKind::Name(name) if self.resolve_symbol(scope, name).is_none() => Some(vec![name.clone()]),
            ExprKind::FieldAccess { receiver, field, .. } => {
                let mut path = self.dotted_path_if_not_local(receiver, scope)?;
                path.push(field.clone());
                Some(path)
            }
            _ => None,
        }
    }

    /// Read-only scope-chain lookup by name, returning the matched `Symbol`
    /// — like the first half of `resolve_name`, but without recording an
    /// occurrence or a diagnostic on failure (used by
    /// `dotted_path_if_not_local`, which needs to *ask* whether a name is
    /// local without committing to either interpretation yet, and by
    /// static type-checking call sites that need more than just the type,
    /// e.g. a `Fun` symbol's `param_types`).
    fn resolve_symbol(&self, scope: usize, name: &str) -> Option<&Symbol> {
        self.resolve_symbol_index(scope, name).map(|idx| &self.table.symbols[idx])
    }

    fn resolve_symbol_index(&self, scope: usize, name: &str) -> Option<usize> {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            if let Some(&idx) = self.table.scopes[s].names.get(name) {
                return Some(idx);
            }
            cur = self.table.scopes[s].parent;
        }
        None
    }

    /// `following` is the span of whatever comes next, textually, after
    /// this whole statement list — used only to give the *last* statement
    /// in the list (if it opens a scope) a real upper bound; every earlier
    /// statement's own `following` is simply the next one in the list.
    ///
    /// Returns the statement list's own statically inferred "value type" —
    /// the last statement's type if it's a bare expression statement, else
    /// `Type::Unit` (an empty list also yields `Unit`) — mirroring
    /// `Interpreter::exec_block`'s identical runtime rule exactly, since
    /// every non-`ExprStmt` arm of `walk_stmt` below returns `Type::Unit`
    /// for itself.
    fn walk_stmts(&mut self, stmts: &[Stmt], scope: usize, following: Span) -> Type {
        let mut last = Type::Unit;
        for (i, s) in stmts.iter().enumerate() {
            let next = stmts.get(i + 1).map(|s| s.span).unwrap_or(following);
            last = self.walk_stmt(s, scope, next);
        }
        last
    }

    /// A block that opens its own child scope but declares nothing of its
    /// own beyond its statements (loop/branch/block bodies — contrast with
    /// `FunDecl`/`For`/`Lambda`, which also bind params/a loop variable
    /// into the new scope before walking its body). Returns the block's
    /// value type, see `walk_stmts`.
    fn walk_block(&mut self, stmts: &[Stmt], parent: usize, open_at: Span, following: Span) -> Type {
        let child = self.open_scope(parent, open_at, following);
        self.walk_stmts(stmts, child, following)
    }

    /// Returns this statement's own "value type" for `walk_stmts`'s
    /// last-statement rule: a bare expression statement yields its
    /// expression's type; every other statement kind yields `Type::Unit`
    /// (matching `exec_stmt`'s runtime shape — only `ExprStmt` ever
    /// contributes a real value to an enclosing block).
    fn walk_stmt(&mut self, stmt: &Stmt, scope: usize, following: Span) -> Type {
        self.touch(scope, stmt.span);
        match &stmt.kind {
            StmtKind::ExprStmt(e) => self.walk_expr(e, scope, following),
            StmtKind::ValDecl { name, name_span, init, mutable, type_hint, type_hint_span } => {
                let init_ty = self.walk_expr(init, scope, following);
                let kind = if *mutable { SymbolKind::Var } else { SymbolKind::Val };
                let keyword = if *mutable { "var" } else { "val" };
                let declared_ty = self.resolve_type_hint_here(type_hint.as_deref(), *type_hint_span);
                let ty = if declared_ty != Type::Unknown {
                    if init_ty != Type::Unknown && !types::is_assignable(&init_ty, &declared_ty) {
                        self.type_errors.push(RuntimeError::new(
                            format!("type mismatch: expected {}, got {}", declared_ty.name(), init_ty.name()),
                            stmt.span,
                        ));
                    }
                    declared_ty
                } else {
                    // No hint — infer from the initializer, matching
                    // Kotlin: `var x = 5` infers `Int` just as surely as
                    // `var x: Int = 5` does, and both are enforced on
                    // reassignment identically (see the `Assign` arm).
                    init_ty
                };
                // Hover text shows the type either way — explicitly
                // annotated or merely inferred — so `var count = 0` reads
                // as `var count: Int`, not a bare `var count` that hides
                // what was actually worked out for it. Only a genuinely
                // `Unknown` type (nothing could be inferred) falls back to
                // the bare form.
                let sig = match (type_hint, &ty) {
                    (Some(t), _) => format!("{keyword} {name}: {t}"),
                    (None, Type::Unknown) => format!("{keyword} {name}"),
                    (None, inferred) => format!("{keyword} {name}: {}", inferred.name()),
                };
                // A hint-less declaration whose initializer's type couldn't
                // be inferred at all has no type — a "type-committing site"
                // per the strict-typing decision, so this is an error, not
                // a silently-accepted `Unknown`.
                if type_hint.is_none() && ty == Type::Unknown {
                    self.type_errors.push(RuntimeError::new(
                        format!("cannot infer type of '{name}'; add an explicit type annotation"),
                        *name_span,
                    ));
                }
                self.define(scope, name, kind, *name_span, *mutable, sig, ty, Type::Unknown, Vec::new(), Vec::new());
                Type::Unit
            }
            StmtKind::Assign { name, op, value } => {
                let value_ty = self.walk_expr(value, scope, following);
                let target_ty =
                    self.resolve_symbol(scope, name).map(|s| s.ty.clone()).unwrap_or(Type::Unknown);
                self.resolve_name(scope, name, stmt.span);
                // Compound assignment (`+=` etc.) is out of scope for v1 —
                // its result type would need the same widening rules as
                // arithmetic, and isn't a plain "is the RHS compatible"
                // check the way `=` is. `target_ty` is whatever the
                // declaration's own type is — explicit hint or inferred
                // from its initializer, no distinction (see `ValDecl`
                // above) — so this enforces reassignment for both forms.
                if *op == AssignOp::Set
                    && target_ty != Type::Unknown
                    && value_ty != Type::Unknown
                    && !types::is_assignable(&value_ty, &target_ty)
                {
                    self.type_errors.push(RuntimeError::new(
                        format!("cannot assign {} to var '{name}' of type {}", value_ty.name(), target_ty.name()),
                        stmt.span,
                    ));
                }
                Type::Unit
            }
            StmtKind::FunDecl { name, name_span, params, return_type, return_type_span, body } => {
                let ret_ty = self.resolve_type_hint_here(return_type.as_deref(), *return_type_span);
                // Resolved quietly (no "unknown type" diagnostic here) —
                // `walk_params` below resolves each param's hint again,
                // at its own declaration site, which is where that
                // diagnostic belongs; resolving twice would double it up.
                // A `fun`'s params are mandatory-annotated (checked for
                // real, with a diagnostic, in `walk_params` below) so this
                // is never actually `Unknown` in practice once that error
                // is fixed — but computed the same way regardless.
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|p| match &p.type_hint {
                        Some(h) => types::resolve_type_hint(h, &self.generic_params, &self.known_classes),
                        None => Type::Unknown,
                    })
                    .collect();
                let param_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                // Defined in the *outer* scope (not the function's own
                // body scope) so both callers and recursive calls from
                // within the body resolve it — matches how `FunDecl`'s
                // runtime handler defines the function's name on the same
                // `env` its closure captures, mutated after capture. Its
                // `return_type` is provisional when there's no annotation
                // (patched to the real inferred value below, once the body
                // has been walked) — a recursive call to this same
                // function from within its own body sees `Unknown` for
                // that call's result in the meantime, never a false
                // rejection, just no check for that one call.
                let has_return_annotation = return_type.is_some();
                let idx = self.define(
                    scope,
                    name,
                    SymbolKind::Fun,
                    *name_span,
                    false,
                    format!("fun {name}({})", param_list(params)),
                    Type::Function,
                    ret_ty.clone(),
                    param_types,
                    param_names,
                );
                let fn_scope = self.open_scope(scope, stmt.span, following);
                self.walk_params(params, fn_scope, following, true);
                self.return_type_stack.push(if has_return_annotation {
                    ReturnTypeContext::Expected(ret_ty.clone())
                } else {
                    ReturnTypeContext::Collect(Vec::new())
                });
                let body_ty = match body {
                    FunBody::Expr(e) => self.walk_expr(e, fn_scope, following),
                    FunBody::Block(stmts) => self.walk_stmts(stmts, fn_scope, following),
                };
                let ctx = self.return_type_stack.pop().unwrap();
                let final_ret_ty = match ctx {
                    ReturnTypeContext::Expected(declared) => {
                        // An expression body (`fun f() = expr`) has no
                        // explicit `return` statement to check via the
                        // stack — the body's own type IS the return value,
                        // so check it directly here.
                        if matches!(body, FunBody::Expr(_))
                            && body_ty != Type::Unknown
                            && declared != Type::Unknown
                            && !types::is_assignable(&body_ty, &declared)
                        {
                            self.type_errors.push(RuntimeError::new(
                                format!(
                                    "return type mismatch: expected {}, got {}",
                                    declared.name(),
                                    body_ty.name()
                                ),
                                stmt.span,
                            ));
                        }
                        declared
                    }
                    ReturnTypeContext::Collect(mut seen) => {
                        // The tail expression only counts as a return-type
                        // candidate if the body could actually fall
                        // through to it — see `always_returns`'s doc
                        // comment for why an unconditional `return` (or an
                        // if/when where every branch returns) must not
                        // also contribute a phantom `Unit` candidate here.
                        let falls_through = match body {
                            FunBody::Expr(_) => true,
                            FunBody::Block(stmts) => {
                                !stmts.last().map(always_returns).unwrap_or(false)
                            }
                        };
                        if falls_through {
                            seen.push(body_ty.clone());
                        }
                        match reconcile_types(&seen) {
                            Reconciled::Agreed(t) => t,
                            Reconciled::Nothing => {
                                self.type_errors.push(RuntimeError::new(
                                    format!(
                                        "cannot infer return type of function '{name}'; add an explicit return type annotation"
                                    ),
                                    *name_span,
                                ));
                                Type::Unknown
                            }
                            Reconciled::Disagreement(a, b) => {
                                self.type_errors.push(RuntimeError::new(
                                    format!("function '{name}' returns both {} and {}", a.name(), b.name()),
                                    stmt.span,
                                ));
                                Type::Unknown
                            }
                        }
                    }
                };
                self.table.symbols[idx].return_type = final_ret_ty;
                Type::Unit
            }
            StmtKind::While { cond, body } => {
                self.walk_expr(cond, scope, following);
                self.walk_block(body, scope, stmt.span, following);
                Type::Unit
            }
            StmtKind::For { var_name, var_name_span, iterable, body } => {
                self.walk_expr(iterable, scope, following);
                // Ranges are always `Int..Int` (enforced at runtime too —
                // see `interpreter.rs`'s Range evaluation), so a loop
                // directly over a range literal has a known element type.
                // Anything else (`listOf(...)`, an arbitrary expression)
                // stays `Unknown` — full `List<T>` element-type tracking
                // is a separate, larger piece of work, out of scope here.
                let elem_ty = match &iterable.kind {
                    ExprKind::Range { .. } => Type::Int,
                    _ => Type::Unknown,
                };
                let for_scope = self.open_scope(scope, stmt.span, following);
                let sig = match &elem_ty {
                    Type::Unknown => format!("val {var_name} (loop variable)"),
                    t => format!("val {var_name}: {} (loop variable)", t.name()),
                };
                self.define(
                    for_scope,
                    var_name,
                    SymbolKind::Val,
                    *var_name_span,
                    false,
                    sig,
                    elem_ty,
                    Type::Unknown,
                    Vec::new(),
                    Vec::new(),
                );
                self.walk_stmts(body, for_scope, following);
                Type::Unit
            }
            StmtKind::Return(value) => {
                let ty = match value {
                    Some(e) => self.walk_expr(e, scope, following),
                    None => Type::Unit,
                };
                match self.return_type_stack.last().cloned() {
                    Some(ReturnTypeContext::Expected(expected)) => {
                        if ty != Type::Unknown
                            && expected != Type::Unknown
                            && !types::is_assignable(&ty, &expected)
                        {
                            self.type_errors.push(RuntimeError::new(
                                format!("return type mismatch: expected {}, got {}", expected.name(), ty.name()),
                                stmt.span,
                            ));
                        }
                    }
                    Some(ReturnTypeContext::Collect(_)) => {
                        if let Some(ReturnTypeContext::Collect(seen)) = self.return_type_stack.last_mut() {
                            seen.push(ty);
                        }
                    }
                    None => {}
                }
                Type::Unit
            }
            StmtKind::Break | StmtKind::Continue => Type::Unit,
        }
    }

    /// Params share one scope with the function/lambda body (not a nested
    /// child of it) — matches `call_function`, which defines each param
    /// directly into `call_env` and then runs the body in that same env.
    /// A default value expression is walked before its own param is
    /// defined but after earlier params are, mirroring the order
    /// `call_function` evaluates defaults in.
    /// `require_annotation`: `true` for a named `fun`'s parameters (must
    /// always carry an explicit type — checked here, immediately, with a
    /// `type_errors` diagnostic if missing); `false` for a lambda's
    /// (inferred from an expected type at the lambda's use site instead —
    /// see the `Lambda` arm of `walk_expr`, which is where that inference
    /// actually happens; this function only handles the "explicit hint
    /// given" case for a lambda, same as always).
    fn walk_params(&mut self, params: &[Param], fn_scope: usize, following: Span, require_annotation: bool) {
        for p in params {
            if let Some(default) = &p.default {
                self.walk_expr(default, fn_scope, following);
            }
            if require_annotation && p.type_hint.is_none() {
                self.type_errors.push(RuntimeError::new(
                    format!("parameter '{}' must have an explicit type annotation", p.name),
                    p.name_span,
                ));
            }
            let sig = match &p.type_hint {
                Some(t) => format!("param {}: {t}", p.name),
                None => format!("param {}", p.name),
            };
            let ty = self.resolve_type_hint_here(p.type_hint.as_deref(), p.type_hint_span);
            self.define(fn_scope, &p.name, SymbolKind::Param, p.name_span, false, sig, ty, Type::Unknown, Vec::new(), Vec::new());
        }
    }

    /// Walks `expr` (recording occurrences/diagnostics as before) and
    /// returns its statically inferred `Type` — syntax-directed and
    /// single-pass, exhaustive over every `ExprKind` (no wildcard arm).
    /// `Type::Unknown` covers anything not covered below (see
    /// `types`'s module doc comment for the "Unknown is always compatible"
    /// rule this feeds into).
    fn walk_expr(&mut self, expr: &Expr, scope: usize, following: Span) -> Type {
        self.touch(scope, expr.span);
        match &expr.kind {
            ExprKind::IntLit(_) => Type::Int,
            ExprKind::DoubleLit(_) => Type::Double,
            ExprKind::BoolLit(_) => Type::Bool,
            ExprKind::StringTemplate(parts) => {
                for part in parts {
                    if let StringPart::Expr(e) = part {
                        self.walk_expr(e, scope, following);
                    }
                }
                Type::String
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
            // A receiver that *is* a real local, though, is an immediate
            // error — no value has real fields until classes exist (same
            // wording as the runtime error this mirrors), unless the
            // receiver's own type is itself `Unknown` (an already-reported
            // problem elsewhere — don't cascade a second diagnostic onto
            // it here).
            ExprKind::FieldAccess { receiver, field, .. } => {
                match self.dotted_path_if_not_local(expr, scope) {
                    Some(path) => self.table.qualified_refs.push((expr.span, path)),
                    None => {
                        let recv_ty = self.walk_expr(receiver, scope, following);
                        if recv_ty != Type::Unknown {
                            self.type_errors.push(RuntimeError::new(
                                format!("'{}' has no field '{field}'", recv_ty.name()),
                                expr.span,
                            ));
                        }
                    }
                }
                Type::Unknown
            }
            ExprKind::MethodCall { receiver, method, method_span, args } => {
                self.table.method_refs.push((*method_span, method.clone()));
                let recv_ty = match self.dotted_path_if_not_local(receiver, scope) {
                    Some(mut path) => {
                        path.push(method.clone());
                        self.table.qualified_refs.push((expr.span, path));
                        None
                    }
                    None => Some(self.walk_expr(receiver, scope, following)),
                };
                if let Some(t) = &recv_ty {
                    self.table.method_receiver_types.push((*method_span, t.clone()));
                }
                for a in args {
                    self.walk_expr(&a.value, scope, following);
                }
                match &recv_ty {
                    // `Unknown` receiver: deferred, no immediate error —
                    // could be a real method once the receiver's type is
                    // itself resolvable.
                    Some(Type::Unknown) => Type::Unknown,
                    Some(t) => {
                        let known = crate::stdlib::BUILTIN_METHOD_SIGNATURES
                            .iter()
                            .any(|(recv_type, name, ..)| *recv_type == t.name() && *name == method);
                        if known {
                            crate::stdlib::builtin_method_return_type(t.name(), method)
                        } else {
                            self.type_errors.push(RuntimeError::new(
                                format!("'{}' has no method '{method}'", t.name()),
                                *method_span,
                            ));
                            Type::Unknown
                        }
                    }
                    None => Type::Unknown,
                }
            }
            ExprKind::Unary { op, expr: inner } => {
                let t = self.walk_expr(inner, scope, following);
                match (op, &t) {
                    (UnaryOp::Neg | UnaryOp::Pos, Type::Int) => Type::Int,
                    (UnaryOp::Neg | UnaryOp::Pos, Type::Double) => Type::Double,
                    (UnaryOp::Not, Type::Bool) => Type::Bool,
                    // `Unknown` (couldn't be determined) is never an
                    // immediate error here, only a concretely-known-but-
                    // wrong type — matches `apply_binary`'s runtime
                    // rejection of the exact same combinations (same
                    // wording, so the message reads the same whether
                    // caught statically or at runtime).
                    (_, Type::Unknown) => Type::Unknown,
                    _ => {
                        self.type_errors.push(RuntimeError::new(
                            format!("bad operand type for unary operator: {}", t.name()),
                            expr.span,
                        ));
                        Type::Unknown
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let l = self.walk_expr(lhs, scope, following);
                let r = self.walk_expr(rhs, scope, following);
                match op {
                    // `==`/`!=`/`&&`/`||` are total at runtime (structural
                    // equality/truthiness accept any value, never a type
                    // error — see `values_equal`/`is_truthy`), so these
                    // never get an immediate error here either, regardless
                    // of operand types.
                    BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::And | BinaryOp::Or => Type::Bool,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                        match (&l, &r) {
                            (Type::Unknown, _) | (_, Type::Unknown) => {}
                            (Type::String, Type::String) => {}
                            (a, b) if is_numeric(a) && is_numeric(b) => {}
                            _ => {
                                self.type_errors.push(RuntimeError::new(
                                    format!("cannot compare {} and {}", l.name(), r.name()),
                                    expr.span,
                                ));
                            }
                        }
                        Type::Bool
                    }
                    BinaryOp::Add => match (&l, &r) {
                        (Type::Int, Type::Int) => Type::Int,
                        (Type::Double, Type::Double) | (Type::Double, Type::Int) | (Type::Int, Type::Double) => {
                            Type::Double
                        }
                        (Type::String, _) | (_, Type::String) => Type::String,
                        (Type::List, _) | (_, Type::List) => Type::List,
                        (Type::Unknown, _) | (_, Type::Unknown) => Type::Unknown,
                        _ => {
                            self.type_errors.push(RuntimeError::new(
                                format!("unsupported operand types for '+': {} and {}", l.name(), r.name()),
                                expr.span,
                            ));
                            Type::Unknown
                        }
                    },
                    BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => match (&l, &r) {
                        (Type::Int, Type::Int) => Type::Int,
                        (Type::Double, Type::Double) | (Type::Double, Type::Int) | (Type::Int, Type::Double) => {
                            Type::Double
                        }
                        (Type::Unknown, _) | (_, Type::Unknown) => Type::Unknown,
                        _ => {
                            let sym = match op {
                                BinaryOp::Sub => "-",
                                BinaryOp::Mul => "*",
                                BinaryOp::Div => "/",
                                BinaryOp::Rem => "%",
                                _ => unreachable!(),
                            };
                            self.type_errors.push(RuntimeError::new(
                                format!("unsupported operand types for '{sym}': {} and {}", l.name(), r.name()),
                                expr.span,
                            ));
                            Type::Unknown
                        }
                    },
                }
            }
            ExprKind::Range { start, end } => {
                self.walk_expr(start, scope, following);
                self.walk_expr(end, scope, following);
                Type::List
            }
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee, scope, following);
                let mut arg_infos: Vec<(Option<String>, Type)> = Vec::with_capacity(args.len());
                for a in args {
                    let t = self.walk_expr(&a.value, scope, following);
                    arg_infos.push((a.name.clone(), t));
                }

                // `abs`/`max`/`min` are numeric-polymorphic (their return
                // type mirrors whichever numeric type the caller actually
                // passed) — doesn't fit the "one fixed Type" shape every
                // other signature below has, so it's handled directly here
                // instead of via `stdlib::stdlib_call_types`.
                if let ExprKind::Name(name) = &callee.kind {
                    if matches!(name.as_str(), "abs" | "max" | "min") {
                        return arg_infos
                            .first()
                            .map(|(_, t)| t.clone())
                            .filter(is_numeric)
                            .unwrap_or(Type::Unknown);
                    }
                }

                // A checkable signature comes from either a local `fun`
                // symbol or a stdlib function with a known machine-
                // readable signature (`STDLIB_SIGNATURES`'s own signature
                // field is a free-text display string, not this). Stdlib
                // params have no real names to match a named argument
                // against (`String::new()` placeholders — any named-arg
                // call to a stdlib function falls into the "unknown
                // parameter" case below, a minor, documented limitation).
                let signature = match &callee.kind {
                    ExprKind::Name(name) => self
                        .resolve_symbol(scope, name)
                        .filter(|s| s.kind == SymbolKind::Fun)
                        .map(|s| (s.return_type.clone(), s.param_types.clone(), s.param_names.clone()))
                        .or_else(|| {
                            crate::stdlib::stdlib_call_types(name)
                                .map(|(param_types, ret_ty)| {
                                    let names = vec![String::new(); param_types.len()];
                                    (ret_ty, param_types, names)
                                })
                        }),
                    _ => None,
                };

                match signature {
                    Some((ret_ty, param_types, param_names)) => {
                        // Two passes, mirroring `call_function`'s own
                        // argument-binding algorithm exactly: named
                        // arguments claim their declared slot first: only
                        // then are the remaining positional arguments
                        // matched, in order, against whichever param slots
                        // are still unclaimed.
                        let mut claimed = vec![false; param_types.len()];
                        for (arg_name, arg_ty) in &arg_infos {
                            let Some(pname) = arg_name else { continue };
                            match param_names.iter().position(|n| n == pname) {
                                Some(i) => {
                                    claimed[i] = true;
                                    if let Some(param_ty) = param_types.get(i) {
                                        if *arg_ty != Type::Unknown
                                            && *param_ty != Type::Unknown
                                            && !types::is_assignable(arg_ty, param_ty)
                                        {
                                            self.type_errors.push(RuntimeError::new(
                                                format!(
                                                    "argument '{pname}' has type {}, expected {}",
                                                    arg_ty.name(),
                                                    param_ty.name()
                                                ),
                                                expr.span,
                                            ));
                                        }
                                    }
                                }
                                None => {
                                    self.type_errors.push(RuntimeError::new(
                                        format!("unknown parameter '{pname}'"),
                                        expr.span,
                                    ));
                                }
                            }
                        }
                        let mut remaining = (0..param_types.len()).filter(|&i| !claimed[i]);
                        for (arg_name, arg_ty) in &arg_infos {
                            if arg_name.is_some() {
                                continue;
                            }
                            let Some(i) = remaining.next() else { continue };
                            if let Some(param_ty) = param_types.get(i) {
                                if *arg_ty != Type::Unknown
                                    && *param_ty != Type::Unknown
                                    && !types::is_assignable(arg_ty, param_ty)
                                {
                                    self.type_errors.push(RuntimeError::new(
                                        format!(
                                            "argument {} has type {}, expected {}",
                                            i + 1,
                                            arg_ty.name(),
                                            param_ty.name()
                                        ),
                                        expr.span,
                                    ));
                                }
                            }
                        }
                        ret_ty
                    }
                    None => Type::Unknown,
                }
            }
            ExprKind::Lambda { params, body } => {
                let lambda_scope = self.open_scope(scope, expr.span, following);
                // `require_annotation: false` — a lambda's params are
                // inferred from an expected type at its use site, not
                // required to carry an explicit hint (see Phase 2.2's
                // expected-type propagation; not yet wired in this pass).
                self.walk_params(params, lambda_scope, following, false);
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
                        Type::Unknown,
                        Type::Unknown,
                        Vec::new(),
                        Vec::new(),
                    );
                }
                // The lambda body's own inferred type is discarded — a
                // lambda's return type isn't tracked (no annotation syntax
                // exists for one); the lambda value itself is always
                // `Type::Function`.
                self.walk_stmts(body, lambda_scope, following);
                Type::Function
            }
            ExprKind::If { cond, then_branch, else_branch } => {
                self.walk_expr(cond, scope, following);
                let then_ty = self.walk_block(then_branch, scope, expr.span, following);
                let else_ty = match else_branch {
                    Some(eb) => self.walk_block(eb, scope, expr.span, following),
                    None => Type::Unit,
                };
                if then_ty == else_ty { then_ty } else { Type::Unknown }
            }
            ExprKind::When { subject, arms } => {
                if let Some(s) = subject {
                    self.walk_expr(s, scope, following);
                }
                // Join rule: same type on every arm (a missing `else`
                // implicitly joins with `Unit`, matching the runtime's "no
                // arm matched" fallthrough) → that type; any mismatch →
                // `Unknown`. `WhenArm` has no span of its own, so the whole
                // `when` expression's span is the anchor for each arm's
                // block.
                let has_else = arms.iter().any(|a| a.is_else);
                let mut acc: Option<Type> = if has_else { None } else { Some(Type::Unit) };
                for arm in arms {
                    for cond in &arm.conditions {
                        match cond {
                            WhenCondition::Equals(e) | WhenCondition::Bool(e) => {
                                self.walk_expr(e, scope, following);
                            }
                        }
                    }
                    let arm_ty = self.walk_block(&arm.body, scope, expr.span, following);
                    acc = Some(match acc {
                        None => arm_ty,
                        Some(prev) if prev == arm_ty => prev,
                        Some(_) => Type::Unknown,
                    });
                }
                acc.unwrap_or(Type::Unknown)
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

    // ---- static type checking ----

    #[test]
    fn initializer_vs_hint_mismatch_is_a_type_error() {
        let result = resolve_src("val x: Double = 5\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("expected Double, got Int"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn initializer_matching_hint_is_not_a_type_error() {
        let result = resolve_src("val x: Int = 5\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn hintless_val_infers_type_from_initializer() {
        let result = resolve_src("val x = 5\nprintln(x)\n");
        assert!(result.type_errors.is_empty());
        let sym = result.table.symbol_at(1, 5).unwrap();
        assert_eq!(sym.ty, Type::Int);
    }

    #[test]
    fn hintless_val_signature_shows_the_inferred_type_for_hover() {
        // The whole point of inferring a hint-less val/var's type is that
        // hover can show it — a bare "val x" would hide what was actually
        // worked out for it.
        let result = resolve_src("val x = 5\n");
        assert_eq!(result.table.symbol_at(1, 5).unwrap().signature, "val x: Int");

        let result = resolve_src("var y = \"hi\"\n");
        assert_eq!(result.table.symbol_at(1, 5).unwrap().signature, "var y: String");
    }

    #[test]
    fn genuinely_unknown_typed_val_signature_stays_bare() {
        // Nothing could be inferred (a package-qualified call result) —
        // the bare "val x" form is honest about that, not a fabricated
        // "val x: Unknown".
        let result = resolve_src("val x = util.math.square(1)\n");
        assert_eq!(result.table.symbol_at(1, 5).unwrap().signature, "val x");
    }

    #[test]
    fn incompatible_var_reassignment_is_a_type_error() {
        let result = resolve_src("var x: Int = 5\nx = \"hi\"\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("cannot assign String to var 'x' of type Int"));
    }

    #[test]
    fn hintless_var_reassignment_across_types_is_flagged_too() {
        // Matching Kotlin: `var x = 5` infers `Int` just as surely as
        // `var x: Int = 5` does, so a later `x = "hi"` is a type error
        // either way — no special leniency for the hint-less form.
        let result = resolve_src("var x = 5\nx = \"hi\"\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn hintless_var_with_uninferable_initializer_is_a_type_error() {
        // Strict-typing decision: a hint-less `var`/`val` is a "type-
        // committing site" — if nothing could be inferred from its
        // initializer either, that's an error requiring an explicit
        // annotation, not a silent `Unknown`.
        let result = resolve_src("var x = util.math.square(1)\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("cannot infer type of 'x'"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn reassignment_of_a_var_that_already_has_an_unknown_type_error_is_never_double_flagged() {
        // `x`'s hint doesn't resolve (reported once, "unknown type") *and*
        // its initializer is a package-qualified call the resolver can't
        // see through either — `ty` ends up `Unknown` this way instead.
        // Reassigning it must not pile on a second, redundant diagnostic —
        // the root cause was already reported at the declaration.
        let result = resolve_src("var x: Sting = util.math.square(1)\nx = \"hi\"\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("unknown type 'Sting'"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn compatible_var_reassignment_is_not_a_type_error() {
        let result = resolve_src("var x: Int = 5\nx = 10\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn hintless_var_reassignment_that_matches_the_inferred_type_is_fine() {
        let result = resolve_src("var x = 5\nx = 10\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    // ---- unknown/typo'd type annotations ----

    #[test]
    fn typo_d_val_type_hint_is_an_unknown_type_error() {
        let result = resolve_src("val x: INt = 5\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("unknown type 'INt'"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn typo_d_param_type_hint_is_an_unknown_type_error() {
        let result = resolve_src("fun f(x: Sting) { }\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("unknown type 'Sting'"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn typo_d_return_type_hint_is_an_unknown_type_error() {
        let result = resolve_src("fun f(): Voidd { }\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("unknown type 'Voidd'"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn unknown_type_error_points_at_the_hint_not_the_whole_declaration() {
        let result = resolve_src("val x: INt = 5\n");
        // "val x: " is 7 chars, so the hint "INt" starts at column 8.
        assert_eq!(result.type_errors[0].span.line, 1);
        assert_eq!(result.type_errors[0].span.col, 8);
    }

    #[test]
    fn positional_argument_type_mismatch_is_flagged() {
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(1, \"x\")\n";
        let result = resolve_src(src);
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("argument 2 has type String, expected Int"));
    }

    #[test]
    fn unknown_typed_argument_is_never_flagged() {
        // The argument's own type is Unknown (a package-qualified call
        // result) — must never be flagged, matching the "Unknown is always
        // compatible" rule.
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(1, util.math.square(2))\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn named_argument_type_mismatch_is_flagged() {
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(a = 1, b = \"x\")\n";
        let result = resolve_src(src);
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("argument 'b' has type String, expected Int"));
    }

    #[test]
    fn named_argument_matching_type_is_not_flagged() {
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(a = 1, b = 2)\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn unknown_named_argument_is_flagged() {
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(a = 1, c = 2)\n";
        let result = resolve_src(src);
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("unknown parameter 'c'"));
    }

    #[test]
    fn mixed_named_and_positional_arguments_fill_remaining_slots_in_order() {
        // Matches `call_function`'s own binding algorithm: `b` claims its
        // slot by name first, then the positional `1`/`3` fill whatever's
        // left, in declaration order (`a`, then `c`) — so this call is
        // `a=1, b="x", c=3`, all correctly typed.
        let src = "fun f(a: Int, b: String, c: Int): Int {\n    return a + c\n}\nf(1, b = \"x\", 3)\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn int_plus_double_infers_double() {
        let result = resolve_src("val x = 1 + 2.0\n");
        let sym = result.table.symbol_at(1, 5).unwrap();
        assert_eq!(sym.ty, Type::Double);
    }

    #[test]
    fn if_join_same_type_on_both_branches() {
        let result = resolve_src("val x = if (true) 1 else 2\n");
        assert_eq!(result.table.symbol_at(1, 5).unwrap().ty, Type::Int);
    }

    #[test]
    fn if_join_mismatched_branches_is_unknown() {
        let result = resolve_src("val x = if (true) 1 else \"two\"\n");
        assert_eq!(result.table.symbol_at(1, 5).unwrap().ty, Type::Unknown);
    }

    #[test]
    fn if_missing_else_joins_with_unit() {
        // then-branch is Int, implicit else is Unit -> mismatched -> Unknown.
        let result = resolve_src("val x = if (true) { 1 }\n");
        assert_eq!(result.table.symbol_at(1, 5).unwrap().ty, Type::Unknown);
        // then-branch is itself Unit (last stmt not a bare expr) -> matches implicit else.
        let result2 = resolve_src("val x = if (true) { val y = 1 }\n");
        assert_eq!(result2.table.symbol_at(1, 5).unwrap().ty, Type::Unit);
    }

    #[test]
    fn return_type_mismatch_is_flagged() {
        let result = resolve_src("fun f(): Int {\n    return \"x\"\n}\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("return type mismatch: expected Int, got String"));
    }

    #[test]
    fn return_type_match_is_not_flagged() {
        let result = resolve_src("fun f(): Int {\n    return 1\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn expression_body_return_type_mismatch_is_flagged() {
        let result = resolve_src("fun f(): Int = \"x\"\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn preexisting_diagnostics_are_unaffected_by_type_checking() {
        // undefined-name / duplicate-declaration diagnostics stay exactly
        // as they were — in `diagnostics`, never `type_errors`.
        let result = resolve_src("println(nope)\n");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.type_errors.is_empty());

        let result2 = resolve_src("val x = 1\nval x = 2\n");
        assert_eq!(result2.diagnostics.len(), 1);
        assert!(result2.type_errors.is_empty());
    }
}
