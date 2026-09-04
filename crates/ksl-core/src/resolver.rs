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
//!   `stdlib::root_symbols()` instead.
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
    /// Doc-comment text for hover/completion, already rendered to a final
    /// display string (mirrors `signature`'s "pre-rendered once" shape,
    /// no separate structured type on `Symbol`). For `Fun`/`Val`/`Var`:
    /// the parsed `/** ... */`'s summary, plus a `**Returns:**` line when
    /// an `@return` tag was present (see `walk_stmt`). For a named `fun`'s
    /// own `Param`: just that parameter's own `@param` description, pulled
    /// from the *function's* doc comment (see `walk_params`) — a lambda's
    /// parameters never get one, since a lambda can't carry its own KDoc
    /// comment. `None` whenever there's nothing worth showing (no comment,
    /// or a `doc_comment::ParsedDoc` that turned out entirely empty).
    pub doc: Option<String>,
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
    /// Every bare `ExprKind::Name` occurrence's span and text, recorded
    /// unconditionally — including names that never resolve to a local
    /// symbol at all (a stdlib function like `println`, or a name brought
    /// in flat by a package-less `@file:Import`, see `resolve_name`).
    /// `occurrences` only covers names that *do* resolve locally, so this
    /// is what a caller (`ksl-lsp`'s `hover`/`goto_definition`) falls back
    /// to for everything else — checking the name against
    /// `stdlib::root_symbols()` or a cross-file flat index, neither of
    /// which this single-file resolver knows about itself.
    name_refs: Vec<(Span, String)>,
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

    /// The bare name (if any) whose own `ExprKind::Name` span covers
    /// `(line, col)` — see `name_refs`'s doc comment. Note this fires for
    /// *every* name reference, including ones that also resolve to a real
    /// local symbol (`occurrence_at` would already find those); a caller
    /// should check `occurrence_at`/`qualified_ref_at` first and only use
    /// this as the final fallback.
    pub fn name_ref_at(&self, line: u32, col: u32) -> Option<&str> {
        self.name_refs
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
            name_refs: Vec::new(),
        },
        diagnostics: Vec::new(),
        type_errors: Vec::new(),
        imported_names: program.imports_use.iter().filter(|u| !u.wildcard).map(|u| u.path.last().unwrap().clone()).collect(),
        has_file_imports: !program.imports.is_empty(),
        has_wildcard_import: program.imports_use.iter().any(|u| u.wildcard),
        generic_params: Vec::new(),
        known_classes: HashSet::new(),
        return_type_stack: Vec::new(),
        narrowed: HashSet::new(),
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
    /// True if this file has any wildcard `import a.b.*`. Same reasoning
    /// and same blanket leniency as `has_file_imports` — a wildcard import
    /// brings in an unknown-to-this-resolver set of names (it has no
    /// access to the interpreter's runtime package envs), so this
    /// suppresses the undefined-name diagnostic file-wide rather than risk
    /// false positives on every name that wildcard import legitimately
    /// provides.
    has_wildcard_import: bool,
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
    /// Flow-sensitive null-safety smart-cast state: symbol indices (into
    /// `table.symbols`) currently proven non-null at this exact point in
    /// the walk, purely because of a `!= null`/`== null` check (and
    /// `&&`/`||`/`!` combinations of those) somewhere in the enclosing
    /// control flow — never because of the symbol's own *declared* type,
    /// which stays `Nullable` in `table.symbols` regardless. Read by
    /// `resolve_name` (the single place this changes an expression's
    /// observed type) and written by whichever construct establishes a
    /// narrowed region (`If`, short-circuit `Binary`, `While`, `When`) —
    /// see each one's own comment. Scoped by `walk_stmts`, which
    /// snapshots/restores this set around its own statement list, so a
    /// narrowing established inside one block never leaks to a sibling or
    /// outer block. Only ever contains indices of `!mutable` symbols
    /// (`val`/parameter) — matches Kotlin's own restriction against
    /// smart-casting a `var`, which could change between the check and the
    /// use.
    narrowed: HashSet<usize>,
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

/// Same shape as `always_returns` (best-effort, single-level), but for a
/// different question: "does this statement always exit the *normal
/// control flow*" — used only by null-safety smart-cast (see
/// `Resolver::narrowed`) to decide whether an `if (x == null) { ... }`
/// with no `else` narrows `x` for the rest of the enclosing block. Unlike
/// `always_returns`, this also counts `break`/`continue` (the other two
/// ways a statement can prevent fallthrough) — deliberately a *separate*
/// function rather than extending `always_returns` itself, since that one
/// means specifically "returns a value" for return-type inference, and
/// must keep meaning exactly that.
fn always_exits(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(_) | StmtKind::Break | StmtKind::Continue => true,
        StmtKind::ExprStmt(e) => match &e.kind {
            ExprKind::If { then_branch, else_branch: Some(eb), .. } => {
                then_branch.last().map(always_exits).unwrap_or(false)
                    && eb.last().map(always_exits).unwrap_or(false)
            }
            ExprKind::When { arms, .. } => {
                arms.iter().any(|a| a.is_else)
                    && arms.iter().all(|a| a.body.last().map(always_exits).unwrap_or(false))
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Double)
}

/// `true` for a primitive that can never have a "common supertype" with a
/// different primitive — used to decide whether mismatched `if`/`when`
/// branch types are an immediate error. `Class`/`Generic`/`Unknown`/
/// `Function` are excluded: computing a real common supertype needs a class
/// hierarchy that doesn't exist yet (deferred to a later phase, see the
/// approved plan's central-principle section), so those keep the existing
/// lenient join-to-`Unknown` behavior for now. `Any`/`Nothing` are excluded
/// too — `Nothing` always joins cleanly (see the `If`/`When` handling
/// below, checked before this function is even consulted) and `Any` isn't
/// a "concrete primitive" that can genuinely disagree with another one, the
/// same reasoning that already excludes `Class`/`Generic`/`Unknown`.
fn is_primitive(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Double | Type::Bool | Type::String | Type::Unit | Type::List)
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
        doc: Option<String>,
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
            doc,
        });
        self.table.scopes[scope].names.insert(name.to_string(), idx);
        self.table.occurrences.push((def_span, idx));
        self.touch(scope, def_span);
        idx
    }

    /// Resolves a type annotation (or `None`, meaning no annotation) to a
    /// `Type` using this resolver's current generic-parameter/known-class
    /// context (both always empty today — see `Resolver::generic_params`/
    /// `known_classes`'s doc comments), recursively diagnosing every
    /// unresolvable name anywhere in the tree (not just a top-level one) —
    /// e.g. `(Foo, Int) -> Bar` with both `Foo` and `Bar` undefined pushes
    /// two separate "unknown type" diagnostics, each pointing at its own
    /// name.
    ///
    /// An **absent** annotation (`None`) is genuinely dynamic and never an
    /// error. An explicitly-**given** name that still resolves to `Unknown`
    /// — a typo, or a name that would be a real type if a later phase
    /// (classes, generics) had landed — has no legitimate meaning today.
    fn resolve_type_annotation_here(&mut self, ann: Option<&TypeAnnotation>) -> Type {
        let Some(ann) = ann else { return Type::Unknown };
        self.diagnose_unknown_names(ann);
        types::resolve_type_annotation(ann, &self.generic_params, &self.known_classes)
    }

    /// Walks `ann`, pushing an "unknown type" diagnostic for every `Named`
    /// leaf that doesn't resolve — the recursive half of
    /// `resolve_type_annotation_here`, split out so it can run once ahead
    /// of the (non-diagnosing) `types::resolve_type_annotation` call rather
    /// than duplicating that function's own traversal.
    fn diagnose_unknown_names(&mut self, ann: &TypeAnnotation) {
        match ann {
            TypeAnnotation::Named(name, span) => {
                let ty = types::resolve_type_annotation(ann, &self.generic_params, &self.known_classes);
                if ty == Type::Unknown {
                    self.type_errors.push(RuntimeError::new(format!("unknown type '{name}'"), *span));
                }
            }
            TypeAnnotation::Function(params, ret, _) => {
                for p in params {
                    self.diagnose_unknown_names(p);
                }
                self.diagnose_unknown_names(ret);
            }
            TypeAnnotation::Nullable(inner, _) => self.diagnose_unknown_names(inner),
        }
    }

    /// Resolves a use of `name` by walking up the scope chain from `scope`,
    /// returning its statically known `Type` (`Unknown` if the name can't be
    /// resolved at all — including the "suppressed" cases below, where it's
    /// still not necessarily a real error). If nothing matches there, it's
    /// still not necessarily an error the way it would be at runtime —
    /// stdlib functions like `println` are never declared in the AST at all
    /// — so it's checked against the known stdlib names too before being
    /// flagged as a diagnostic.
    ///
    /// **Smart-cast integration point**: if the symbol's declared type is
    /// `Nullable(inner)` and its index is currently in `self.narrowed`, the
    /// *unwrapped* `inner` type is returned instead — this is the single
    /// place null-safety narrowing takes effect, so every consumer of a
    /// `Name`'s type (method/field access, arithmetic, comparisons, elvis,
    /// ...) benefits uniformly with no special-casing anywhere else. See
    /// `Resolver::narrowed`'s own doc comment for where narrowing is
    /// established.
    fn resolve_name(&mut self, scope: usize, name: &str, use_span: Span) -> Type {
        self.touch(scope, use_span);
        if let Some(idx) = self.resolve_symbol_index(scope, name) {
            self.table.occurrences.push((use_span, idx));
            let ty = self.table.symbols[idx].ty.clone();
            if let Type::Nullable(inner) = &ty {
                if self.narrowed.contains(&idx) {
                    return (**inner).clone();
                }
            }
            return ty;
        }
        if crate::stdlib::is_known(name) || self.imported_names.contains(name) || self.has_file_imports || self.has_wildcard_import {
            return Type::Unknown;
        }
        self.diagnostics.push(RuntimeError::new(format!("undefined name '{name}'"), use_span));
        Type::Unknown
    }

    /// Structural, read-only analysis of a boolean condition expression for
    /// null-safety smart-cast: returns `(positive, negative)` — the symbol
    /// indices proven non-null when `cond` evaluates to `true`, and when it
    /// evaluates to `false`, respectively. Recurses through `&&`/`||`/`!`
    /// so a compound condition like `x != null && y != null` narrows both;
    /// anything not recognized contributes no narrowing (always sound,
    /// just uninformative) rather than a false claim.
    fn narrowing_for_condition(&self, cond: &Expr, scope: usize) -> (HashSet<usize>, HashSet<usize>) {
        match &cond.kind {
            ExprKind::Binary { op: BinaryOp::NotEq, lhs, rhs } => match self.null_check_symbol(lhs, rhs, scope) {
                Some(idx) => (HashSet::from([idx]), HashSet::new()),
                None => (HashSet::new(), HashSet::new()),
            },
            ExprKind::Binary { op: BinaryOp::Eq, lhs, rhs } => match self.null_check_symbol(lhs, rhs, scope) {
                Some(idx) => (HashSet::new(), HashSet::from([idx])),
                None => (HashSet::new(), HashSet::new()),
            },
            ExprKind::Binary { op: BinaryOp::And, lhs, rhs } => {
                let (lpos, lneg) = self.narrowing_for_condition(lhs, scope);
                let (rpos, rneg) = self.narrowing_for_condition(rhs, scope);
                (lpos.union(&rpos).copied().collect(), lneg.intersection(&rneg).copied().collect())
            }
            ExprKind::Binary { op: BinaryOp::Or, lhs, rhs } => {
                let (lpos, lneg) = self.narrowing_for_condition(lhs, scope);
                let (rpos, rneg) = self.narrowing_for_condition(rhs, scope);
                (lpos.intersection(&rpos).copied().collect(), lneg.union(&rneg).copied().collect())
            }
            ExprKind::Unary { op: UnaryOp::Not, expr: inner } => {
                let (pos, neg) = self.narrowing_for_condition(inner, scope);
                (neg, pos)
            }
            _ => (HashSet::new(), HashSet::new()),
        }
    }

    /// If exactly one of `lhs`/`rhs` is the `null` literal and the other is
    /// a bare `Name` resolving to a narrowing-eligible symbol (immutable —
    /// `val`/parameter, matching Kotlin's own restriction against
    /// smart-casting a `var` — and declared `Nullable`), returns that
    /// symbol's index. This is the shared "is this an `x != null`/`x ==
    /// null` check" pattern behind both `narrowing_for_condition` arms
    /// (the caller decides which side — true or false — the match implies).
    fn null_check_symbol(&self, lhs: &Expr, rhs: &Expr, scope: usize) -> Option<usize> {
        let name_expr = match (&lhs.kind, &rhs.kind) {
            (ExprKind::NullLit, ExprKind::Name(_)) => rhs,
            (ExprKind::Name(_), ExprKind::NullLit) => lhs,
            _ => return None,
        };
        let ExprKind::Name(name) = &name_expr.kind else { return None };
        let idx = self.resolve_symbol_index(scope, name)?;
        let sym = &self.table.symbols[idx];
        if sym.mutable || !matches!(sym.ty, Type::Nullable(_)) {
            return None;
        }
        Some(idx)
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
    ///
    /// **Smart-cast scoping**: also snapshots/restores `self.narrowed`
    /// around the whole list — any narrowing established while walking
    /// these statements (a branch-local one from a nested `If`/`While`, or
    /// carry-forward from an early-exiting `if` earlier in *this* list) is
    /// reverted once the list finishes, so it never leaks to a sibling
    /// block or back out to the caller. Every statement list ultimately
    /// funnels through here (directly, or via `walk_block`), so this one
    /// snapshot/restore correctly scopes every narrowing source.
    fn walk_stmts(&mut self, stmts: &[Stmt], scope: usize, following: Span) -> Type {
        let narrowed_on_entry = self.narrowed.clone();
        let mut last = Type::Unit;
        for (i, s) in stmts.iter().enumerate() {
            let next = stmts.get(i + 1).map(|s| s.span).unwrap_or(following);
            last = self.walk_stmt(s, scope, next);
        }
        self.narrowed = narrowed_on_entry;
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
            StmtKind::ValDecl { name, name_span, init, mutable, type_annotation, doc_comment } => {
                let kind = if *mutable { SymbolKind::Var } else { SymbolKind::Val };
                let keyword = if *mutable { "var" } else { "val" };
                let declared_ty = self.resolve_type_annotation_here(type_annotation.as_ref());
                // Walked with the declared type as the "expected" type so a
                // lambda initializer (`val f: (Int) -> Int = { x -> ... }`)
                // can infer its own parameter types from it — see
                // `walk_expr_expected`'s doc comment.
                let expected = if matches!(declared_ty, Type::Function(..)) { Some(&declared_ty) } else { None };
                let init_ty = self.walk_expr_expected(init, scope, following, expected);
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
                // Hover text shows the resolved type either way —
                // explicitly annotated or merely inferred — via
                // `Type::name()` (which also renders a function type as
                // `(Int) -> Int`), so `var count = 0` reads as
                // `var count: Int`, not a bare `var count` that hides what
                // was actually worked out for it. Only a genuinely
                // `Unknown` type (nothing could be inferred) falls back to
                // the bare form.
                let sig = match &ty {
                    Type::Unknown => format!("{keyword} {name}"),
                    t => format!("{keyword} {name}: {}", t.name()),
                };
                // A hint-less declaration whose initializer's type couldn't
                // be inferred at all has no type — a "type-committing site"
                // per the strict-typing decision, so this is an error, not
                // a silently-accepted `Unknown`.
                if type_annotation.is_none() && ty == Type::Unknown {
                    self.type_errors.push(RuntimeError::new(
                        format!("cannot infer type of '{name}'; add an explicit type annotation"),
                        *name_span,
                    ));
                }
                // Only the summary matters for a `val`/`var` — `@param`/
                // `@return` tags aren't meaningful here (a plain value has
                // neither), so they're simply ignored if present.
                let doc = doc_comment
                    .as_deref()
                    .map(crate::doc_comment::parse)
                    .map(|d| d.summary)
                    .filter(|s| !s.is_empty());
                self.define(scope, name, kind, *name_span, *mutable, sig, ty, Type::Unknown, Vec::new(), Vec::new(), doc);
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
            StmtKind::FunDecl { name, name_span, params, return_type, body, doc_comment } => {
                // Parsed once, up front: `fun_doc` (the function's own
                // hover doc, summary + an optional `**Returns:**` line) is
                // consumed right here at `define()`; `parsed_doc` itself
                // stays alive for `walk_params` below, which pulls each
                // parameter's own description out of its `@param` tags.
                let parsed_doc = doc_comment.as_deref().map(crate::doc_comment::parse);
                let fun_doc = parsed_doc.as_ref().and_then(|d| {
                    if d.summary.is_empty() && d.returns.is_none() && d.params.is_empty() {
                        None
                    } else {
                        let mut parts = Vec::new();
                        if !d.summary.is_empty() {
                            parts.push(d.summary.clone());
                        }
                        // Each parameter's own description also appears on
                        // its own `Symbol` (see `walk_params` below, which
                        // is where hovering the *parameter itself* picks
                        // this same text up) — repeated here too so
                        // hovering the *function* shows the full picture
                        // without needing to hover each parameter one by
                        // one, same as real Kotlin IDEs' quick-doc popup.
                        if !d.params.is_empty() {
                            let mut block = String::from("**Parameters:**");
                            for (name, desc) in &d.params {
                                block.push_str(&format!("\n- `{name}` — {desc}"));
                            }
                            parts.push(block);
                        }
                        if let Some(returns) = &d.returns {
                            parts.push(format!("**Returns:** {returns}"));
                        }
                        Some(parts.join("\n\n"))
                    }
                });
                let ret_ty = self.resolve_type_annotation_here(return_type.as_ref());
                // Resolved quietly (no "unknown type" diagnostic here) —
                // `walk_params` below resolves each param's annotation
                // again, at its own declaration site, which is where that
                // diagnostic belongs; resolving twice would double it up.
                // A `fun`'s params are mandatory-annotated (checked for
                // real, with a diagnostic, in `walk_params` below) so this
                // is never actually `Unknown` in practice once that error
                // is fixed — but computed the same way regardless.
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|p| match &p.type_annotation {
                        Some(ann) => types::resolve_type_annotation(ann, &self.generic_params, &self.known_classes),
                        None => Type::Unknown,
                    })
                    .collect();
                let param_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                // Defined in the *outer* scope (not the function's own
                // body scope) so both callers and recursive calls from
                // within the body resolve it — matches how `FunDecl`'s
                // runtime handler defines the function's name on the same
                // `env` its closure captures, mutated after capture. Its
                // `return_type` (and the `Type::Function` stored as `ty`)
                // is provisional when there's no annotation (patched to the
                // real inferred value below, once the body has been walked)
                // — a recursive call to this same function from within its
                // own body sees `Unknown` for that call's result in the
                // meantime, never a false rejection, just no check for that
                // one call.
                let has_return_annotation = return_type.is_some();
                let idx = self.define(
                    scope,
                    name,
                    SymbolKind::Fun,
                    *name_span,
                    false,
                    format!("fun {name}({})", param_list(params)),
                    Type::Function(param_types.clone(), Box::new(ret_ty.clone())),
                    ret_ty.clone(),
                    param_types.clone(),
                    param_names,
                    fun_doc,
                );
                let fn_scope = self.open_scope(scope, stmt.span, following);
                self.walk_params(params, fn_scope, following, parsed_doc.as_ref());
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
                self.table.symbols[idx].ty = Type::Function(param_types, Box::new(final_ret_ty.clone()));
                // The signature built at `define()` time above (before the
                // body was walked) can't have known this yet — an
                // unannotated return type is only ever inferred *after*
                // walking the body. Bare (no `: Type` at all) only when
                // `final_ret_ty` is itself `Unknown` — always an already-
                // reported error state here (return-type inference failed
                // or disagreed), never a legitimate "genuinely no return
                // type" case (`Unit` is shown explicitly, same as anywhere
                // else) — mirrors `ValDecl`'s identical bare-vs-typed
                // signature choice for a hint-less `val`.
                self.table.symbols[idx].signature = match &final_ret_ty {
                    Type::Unknown => format!("fun {name}({})", param_list(params)),
                    t => format!("fun {name}({}): {}", param_list(params), t.name()),
                };
                self.table.symbols[idx].return_type = final_ret_ty;
                Type::Unit
            }
            StmtKind::While { cond, body } => {
                self.walk_expr(cond, scope, following);
                // Same null-safety smart-cast as `If`'s then-branch: the
                // body only ever runs when `cond` is true.
                let (pos, _neg) = self.narrowing_for_condition(cond, scope);
                let ambient = self.narrowed.clone();
                self.narrowed.extend(pos);
                self.walk_block(body, scope, stmt.span, following);
                self.narrowed = ambient;
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
                    None,
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

    /// Params share one scope with the function body (not a nested child of
    /// it) — matches `call_function`, which defines each param directly
    /// into `call_env` and then runs the body in that same env. A default
    /// value expression is walked before its own param is defined but
    /// after earlier params are, mirroring the order `call_function`
    /// evaluates defaults in.
    ///
    /// Only ever called for a named `fun`'s parameters, which must always
    /// carry an explicit type annotation (checked here, immediately, with
    /// a `type_errors` diagnostic if missing) — a lambda's parameters go
    /// through `walk_lambda_params` instead, which infers a missing
    /// annotation from an expected type at the lambda's use site rather
    /// than requiring one outright.
    fn walk_params(
        &mut self,
        params: &[Param],
        fn_scope: usize,
        following: Span,
        fun_doc: Option<&crate::doc_comment::ParsedDoc>,
    ) {
        for p in params {
            if let Some(default) = &p.default {
                self.walk_expr(default, fn_scope, following);
            }
            if p.type_annotation.is_none() {
                self.type_errors.push(RuntimeError::new(
                    format!("parameter '{}' must have an explicit type annotation", p.name),
                    p.name_span,
                ));
            }
            let ty = self.resolve_type_annotation_here(p.type_annotation.as_ref());
            let sig = match &ty {
                Type::Unknown => format!("param {}", p.name),
                t => format!("param {}: {}", p.name, t.name()),
            };
            // Pulled from the enclosing function's own `@param` tag, if
            // any named this parameter — a param has no doc-comment syntax
            // of its own (see `ast::Param`'s doc comment).
            let doc = fun_doc.and_then(|d| d.param(&p.name)).map(str::to_string);
            self.define(fn_scope, &p.name, SymbolKind::Param, p.name_span, false, sig, ty, Type::Unknown, Vec::new(), Vec::new(), doc);
        }
    }

    /// A lambda's parameters, unlike a named `fun`'s (`walk_params`), are
    /// never required to carry an explicit annotation — each one's type
    /// comes from (in order of preference) its own explicit annotation (a
    /// lambda param *can* be annotated, same syntax as a `fun` param's),
    /// else the corresponding slot of `expected_param_types` (the
    /// surrounding context's declared function type — a `val`/`var`
    /// annotation, or the matching parameter's type at the lambda's call
    /// site; see `walk_expr_expected`), else — a genuine "type-committing
    /// site" per the strict-typing decision — a `type_errors` diagnostic,
    /// same as a hint-less `val` with an uninferable initializer.
    ///
    /// The implicit `it` parameter (an empty `params` list) is handled the
    /// same way, using `expected_param_types` only when it names exactly
    /// one parameter (any other arity means `it`'s type genuinely can't be
    /// worked out from context either).
    ///
    /// Returns the resolved parameter types, in order — needed by the
    /// caller (`walk_lambda`) to build the lambda's own `Type::Function`.
    fn walk_lambda_params(
        &mut self,
        params: &[Param],
        fn_scope: usize,
        lambda_span: Span,
        following: Span,
        expected_param_types: Option<&[Type]>,
    ) -> Vec<Type> {
        if params.is_empty() {
            let inferred = expected_param_types
                .filter(|p| p.len() == 1)
                .map(|p| p[0].clone())
                .unwrap_or(Type::Unknown);
            if inferred == Type::Unknown {
                self.type_errors.push(RuntimeError::new(
                    "cannot infer type of lambda parameter 'it'; add an explicit expected function type"
                        .to_string(),
                    lambda_span,
                ));
            }
            let sig = match &inferred {
                Type::Unknown => "param it (implicit lambda parameter)".to_string(),
                t => format!("param it: {} (implicit lambda parameter)", t.name()),
            };
            self.define(
                fn_scope,
                "it",
                SymbolKind::Param,
                lambda_span,
                false,
                sig,
                inferred.clone(),
                Type::Unknown,
                Vec::new(),
                Vec::new(),
                None,
            );
            return vec![inferred];
        }

        let mut param_types = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            if let Some(default) = &p.default {
                self.walk_expr(default, fn_scope, following);
            }
            let explicit = self.resolve_type_annotation_here(p.type_annotation.as_ref());
            let inferred = if explicit != Type::Unknown {
                explicit
            } else {
                expected_param_types.and_then(|v| v.get(i)).cloned().unwrap_or(Type::Unknown)
            };
            if inferred == Type::Unknown {
                self.type_errors.push(RuntimeError::new(
                    format!(
                        "cannot infer type of lambda parameter '{}'; add an explicit expected function type",
                        p.name
                    ),
                    p.name_span,
                ));
            }
            let sig = match &inferred {
                Type::Unknown => format!("param {}", p.name),
                t => format!("param {}: {}", p.name, t.name()),
            };
            self.define(fn_scope, &p.name, SymbolKind::Param, p.name_span, false, sig, inferred.clone(), Type::Unknown, Vec::new(), Vec::new(), None);
            param_types.push(inferred);
        }
        param_types
    }

    /// A lambda's own type is always a real `Type::Function` — inferred,
    /// never optionally absent, matching the strict-typing decision that a
    /// lambda "always has its own type, e.g. `(Int, String) -> Result`".
    /// Parameter types come from `walk_lambda_params` (explicit annotation,
    /// else `expected`'s corresponding slot, else a diagnosed error); the
    /// return type is inferred from the body the same way an unannotated
    /// `fun`'s is (see `FunDecl`'s `ReturnTypeContext::Collect` handling,
    /// which this mirrors) — every `return` in the body plus, when nothing
    /// unconditionally returns, the tail expression, all reconciled into
    /// one type or flagged as a disagreement/uninferable error.
    fn walk_lambda(
        &mut self,
        expr: &Expr,
        params: &[Param],
        body: &[Stmt],
        scope: usize,
        following: Span,
        expected: Option<&Type>,
    ) -> Type {
        let expected_params: Option<&[Type]> = match expected {
            Some(Type::Function(p, _)) => Some(p.as_slice()),
            _ => None,
        };
        let lambda_scope = self.open_scope(scope, expr.span, following);
        let param_types = self.walk_lambda_params(params, lambda_scope, expr.span, following, expected_params);

        self.return_type_stack.push(ReturnTypeContext::Collect(Vec::new()));
        let body_ty = self.walk_stmts(body, lambda_scope, following);
        let ReturnTypeContext::Collect(mut seen) = self.return_type_stack.pop().unwrap() else {
            unreachable!("just pushed a Collect context")
        };
        let falls_through = !body.last().map(always_returns).unwrap_or(false);
        if falls_through {
            seen.push(body_ty);
        }
        let inferred_ret = match reconcile_types(&seen) {
            Reconciled::Agreed(t) => t,
            Reconciled::Nothing => {
                self.type_errors.push(RuntimeError::new(
                    "cannot infer return type of lambda; add an explicit expected function type".to_string(),
                    expr.span,
                ));
                Type::Unknown
            }
            Reconciled::Disagreement(a, b) => {
                self.type_errors.push(RuntimeError::new(
                    format!("lambda returns both {} and {}", a.name(), b.name()),
                    expr.span,
                ));
                Type::Unknown
            }
        };
        Type::Function(param_types, Box::new(inferred_ret))
    }

    /// Walks `expr` (recording occurrences/diagnostics as before) and
    /// returns its statically inferred `Type` — syntax-directed and
    /// single-pass, exhaustive over every `ExprKind` (no wildcard arm).
    /// `Type::Unknown` covers anything not covered below (see
    /// `types`'s module doc comment for the "Unknown is always compatible"
    /// rule this feeds into). A thin wrapper over `walk_expr_expected` with
    /// no expected type — see its doc comment for when a caller needs one.
    fn walk_expr(&mut self, expr: &Expr, scope: usize, following: Span) -> Type {
        self.walk_expr_expected(expr, scope, following, None)
    }

    /// Walks every argument of a call (`Call` or a qualified `MethodCall`
    /// — both share the same `Vec<Arg>` shape), propagating each one's
    /// expected type (its matching declared parameter, by name if named
    /// else by position) into `walk_expr_expected` so a lambda argument can
    /// infer its own parameter types from it (`apply({ x -> x * 2 }, 5)`).
    /// `signature` is `None` for a callee with no checkable signature at
    /// all — every argument is still walked (occurrences/diagnostics still
    /// recorded), just with no expected type propagated. Returns each
    /// argument's own name (if any) and inferred type, for
    /// `check_call_signature` below to check against `signature`.
    fn walk_call_args(
        &mut self,
        args: &[Arg],
        signature: Option<&(Type, Vec<Type>, Vec<String>)>,
        scope: usize,
        following: Span,
    ) -> Vec<(Option<String>, Type)> {
        let mut arg_infos: Vec<(Option<String>, Type)> = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let expected_param_ty = signature.and_then(|(_, param_types, param_names)| {
                match &a.name {
                    Some(n) => param_names.iter().position(|p| p == n).and_then(|i| param_types.get(i)),
                    None => param_types.get(i),
                }
            });
            let t = self.walk_expr_expected(&a.value, scope, following, expected_param_ty);
            arg_infos.push((a.name.clone(), t));
        }
        arg_infos
    }

    /// Checks already-walked call arguments (`arg_infos`, from
    /// `walk_call_args`) against `signature`'s declared parameter types —
    /// shared by `Call` (a bare-name callee) and `MethodCall` (a fully
    /// package-qualified callee, e.g. `kotlin.test.assertFalse(...)`) so a
    /// stdlib function gets identical argument checking whichever way it's
    /// called, matching real Kotlin where both are the same function. Two
    /// passes, mirroring `call_function`'s own argument-binding algorithm
    /// exactly: named arguments claim their declared slot first; only then
    /// are the remaining positional arguments matched, in order, against
    /// whichever param slots are still unclaimed. `span` is where a
    /// mismatch diagnostic points — the whole call for `Call`, just the
    /// method name for `MethodCall` (matching that arm's own existing
    /// diagnostics). Returns `signature`'s return type, or `Type::Unknown`
    /// when there's no signature to check against at all.
    fn check_call_signature(
        &mut self,
        arg_infos: &[(Option<String>, Type)],
        signature: Option<(Type, Vec<Type>, Vec<String>)>,
        span: Span,
    ) -> Type {
        let Some((ret_ty, param_types, param_names)) = signature else {
            return Type::Unknown;
        };
        let mut claimed = vec![false; param_types.len()];
        for (arg_name, arg_ty) in arg_infos {
            let Some(pname) = arg_name else { continue };
            match param_names.iter().position(|n| n == pname) {
                Some(i) => {
                    claimed[i] = true;
                    if let Some(param_ty) = param_types.get(i) {
                        if *arg_ty != Type::Unknown && *param_ty != Type::Unknown && !types::is_assignable(arg_ty, param_ty) {
                            self.type_errors.push(RuntimeError::new(
                                format!("argument '{pname}' has type {}, expected {}", arg_ty.name(), param_ty.name()),
                                span,
                            ));
                        }
                    }
                }
                None => {
                    self.type_errors.push(RuntimeError::new(format!("unknown parameter '{pname}'"), span));
                }
            }
        }
        let mut remaining = (0..param_types.len()).filter(|&i| !claimed[i]);
        for (arg_name, arg_ty) in arg_infos {
            if arg_name.is_some() {
                continue;
            }
            let Some(i) = remaining.next() else { continue };
            if let Some(param_ty) = param_types.get(i) {
                if *arg_ty != Type::Unknown && *param_ty != Type::Unknown && !types::is_assignable(arg_ty, param_ty) {
                    self.type_errors.push(RuntimeError::new(
                        format!("argument {} has type {}, expected {}", i + 1, arg_ty.name(), param_ty.name()),
                        span,
                    ));
                }
            }
        }
        ret_ty
    }

    /// Like `walk_expr`, but with an optional "expected type" for `expr`
    /// itself — used only by a `Lambda` at this exact position to infer its
    /// parameter types when it has no explicit annotation of its own (see
    /// `walk_lambda_params`): a `val`/`var`'s own annotation
    /// (`ValDecl`'s handling) or a call argument's matching declared
    /// parameter type (`Call`'s handling), both of which call this
    /// directly instead of `walk_expr`. Not threaded into recursive calls
    /// on subexpressions — an expected type only ever applies to the exact
    /// expression position it was computed for (a lambda passed as *this*
    /// argument, not one buried three calls deep inside it), so every
    /// nested `walk_expr`/`walk_stmt` call below still goes through the
    /// plain, expected-type-less path.
    fn walk_expr_expected(&mut self, expr: &Expr, scope: usize, following: Span, expected: Option<&Type>) -> Type {
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
            ExprKind::Name(name) => {
                self.table.name_refs.push((expr.span, name.clone()));
                self.resolve_name(scope, name, expr.span)
            }
            // `++x`/`x--` etc. — same name resolution as a bare `Name`
            // (goto-def/hover on the identifier), returning the target's
            // own type since the value's type never changes across
            // increment/decrement (`Int` stays `Int`, `Double` stays
            // `Double`). No dedicated arithmetic-compatibility check here,
            // same as `StmtKind::Assign`'s `+=`/`-=` (see its own comment) —
            // this is sugar over exactly that, and inherits the same
            // "out of scope for v1" reasoning; a non-numeric target still
            // surfaces a runtime error via `apply_binary`.
            ExprKind::IncDec { name, name_span, .. } => {
                self.table.name_refs.push((*name_span, name.clone()));
                self.resolve_name(scope, name, *name_span)
            }
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
            ExprKind::FieldAccess { receiver, field, safe, .. } => {
                match self.dotted_path_if_not_local(expr, scope) {
                    Some(path) => self.table.qualified_refs.push((expr.span, path)),
                    None => {
                        let recv_ty = self.walk_expr(receiver, scope, following);
                        // An unsafe (`.`) access on a still-nullable
                        // receiver is rejected outright, matching Kotlin's
                        // own compile error — this is what makes an
                        // un-narrowed `chain.length()` a clear diagnostic
                        // instead of a confusing runtime "'Null' has no
                        // field" message. A narrowed receiver never reaches
                        // here as `Nullable` in the first place (see
                        // `resolve_name`'s smart-cast integration).
                        if let Type::Nullable(_) = &recv_ty {
                            if !*safe {
                                self.type_errors.push(RuntimeError::new(
                                    format!(
                                        "only safe (?.) or non-null asserted (!!) calls are allowed on a nullable receiver of type '{}'",
                                        recv_ty.name()
                                    ),
                                    expr.span,
                                ));
                            }
                        } else if recv_ty != Type::Unknown {
                            self.type_errors.push(RuntimeError::new(
                                format!("'{}' has no field '{field}'", recv_ty.name()),
                                expr.span,
                            ));
                        }
                    }
                }
                Type::Unknown
            }
            ExprKind::MethodCall { receiver, method, method_span, args, safe } => {
                self.table.method_refs.push((*method_span, method.clone()));
                let mut qualified_path: Option<Vec<String>> = None;
                let recv_ty = match self.dotted_path_if_not_local(receiver, scope) {
                    Some(mut path) => {
                        path.push(method.clone());
                        self.table.qualified_refs.push((expr.span, path.clone()));
                        qualified_path = Some(path);
                        None
                    }
                    None => Some(self.walk_expr(receiver, scope, following)),
                };
                if let Some(t) = &recv_ty {
                    self.table.method_receiver_types.push((*method_span, t.clone()));
                }
                // A fully package-qualified call to a real stdlib function
                // (`kotlin.test.assertFalse(...)`) gets the exact same
                // argument-vs-parameter checking a bare, imported call to
                // the same function already gets via `ExprKind::Call`'s
                // `stdlib::stdlib_call_types` — matching real Kotlin, where
                // both call forms are the same function. `None` here (an
                // unrecognized qualified path — a typo, or a real cross-
                // file user package this single-file resolver has no
                // static visibility into either way) falls through to
                // `check_call_signature`'s own `None` case below, exactly
                // preserving today's lenient `Type::Unknown` behavior.
                let signature = qualified_path.as_ref().and_then(|path| {
                    crate::stdlib::stdlib_qualified_call_types(path).map(|(param_types, ret_ty)| {
                        let names = vec![String::new(); param_types.len()];
                        (ret_ty, param_types, names)
                    })
                });
                let arg_infos = self.walk_call_args(args, signature.as_ref(), scope, following);
                // An unsafe (`.`) call on a still-nullable receiver is
                // rejected outright, matching Kotlin's own compile error —
                // see the identical check in `FieldAccess` above. Never
                // fires for a narrowed or `!!`-unwrapped receiver, since
                // both already observe a concretely non-nullable type by
                // this point (see `resolve_name`).
                let unchecked_nullable = !*safe && matches!(&recv_ty, Some(Type::Nullable(_)));
                if unchecked_nullable {
                    let t = recv_ty.as_ref().unwrap();
                    self.type_errors.push(RuntimeError::new(
                        format!(
                            "only safe (?.) or non-null asserted (!!) calls are allowed on a nullable receiver of type '{}'",
                            t.name()
                        ),
                        *method_span,
                    ));
                }
                let result_ty = if unchecked_nullable {
                    Type::Unknown
                } else {
                    match &recv_ty {
                        // `Unknown` receiver: deferred, no immediate error —
                        // could be a real method once the receiver's type is
                        // itself resolvable.
                        Some(Type::Unknown) => Type::Unknown,
                        Some(t) => {
                            // A `Nullable(T)` receiver here can only be a
                            // *safe* (`?.`) call at this point (the unsafe
                            // case already returned above) — dispatches
                            // against `T`'s own builtin-method table
                            // (`String?`'s methods are exactly `String`'s —
                            // matches Kotlin's own safe-call semantics).
                            let dispatch_name = match t {
                                Type::Nullable(inner) => inner.name(),
                                other => other.name(),
                            };
                            let known = crate::stdlib::BUILTIN_METHOD_SIGNATURES
                                .iter()
                                .any(|(recv_type, name, ..)| *recv_type == dispatch_name && *name == method);
                            if known {
                                crate::stdlib::builtin_method_return_type(&dispatch_name, method)
                            } else {
                                self.type_errors.push(RuntimeError::new(
                                    format!("'{}' has no method '{method}'", t.name()),
                                    *method_span,
                                ));
                                Type::Unknown
                            }
                        }
                        // The qualified-path case (`kotlin.test.assertFalse(...)`):
                        // `signature` is `Some` exactly when the path
                        // resolved to a real stdlib member above, in which
                        // case this both checks `arg_infos` against it and
                        // returns its real return type — `None` (unknown
                        // qualified path) falls back to `Type::Unknown`,
                        // unchanged from before this check existed.
                        None => self.check_call_signature(&arg_infos, signature, *method_span),
                    }
                };
                // A safe (`?.`) call's own result is nullable whenever it
                // produced a real type — matches Kotlin (`s?.length()` is
                // `Int?`, not `Int`).
                if *safe && result_ty != Type::Unknown {
                    Type::Nullable(Box::new(result_ty))
                } else {
                    result_ty
                }
            }
            ExprKind::Unary { op, expr: inner } => {
                let t = self.walk_expr(inner, scope, following);
                match (op, &t) {
                    (UnaryOp::Neg | UnaryOp::Pos, Type::Int) => Type::Int,
                    (UnaryOp::Neg | UnaryOp::Pos, Type::Double) => Type::Double,
                    (UnaryOp::Not, Type::Bool) => Type::Bool,
                    // `!!` unwraps one layer of `Nullable`; on a
                    // non-nullable operand it's a no-op pass-through.
                    (UnaryOp::NotNullAssert, Type::Nullable(inner)) => (**inner).clone(),
                    (UnaryOp::NotNullAssert, Type::Null) => Type::Unknown,
                    (UnaryOp::NotNullAssert, other) if other != &Type::Unknown => other.clone(),
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
                // `&&`/`||` short-circuit at runtime, and Kotlin smart-
                // casts accordingly: `x != null && x.foo()` only evaluates
                // `x.foo()` once `x != null` is known true, so `rhs` is
                // walked with `lhs`'s implied narrowing already applied
                // (`pos` for `&&`, `neg` for `||` — the same halves
                // `narrowing_for_condition` would compute for `lhs` as a
                // standalone `If`/`While` condition), restored right after.
                let r = if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    let (lpos, lneg) = self.narrowing_for_condition(lhs, scope);
                    let ambient = self.narrowed.clone();
                    self.narrowed.extend(if *op == BinaryOp::And { lpos } else { lneg });
                    let r = self.walk_expr(rhs, scope, following);
                    self.narrowed = ambient;
                    r
                } else {
                    self.walk_expr(rhs, scope, following)
                };
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

                // A checkable signature comes from either a local `fun`
                // symbol or a stdlib function with a known machine-
                // readable signature (a `BuiltinSymbol`'s own signature
                // field is a free-text display string, not this). Stdlib
                // params have no real names to match a named argument
                // against (`String::new()` placeholders — any named-arg
                // call to a stdlib function falls into the "unknown
                // parameter" case below, a minor, documented limitation).
                // Resolved *before* walking the arguments (unlike every
                // other subexpression here) so a lambda argument whose
                // matching parameter is itself a function type
                // (`apply({ x -> x * 2 }, 5)`) can infer its own parameter
                // types from that — see `walk_expr_expected`.
                let signature = match &callee.kind {
                    ExprKind::Name(name) => self
                        .resolve_symbol(scope, name)
                        .and_then(|s| {
                            // A named `fun` symbol carries its own
                            // `param_names` (needed for named-argument
                            // matching below); anything else — a `val`/
                            // `var` holding a lambda, or a function-typed
                            // parameter (`f: (Int) -> Int` inside another
                            // function's body) — is only known
                            // structurally, via its `Type::Function`, with
                            // no parameter names of its own to match a
                            // named argument against (same placeholder
                            // scheme already used for stdlib calls below,
                            // and for the same reason: a function *type*,
                            // unlike a `fun` declaration, never names its
                            // parameters).
                            if s.kind == SymbolKind::Fun {
                                Some((s.return_type.clone(), s.param_types.clone(), s.param_names.clone()))
                            } else if let Type::Function(params, ret) = &s.ty {
                                let names = vec![String::new(); params.len()];
                                Some(((**ret).clone(), params.clone(), names))
                            } else {
                                None
                            }
                        })
                        .or_else(|| {
                            crate::stdlib::stdlib_call_types(name)
                                .map(|(param_types, ret_ty)| {
                                    let names = vec![String::new(); param_types.len()];
                                    (ret_ty, param_types, names)
                                })
                        }),
                    _ => None,
                };

                let arg_infos = self.walk_call_args(args, signature.as_ref(), scope, following);

                // A numeric-polymorphic stdlib name (`abs`/`max`/`min` —
                // see `stdlib::is_numeric_polymorphic`'s doc comment for
                // exactly which shape qualifies) has a return type that
                // mirrors whichever numeric type the caller actually
                // passed — doesn't fit the "one fixed Type" shape every
                // other signature above has, so it's handled directly here
                // instead of via `stdlib::stdlib_call_types` (and, being
                // stdlib functions, never matched a real `signature` above
                // anyway).
                if let ExprKind::Name(name) = &callee.kind {
                    if crate::stdlib::is_numeric_polymorphic(name) {
                        return arg_infos
                            .first()
                            .map(|(_, t)| t.clone())
                            .filter(is_numeric)
                            .unwrap_or(Type::Unknown);
                    }
                }

                self.check_call_signature(&arg_infos, signature, expr.span)
            }
            ExprKind::Lambda { params, body } => self.walk_lambda(expr, params, body, scope, following, expected),
            ExprKind::If { cond, then_branch, else_branch } => {
                self.walk_expr(cond, scope, following);
                // Null-safety smart-cast: `pos`/`neg` are what `cond`
                // proves non-null when true/false — see
                // `narrowing_for_condition`. Each branch is walked with the
                // ambient narrowing extended accordingly, restored right
                // after so one branch's narrowing never leaks into the
                // other or beyond the `if`.
                let (pos, neg) = self.narrowing_for_condition(cond, scope);
                let ambient = self.narrowed.clone();
                self.narrowed.extend(pos);
                let then_ty = self.walk_block(then_branch, scope, expr.span, following);
                self.narrowed = ambient.clone();
                let else_ty = match else_branch {
                    Some(eb) => {
                        self.narrowed.extend(neg.iter().copied());
                        let ty = self.walk_block(eb, scope, expr.span, following);
                        self.narrowed = ambient.clone();
                        ty
                    }
                    None => Type::Unit,
                };
                // Carry-forward: when the then-branch always exits the
                // normal control flow (`return`/`break`/`continue`),
                // whatever continues past this `if` in the same statement
                // list only does so because `cond` was false — so `neg`
                // holds for the rest of the enclosing block (reverted, like
                // any other narrowing, when `walk_stmts` for that block
                // finishes — see its own doc comment).
                if then_branch.last().map(always_exits).unwrap_or(false) {
                    self.narrowed.extend(neg);
                }
                if then_ty == Type::Nothing {
                    // `Nothing` is the identity element of least-upper-bound
                    // (`LUB(Nothing, T) = T`) — a branch that never returns
                    // normally (e.g. `throw`, once Phase 5 lands) carries no
                    // real type info of its own to join against the other
                    // branch with, so the whole `if` just takes the other
                    // branch's type. Checked before the equality/mismatch
                    // logic below so it applies unconditionally, even when
                    // the other branch is itself a primitive.
                    else_ty
                } else if else_ty == Type::Nothing || then_ty == else_ty {
                    then_ty
                } else {
                    // Two different primitives have no common supertype to
                    // join to (no class hierarchy exists yet — see
                    // `is_primitive`'s doc comment) — an immediate error,
                    // per the strict-typing decision, rather than the
                    // lenient `Unknown` join every other mismatch still
                    // gets. Only when there's a *real, explicit* `else`,
                    // though: with no `else`, `else_ty` above is just the
                    // implicit-Unit fallback for "the condition was false"
                    // (matching `eval_expr`'s identical runtime rule), not
                    // a second real value the then-branch is obligated to
                    // agree with — `if (true) 10` used as a statement (or
                    // assigned to a `val`, which then simply can't rely on
                    // a specific type) is valid Kotlin, not a mismatch.
                    if else_branch.is_some() && is_primitive(&then_ty) && is_primitive(&else_ty) {
                        self.type_errors.push(RuntimeError::new(
                            format!("branches have incompatible types: {} and {}", then_ty.name(), else_ty.name()),
                            expr.span,
                        ));
                    }
                    Type::Unknown
                }
            }
            ExprKind::When { subject, arms } => {
                if let Some(s) = subject {
                    self.walk_expr(s, scope, following);
                }
                // Join rule: same type on every arm (a missing `else`
                // implicitly joins with `Unit`, matching the runtime's "no
                // arm matched" fallthrough) → that type; a primitive-vs-
                // primitive mismatch → an immediate error (see `If`'s
                // handling above, mirrored here); any other mismatch →
                // `Unknown`. `WhenArm` has no span of its own, so the whole
                // `when` expression's span is the anchor for each arm's
                // block. Only the *first* mismatch is flagged — once `acc`
                // itself becomes `Unknown`, `is_primitive(&prev)` is false
                // for every later arm, so a three-way `Int`/`String`/`Bool`
                // mismatch reports once, not twice.
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
                    // Null-safety smart-cast for the subject-less form
                    // only, and only for a single-condition arm: comma-
                    // separated conditions are OR'd together (`2, 3 ->`),
                    // so no single narrowing would be sound across all of
                    // them the way it is for one `Bool` condition.
                    let arm_narrowing = match (subject, arm.conditions.as_slice()) {
                        (None, [WhenCondition::Bool(e)]) => Some(self.narrowing_for_condition(e, scope).0),
                        _ => None,
                    };
                    let ambient = self.narrowed.clone();
                    if let Some(pos) = arm_narrowing {
                        self.narrowed.extend(pos);
                    }
                    let arm_ty = self.walk_block(&arm.body, scope, expr.span, following);
                    self.narrowed = ambient;
                    acc = Some(match acc {
                        None => arm_ty,
                        // Same `Nothing`-is-the-join-identity rule as `If`
                        // above: a never-returning arm carries no real type
                        // of its own, so the accumulated join just becomes
                        // whichever side isn't `Nothing`.
                        Some(Type::Nothing) => arm_ty,
                        Some(prev) if arm_ty == Type::Nothing => prev,
                        Some(prev) if prev == arm_ty => prev,
                        Some(prev) => {
                            // Same reasoning as `If`'s identical guard: with
                            // no explicit `else` arm, `acc`'s seed is just
                            // the implicit-Unit "no arm matched" fallback
                            // (see `eval_when`'s matching runtime rule), not
                            // a second real value every arm must agree
                            // with — `when { true -> 10 }` used as a
                            // statement is valid, not a mismatch.
                            if has_else && is_primitive(&prev) && is_primitive(&arm_ty) {
                                self.type_errors.push(RuntimeError::new(
                                    format!(
                                        "branches have incompatible types: {} and {}",
                                        prev.name(),
                                        arm_ty.name()
                                    ),
                                    expr.span,
                                ));
                            }
                            Type::Unknown
                        }
                    });
                }
                acc.unwrap_or(Type::Unknown)
            }
            ExprKind::Block(stmts) => self.walk_block(stmts, scope, expr.span, following),
            ExprKind::NullLit => Type::Null,
            ExprKind::Elvis { lhs, rhs } => {
                let l = self.walk_expr(lhs, scope, following);
                let r = self.walk_expr(rhs, scope, following);
                let unwrapped_l = match &l {
                    Type::Nullable(inner) => (**inner).clone(),
                    // An always-null lhs carries no real type info of its
                    // own to join against `r` with.
                    Type::Null => Type::Unknown,
                    other => other.clone(),
                };
                if unwrapped_l == r {
                    unwrapped_l
                } else if unwrapped_l == Type::Unknown || r == Type::Unknown {
                    Type::Unknown
                } else if is_primitive(&unwrapped_l) && is_primitive(&r) {
                    self.type_errors.push(RuntimeError::new(
                        format!("elvis operands have incompatible types: {} and {}", unwrapped_l.name(), r.name()),
                        expr.span,
                    ));
                    Type::Unknown
                } else {
                    Type::Unknown
                }
            }
        }
    }
}

/// Renders a `TypeAnnotation` back to source-like text — used only for
/// `param_list`'s hover signature, built straight from the raw AST *before*
/// the resolver has walked (and thus validated) it, so this shows exactly
/// what was written, typo and all, rather than a resolved `Type::name()`.
fn type_annotation_display(ann: &TypeAnnotation) -> String {
    match ann {
        TypeAnnotation::Named(name, _) => name.clone(),
        TypeAnnotation::Function(params, ret, _) => {
            let params = params.iter().map(type_annotation_display).collect::<Vec<_>>().join(", ");
            format!("({params}) -> {}", type_annotation_display(ret))
        }
        TypeAnnotation::Nullable(inner, _) => format!("{}?", type_annotation_display(inner)),
    }
}

fn param_list(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| match &p.type_annotation {
            Some(ann) => format!("{}: {}", p.name, type_annotation_display(ann)),
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
        assert_eq!(result.table.symbol_at(1, 5).unwrap().signature, "fun fact(n: Int): Int");
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

    /// Regression test: a fully package-qualified stdlib call
    /// (`kotlin.test.assertFalse(...)`, parsed as `ExprKind::MethodCall`,
    /// not `ExprKind::Call`) used to skip argument type-checking entirely —
    /// only the bare, imported form (`assertFalse(...)`) was checked. Both
    /// forms must now be flagged identically.
    #[test]
    fn qualified_stdlib_call_argument_type_mismatch_is_flagged() {
        let src = "kotlin.test.assertFalse(true == false, 123)\n";
        let result = resolve_src(src);
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("expected String"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn qualified_stdlib_call_with_correct_argument_types_is_not_flagged() {
        let src = "kotlin.test.assertFalse(true == false, \"msg\")\nkotlin.io.println(5)\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn qualified_call_under_the_wrong_stdlib_package_is_not_falsely_checked() {
        // `assertFalse` is a real member of `kotlin.test`, not `kotlin.math`
        // — this must resolve to `Type::Unknown` (today's existing lenient
        // behavior for an unrecognized qualified path), not incorrectly
        // reuse `kotlin.test.assertFalse`'s signature just because the
        // trailing name matches.
        let src = "kotlin.math.assertFalse(true, 123)\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    /// Regression test: a hint-less `val`/`var` initialized from a
    /// variadic stdlib call (`listOf`) used to incorrectly fail the
    /// "cannot infer type" check — `listOf(...)`'s return type (`List`)
    /// is always known regardless of its argument count, unlike a
    /// genuinely un-inferable initializer.
    #[test]
    fn hint_less_val_from_list_of_infers_list_not_an_error() {
        let result = resolve_src("val l = listOf(1, 2, 3)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
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
    fn if_without_else_used_as_a_bare_statement_is_never_a_type_error() {
        // `if (true) 10` with no `else`, its value unused — the implicit
        // Unit "else" is a runtime fallback for the untaken path, not a
        // second real value the then-branch is obligated to match, so
        // this must not be flagged even though `Int`/`Unit` are both
        // primitives (the same combination `if_branches_with_mismatched_primitives_is_a_type_error`,
        // below, confirms *does* still error with an explicit `else`).
        let result = resolve_src("if (true) 10\nprintln(\"done\")\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn when_without_else_used_as_a_bare_statement_is_never_a_type_error() {
        let result = resolve_src("when { true -> 10 }\nprintln(\"done\")\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn if_with_an_explicit_mismatched_else_still_errors() {
        // Regression guard for the fix above: a *real* else must still be
        // held to the same-type requirement.
        let result = resolve_src("if (true) 1 else \"two\"\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn when_with_an_explicit_mismatched_else_arm_still_errors() {
        let result = resolve_src("when { true -> 1\nelse -> \"two\" }\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
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

    #[test]
    fn if_branches_with_mismatched_primitives_is_a_type_error() {
        // `x` itself also has no inferable type (the `if`'s own type is
        // `Unknown` once its branches disagree) — that's a second, genuine
        // diagnosis from the same construct, not a bug, so this only checks
        // that the branch-mismatch error is among the ones reported.
        let result = resolve_src("val x = if (true) 1 else \"two\"\n");
        assert!(
            result
                .type_errors
                .iter()
                .any(|e| e.message.contains("branches have incompatible types: Int and String")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn when_arms_with_mismatched_primitives_is_a_type_error_reported_once() {
        let result = resolve_src("val x = when {\n    1 < 2 -> 1\n    else -> \"two\"\n}\n");
        assert_eq!(
            result.type_errors.iter().filter(|e| e.message.contains("branches have incompatible types")).count(),
            1,
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn if_branches_with_matching_primitives_is_not_a_type_error() {
        let result = resolve_src("val x = if (true) 1 else 2\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn any_type_annotation_resolves_and_accepts_any_value() {
        let result = resolve_src("val x: Any = 1\nval y: Any = \"hi\"\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn any_typed_value_is_not_assignable_back_to_a_concrete_type() {
        let result = resolve_src("val x: Any = 1\nval y: Int = x\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn nothing_type_annotation_resolves() {
        let result = resolve_src("fun f(x: Nothing) { }\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn fun_parameter_without_a_type_annotation_is_a_type_error() {
        let result = resolve_src("fun apply(f, x: Int): Int = x\n");
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("parameter 'f' must have an explicit type annotation")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn function_type_annotated_parameter_resolves_structurally() {
        let result = resolve_src("fun apply(f: (Int) -> Int, x: Int): Int = f(x)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        let f = result.table.symbol_at(1, 11).unwrap();
        assert_eq!(f.ty, Type::Function(vec![Type::Int], Box::new(Type::Int)));
    }

    #[test]
    fn calling_a_val_holding_a_lambda_checks_its_argument_types() {
        // Regression test: the `Call` arm's signature lookup only
        // recognized `SymbolKind::Fun`, so calling a function-typed
        // `val`/`var` (as opposed to a named `fun`) skipped argument
        // checking entirely and silently accepted anything.
        let result = resolve_src("val fn: (Int) -> String = { x -> \"hi\" }\nprintln(fn(\"wrong\"))\n");
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("argument 1 has type String, expected Int")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn calling_a_val_holding_a_lambda_with_a_correct_argument_is_not_flagged() {
        let result = resolve_src("val fn: (Int) -> String = { x -> \"hi\" }\nprintln(fn(5))\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn calling_a_function_typed_parameter_checks_its_argument_types() {
        // Same bug, reached through a function-typed *parameter* rather
        // than a `val`: `f`'s `SymbolKind::Param` was excluded the same way.
        let result = resolve_src("fun apply(f: (Int) -> Int, x: Int): Int = f(\"bad\")\n");
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("argument 1 has type String, expected Int")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn calling_a_val_holding_a_lambda_infers_its_return_type() {
        let result = resolve_src("val fn: (Int) -> String = { x -> \"hi\" }\nval r = fn(5)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(result.table.symbol_at(2, 5).unwrap().ty, Type::String);
    }

    #[test]
    fn lambda_parameter_type_is_inferred_from_the_val_annotation() {
        let result = resolve_src("val double: (Int) -> Int = { x -> x * 2 }\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(
            result.table.symbol_at(1, 5).unwrap().ty,
            Type::Function(vec![Type::Int], Box::new(Type::Int))
        );
    }

    #[test]
    fn lambda_implicit_it_type_is_inferred_from_the_val_annotation() {
        let result = resolve_src("val addOne: (Int) -> Int = { it + 1 }\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(
            result.table.symbol_at(1, 5).unwrap().ty,
            Type::Function(vec![Type::Int], Box::new(Type::Int))
        );
    }

    #[test]
    fn lambda_parameter_type_is_inferred_from_the_call_site_argument() {
        let result =
            resolve_src("fun apply(f: (Int) -> Int, x: Int): Int = f(x)\napply({ x -> x * 2 }, 5)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn lambda_parameter_without_any_expected_type_is_a_type_error() {
        let result = resolve_src("val double = { x -> x * 2 }\n");
        assert!(
            result
                .type_errors
                .iter()
                .any(|e| e.message.contains("cannot infer type of lambda parameter 'x'")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn lambda_implicit_it_without_any_expected_type_is_a_type_error() {
        let result = resolve_src("val addOne = { it + 1 }\n");
        assert!(
            result
                .type_errors
                .iter()
                .any(|e| e.message.contains("cannot infer type of lambda parameter 'it'")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn lambda_return_type_disagreement_is_a_type_error() {
        let result = resolve_src(
            "val f: (Boolean) -> Int = { b ->\n    if (b) { return 1 }\n    return \"x\"\n}\n",
        );
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("lambda returns both")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn lambda_return_type_is_inferred_from_its_body() {
        let result = resolve_src("val f: (Int) -> Int = { x -> x + 1 }\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(
            result.table.symbol_at(1, 5).unwrap().ty,
            Type::Function(vec![Type::Int], Box::new(Type::Int))
        );
    }

    // ---- null-safety ----

    #[test]
    fn assigning_null_to_a_non_nullable_declared_type_is_flagged() {
        let result = resolve_src("val x: String = null\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn assigning_null_to_a_nullable_declared_type_is_not_flagged() {
        let result = resolve_src("val x: String? = null\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(result.table.symbol_at(1, 5).unwrap().ty, Type::Nullable(Box::new(Type::String)));
    }

    #[test]
    fn unknown_name_inside_a_nullable_annotation_is_flagged() {
        let result = resolve_src("val x: Sting? = null\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("unknown type 'Sting'"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn elvis_infers_the_join_of_the_unwrapped_lhs_and_the_rhs() {
        let result = resolve_src("fun f(s: String?): Int = s?.length() ?: -1\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(result.table.symbol_at(1, 5).unwrap().return_type, Type::Int);
    }

    #[test]
    fn safe_call_result_is_inferred_as_nullable() {
        let result = resolve_src("val s: String? = \"hi\"\nval n = s?.length()\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(result.table.symbol_at(2, 5).unwrap().ty, Type::Nullable(Box::new(Type::Int)));
    }

    #[test]
    fn unsafe_call_on_an_un_narrowed_nullable_typed_local_is_rejected() {
        // Reverses the old accepted-gap decision: this resolver now does
        // real flow-sensitive smart-casting (see `Resolver::narrowed`), so
        // an unsafe `.` call on a `T?`-typed local with no preceding null
        // check is a clear, Kotlin-shaped compile-time error instead of a
        // silently-accepted gap that only NPE'd at runtime.
        let result = resolve_src("fun f(s: String?): Int = s.length()\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("only safe (?.) or non-null asserted (!!)"),
            "{}",
            result.type_errors[0].message
        );
        assert!(result.type_errors[0].message.contains("'String?'"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn unsafe_field_access_on_an_un_narrowed_nullable_receiver_is_rejected() {
        let result = resolve_src("fun f(s: String?): Int {\n    return s.x\n}\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("only safe (?.) or non-null asserted (!!)"),
            "{}",
            result.type_errors[0].message
        );
    }

    // ---- smart-cast ----

    #[test]
    fn if_not_null_then_branch_narrows() {
        let result = resolve_src("fun f(s: String?): Int {\n    if (s != null) {\n        return s.length()\n    }\n    return -1\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn if_null_reversed_operand_order_then_branch_still_narrows() {
        // `null != s` (operands swapped) narrows exactly like `s != null`.
        let result = resolve_src("fun f(s: String?): Int {\n    if (null != s) {\n        return s.length()\n    }\n    return -1\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn if_equals_null_else_branch_narrows() {
        let result = resolve_src("fun f(s: String?): Int {\n    if (s == null) {\n        return -1\n    } else {\n        return s.length()\n    }\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn if_equals_null_then_branch_stays_nullable() {
        // Inside the `then` branch of `s == null`, `s` is still nullable —
        // an unsafe call there must still be rejected.
        let result = resolve_src("fun f(s: String?): Int {\n    if (s == null) {\n        return s.length()\n    }\n    return -1\n}\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn early_return_carries_narrowing_forward() {
        // `if (s == null) { return -1 }` always exits when taken, so `s`
        // is narrowed for the rest of the function body — the single most
        // common real-world null-check idiom.
        let result = resolve_src("fun f(s: String?): Int {\n    if (s == null) {\n        return -1\n    }\n    return s.length()\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn early_continue_in_a_loop_carries_narrowing_forward() {
        let result = resolve_src(
            "fun f(s: String?) {\n    while (true) {\n        if (s == null) {\n            continue\n        }\n        println(s.length())\n    }\n}\n",
        );
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn narrowing_does_not_leak_past_a_non_exiting_if() {
        // The `then` branch here doesn't always exit, so nothing about `s`
        // is known after the `if` — an unsafe call afterward must still be
        // rejected.
        let result = resolve_src(
            "fun f(s: String?): Int {\n    if (s != null) {\n        println(\"checked\")\n    }\n    return s.length()\n}\n",
        );
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn and_short_circuit_narrows_the_right_operand() {
        let result = resolve_src("fun f(s: String?): Boolean = s != null && s.length() > 0\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn or_short_circuit_narrows_the_right_operand() {
        let result = resolve_src("fun f(s: String?): Boolean = s == null || s.length() > 0\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn while_condition_narrows_the_loop_body() {
        let result = resolve_src(
            "fun f(s: String?) {\n    while (s != null) {\n        println(s.length())\n    }\n}\n",
        );
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn subject_less_when_arm_narrows_its_own_body() {
        let result = resolve_src(
            "fun f(s: String?): Int {\n    return when {\n        s != null -> s.length()\n        else -> -1\n    }\n}\n",
        );
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn var_never_narrows() {
        // Kotlin itself never smart-casts a mutable local — it could
        // change between the check and the use — and neither does this
        // resolver.
        let result = resolve_src(
            "fun f() {\n    var s: String? = \"hi\"\n    if (s != null) {\n        println(s.length())\n    }\n}\n",
        );
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
    }

    #[test]
    fn not_operator_flips_narrowing() {
        // `!(s == null)` is equivalent to `s != null` for narrowing
        // purposes.
        let result = resolve_src("fun f(s: String?): Int {\n    if (!(s == null)) {\n        return s.length()\n    }\n    return -1\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }
}
