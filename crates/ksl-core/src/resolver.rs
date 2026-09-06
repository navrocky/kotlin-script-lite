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
use std::path::PathBuf;
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
    /// A `class` declaration's own name (`Point` in `class Point(...)`) —
    /// distinct from an *instance* of it, which has no symbol of its own
    /// (see `ExprKind::This`'s handling, the only place a real instance
    /// gets a synthesized `Symbol`, kind `Val`).
    Class,
    /// A class's own field — constructor `val`/`var` param promotion or a
    /// body `Property` member. Synthesized via `define_class_member`, not
    /// `define` (a field is never a bare, scope-visible name — only
    /// reachable through `this.name`/`obj.name`, see `Resolver::
    /// class_info`), mirroring `named_arg_labels`'s existing "synthesize a
    /// `Symbol`, don't register it in `scope.names`" pattern.
    Field,
    /// A class's own method — synthesized the same way as `Field`, for the
    /// same reason (only reachable through `this.name(...)`/
    /// `obj.name(...)`, never as a bare call).
    Method,
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
    /// Only meaningful for `SymbolKind::Fun`: one parameter's own `@param`
    /// description (if any), same order as `param_types`/`param_names` —
    /// lets a named-argument call site (`f(a = 1)`) show that specific
    /// parameter's doc on hover without needing the whole function's doc
    /// (which bundles every parameter together, see `doc` below). Empty for
    /// every other kind.
    pub param_docs: Vec<Option<String>>,
    /// Same order as `param_types`/`param_names`: whether that parameter
    /// has a `= default` value (`ast::Param::default`), which lets a call
    /// site legitimately omit it — `check_call_signature`'s "missing
    /// argument" arity check consults this so a real default doesn't get
    /// flagged as a missing required argument. Empty for every other kind,
    /// same convention as `param_types`.
    pub param_has_default: Vec<bool>,
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

/// A callee's checkable signature, as used by `walk_call_args`/
/// `check_call_signature`: return type, one resolved type per declared
/// parameter, one name per parameter (an empty string where the callee has
/// no real parameter names of its own — see the `Call`/`MethodCall`
/// handling in `walk_expr_expected`), one `@param` description per
/// parameter (`None` where there is none, or where the callee has no doc
/// comments to begin with, e.g. a stdlib function), and one "has an
/// `= default`" flag per parameter — `check_call_signature`'s arity check
/// only ever consults this last element when every parameter name is real
/// (non-empty); a stdlib/structural-type signature's blank-name
/// placeholders always carry `false` here regardless, since arity is never
/// checked for those anyway.
type CallSignature = (Type, Vec<Type>, Vec<String>, Vec<Option<String>>, Vec<bool>);

/// Adapts `stdlib::builtin_method_signature`'s own 4-element
/// `MethodSignature` (never a real per-parameter name, so never arity-
/// checked either — see `CallSignature`'s doc comment) into a `CallSignature`
/// by tacking on an all-`false` `param_has_default`.
fn builtin_method_signature_as_call_signature(receiver_type: &str, method: &str) -> Option<CallSignature> {
    crate::stdlib::builtin_method_signature(receiver_type, method).map(|(ret, pt, pn, pd)| {
        let has_default = vec![false; pt.len()];
        (ret, pt, pn, pd, has_default)
    })
}

/// A `class` declaration's own static shape — `Resolver::class_info`'s
/// value, keyed by class name. Built in two passes by `Resolver::
/// walk_class_decl`: every field's/method's *signature* is inserted before
/// any member body is walked, so one member referencing another (either
/// order — `this.other()` calling a method declared later in the same
/// class) resolves correctly, mirroring how `StmtKind::FunDecl` defines its
/// own name before walking its body. Flat, no superclass chain of its own —
/// every user class implicitly extends the real builtin `Any` (single hop;
/// `:` multi-level inheritance is Phase 7's job), so a lookup that misses
/// here falls through to `stdlib::builtin_method_signature("Any", ...)`
/// directly (see `Resolver::class_method_signature`) rather than walking a
/// chain of `ClassInfo`s.
struct ClassInfo {
    fields: Vec<ClassFieldInfo>,
    methods: Vec<ClassMethodInfo>,
    /// The primary constructor's own real parameter types/names — what
    /// lets `Point(3, 4)` (`ExprKind::Call`, callee resolving to this
    /// class's own `SymbolKind::Class` symbol) get exactly the same
    /// argument/named-argument checking an ordinary `fun` call already
    /// gets, with `Type::Class(name)` (not `Type::Unknown`, unlike the
    /// class symbol's own bare-reference type) as the call's result.
    ctor_param_types: Vec<Type>,
    ctor_param_names: Vec<String>,
    /// Same order as `ctor_param_types`/`ctor_param_names` — see
    /// `Symbol::param_has_default`'s identical doc comment.
    ctor_param_has_default: Vec<bool>,
}

struct ClassFieldInfo {
    name: String,
    ty: Type,
    mutable: bool,
    /// The field's own declaration-site `Symbol` index — a promoted ctor
    /// param's real, scope-registered symbol (`self.define`), or a body
    /// `Property`'s synthesized one (`define_class_member`). Lets a
    /// `this.field`/`obj.field` *use* site record a real `occurrences`
    /// entry pointing back at it, the same way a use of any ordinary local
    /// does — see `ExprKind::FieldAccess`'s handling.
    symbol_idx: usize,
}

struct ClassMethodInfo {
    name: String,
    return_type: Type,
    param_types: Vec<Type>,
    param_names: Vec<String>,
    param_docs: Vec<Option<String>>,
    /// Same order as `param_types`/`param_names` — see
    /// `Symbol::param_has_default`'s identical doc comment.
    param_has_default: Vec<bool>,
    /// The method's own declaration-site `Symbol` index
    /// (`define_class_member`) — see `ClassFieldInfo::symbol_idx`'s
    /// identical rationale.
    symbol_idx: usize,
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
    /// at scope creation (see `Resolver::open_scope`) to whichever comes
    /// first, textually: this construct's own real closing extent (every
    /// block-bodied `Stmt`/`Expr` kind that opens a scope —
    /// `FunDecl`/`While`/`For`/`If`/`When`/`Lambda` — has its own span
    /// widened all the way to its real closing `}` by
    /// `parser::Parser::parse_stmt`/`parse_lambda`/`parse_if_expr`/
    /// `parse_when_expr`, not just anchored at its opening keyword), or the
    /// next sibling construct's start (end-of-file if there is no next
    /// sibling) — the latter is only ever the tighter bound for a
    /// construct whose own span end is somehow unavailable/unreliable; see
    /// `open_scope`'s own doc comment for why the `min` is what makes an
    /// empty gap (blank lines, trailing whitespace) between this
    /// construct's closing `}` and the next sibling correctly fall
    /// *outside* this scope rather than still inside it.
    bounds: Span,
    /// name -> every declared symbol index sharing it, in declaration
    /// order — almost always exactly one entry; more than one only when
    /// they're all mutually-distinguishable `Fun` overloads (real
    /// overloading — see `Resolver::define`), which is the one case this
    /// isn't a conflict. Any other same-name redefinition in one scope
    /// (a second `val`/`var`, or a `Fun` whose signature exactly matches
    /// an existing one) is flagged as a diagnostic at definition time but
    /// still appended, matching last-write-wins the way a second
    /// `env.define` with the same name would at runtime — `resolve_symbol`
    /// resolves such a group to its *last* entry as a documented
    /// simplification; only a real call site (`Call`/`MethodCall`) needs
    /// the *whole* group, via `resolve_symbols`.
    names: HashMap<String, Vec<usize>>,
    kind: ScopeKind,
}

/// What kind of construct a scope belongs to — used by
/// `SymbolTable::inside_a_loop_at` to answer "would `break`/`continue`
/// typed here resolve to an enclosing loop", matching
/// `parser::Parser::loop_depth`'s own rule exactly.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    /// A function or lambda body — crossing one *resets* loop context:
    /// `parse_fun_decl`/`parse_lambda` both save and zero `loop_depth`
    /// around their own body, so a loop enclosing the function/lambda from
    /// the *outside* doesn't make `break`/`continue` inside it legal.
    FunctionBoundary,
    /// A `while`/`for` loop's own body.
    Loop,
    /// Everything else that opens a scope but isn't loop-relevant on its
    /// own (an `if`/`when` branch, the file's global scope) — keep
    /// searching further up.
    Other,
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
    /// Every named-argument label's own span at a call site (`f(a = 1)`),
    /// alongside a synthesized `Symbol` describing that specific declared
    /// parameter — built from whichever callee signature `walk_call_args`
    /// resolved (see there), even when that's a cross-file or stdlib
    /// callee this resolver otherwise never builds real `Symbol`s for.
    /// Empty for a positional argument, or when the callee's signature (and
    /// so which parameter this name refers to) couldn't be resolved at all.
    named_arg_labels: Vec<(Span, Symbol)>,
    /// Every `if` expression with no `else` of its own, as `(if_span,
    /// following)` — `if_span` is the whole `if`'s own span (widened to its
    /// then-branch's real closing extent, see `parser::Parser::prev_span`),
    /// `following` is wherever the next sibling construct begins. Lets a
    /// caller (`ksl-lsp`'s completion) answer "would `else` be legal right
    /// here" from a real fact instead of a raw-text "does this look like
    /// it's right after a `}`" guess — see `else_allowed_at`.
    dangling_ifs: Vec<(Span, Span)>,
    /// Every class's own member symbols (fields and methods, in
    /// declaration order — ctor-promoted fields first, then body
    /// `Property`s, then methods, matching `walk_class_decl`'s own
    /// traversal order), keyed by class name — lets a caller (`ksl-lsp`'s
    /// completion, post-`.` on a `Type::Class` receiver) enumerate a
    /// class's own completable members without needing `resolver::
    /// Resolver`'s private `class_info` (which doesn't survive past
    /// resolution) — see `class_members`.
    class_members: HashMap<String, Vec<usize>>,
    /// Every expression's own span, paired with its statically computed
    /// `Type` — populated once, generically, by `Resolver::
    /// walk_expr_expected`'s thin wrapper (see its own doc comment), for
    /// *every* `ExprKind` at *every* nesting depth, not just calls. Lets a
    /// caller (`ksl-lsp`'s completion, via `type_ending_before`) find "the
    /// static type of whatever expression sits right before this cursor
    /// position" straight from the AST — no text-based heuristics (no
    /// re-parsing/paren-balancing over raw source text). Pushed in
    /// post-order (every subexpression's own entry lands before the
    /// enclosing expression's), which `type_ending_before` relies on to
    /// prefer the outermost expression on a tie.
    expr_types: Vec<(Span, Type)>,
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

    /// The named-argument label (if any) whose own span covers
    /// `(line, col)` — e.g. `name` in `greet(name = "World")` — with a
    /// synthesized `Symbol` describing that specific parameter. See
    /// `named_arg_labels`'s doc comment.
    pub fn named_arg_label_at(&self, line: u32, col: u32) -> Option<(Span, &Symbol)> {
        self.named_arg_labels
            .iter()
            .find(|(span, _)| span.contains_line_col(line, col))
            .map(|(span, symbol)| (*span, symbol))
    }

    /// Every symbol declared directly at this file's top level (scope 0,
    /// always the global scope — see `resolve`). Used by callers indexing
    /// a `package`-declaring file's exports (`ksl-lsp`'s `package_index`);
    /// unrelated to `symbols_in_scope`, which is scoped to a cursor
    /// position within *this* file, not "what does this file export".
    pub fn top_level_symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.scopes[0].names.values().flatten().map(|&idx| &self.symbols[idx])
    }

    /// Every member symbol (fields and methods) `class_name`'s own
    /// `ClassDecl` declares — empty for a name that isn't a real,
    /// already-walked class in this file (an unknown name, or a real
    /// class whose own declaration hasn't been walked yet — see
    /// `Resolver::class_info`'s doc comment on that latter, accepted
    /// limitation). Used by `ksl-lsp`'s completion for post-`.` member
    /// suggestions on a `Type::Class` receiver.
    pub fn class_members(&self, class_name: &str) -> impl Iterator<Item = &Symbol> {
        self.class_members.get(class_name).into_iter().flatten().map(|&idx| &self.symbols[idx])
    }

    /// The static `Type` of whichever recorded expression ends closest to
    /// (but strictly before) `(line, col)` on that same line — e.g. for
    /// `val origin = Point(0, 0).` with `(line, col)` right after the
    /// dot, this finds the `Call` expression `Point(0, 0)` (its span
    /// widened to its own closing `)` — see `parser.rs`'s `parse_postfix`)
    /// and returns its `Type::Class("Point")`. Strict `<` (not `<=`)
    /// deliberately doesn't care whether the operator right before the
    /// cursor is one character (`.`) or two (`?.`): either way the
    /// expression's own `end_col` (which never includes the operator
    /// itself) sits strictly below the cursor's column. On a tie (more
    /// than one expression ending at the same column — always nested
    /// ones, never two unrelated expressions, since only one real
    /// expression chain can end at a given source position),
    /// `max_by_key` returns the last-pushed entry, which — thanks to
    /// `expr_types`'s post-order recording — is the outermost one, the
    /// correct choice. `None` when nothing recorded ends there at all
    /// (e.g. a completely stale/unresolved position, or the line is
    /// something `expr_types` never covers, like a bare type annotation).
    pub fn type_ending_before(&self, line: u32, col: u32) -> Option<&Type> {
        self.expr_types
            .iter()
            .filter(|(span, _)| span.end_line == line && span.end_col < col)
            .max_by_key(|(span, _)| span.end_col)
            .map(|(_, ty)| ty)
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
            for (name, indices) in &scope.names {
                if seen.insert(name.clone()) {
                    // Every overload sharing this name in the nearest
                    // scope that declares it at all (almost always just
                    // one) — an outer scope's same name is still fully
                    // shadowed, exactly as before.
                    for &sym_idx in indices {
                        out.push(&self.symbols[sym_idx]);
                    }
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

    /// `true` when a `break`/`continue` typed at `(line, col)` would
    /// resolve to an enclosing loop — walks outward from the innermost
    /// scope there, stopping at (and returning `true` for) the first
    /// `ScopeKind::Loop`, or `false` at the first `ScopeKind::
    /// FunctionBoundary` or the top without finding either. Matches
    /// `parser::Parser::loop_depth`'s own rule exactly (see
    /// `ScopeKind::FunctionBoundary`'s doc comment) — used by `ksl-lsp`'s
    /// completion to only offer `break`/`continue` where they'd actually
    /// be legal, rather than everywhere.
    pub fn inside_a_loop_at(&self, line: u32, col: u32) -> bool {
        let Some(mut idx) = self.innermost_scope(line, col) else { return false };
        loop {
            match self.scopes[idx].kind {
                ScopeKind::Loop => return true,
                ScopeKind::FunctionBoundary => return false,
                ScopeKind::Other => {}
            }
            match self.scopes[idx].parent {
                Some(p) => idx = p,
                None => return false,
            }
        }
    }

    /// `true` when `else` typed at `(line, col)` would attach to a real
    /// `if` with no `else` of its own yet — the gap right after that
    /// `if`'s own closing extent, up to (not including) wherever the next
    /// sibling construct begins, same "gap belongs to what comes after"
    /// rule `open_scope`'s bounds already use. Replaces a raw-text "does
    /// this look like it's right after some `}`" guess, which couldn't
    /// tell an `if` needing an `else` apart from any other block's own
    /// closing brace.
    pub fn else_allowed_at(&self, line: u32, col: u32) -> bool {
        let pos = (line, col);
        self.dangling_ifs.iter().any(|(if_span, following)| {
            let if_end = (if_span.end_line, if_span.end_col);
            let following_start = (following.line, following.col);
            if_end <= pos && pos < following_start
        })
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
    /// Call-site span → the chosen overload's own declaration span, for
    /// every call that resolved to exactly one of *several* local `fun`
    /// overloads (see `ScopeData::names`'s doc comment) — empty for the
    /// overwhelming majority of calls, which never had more than one
    /// candidate to begin with. This is what lets a later execution pass
    /// skip redoing the same by-argument-type search the resolver already
    /// did, by matching a `FunctionValue`'s own `decl_span` against the
    /// value recorded here for a given call site.
    pub resolved_calls: HashMap<Span, Span>,
    /// The cross-file counterpart of `resolved_calls`: every bare-name use
    /// or call that resolved to a symbol brought in by a real `import`/
    /// `@file:Import` (see `CrossFileImports`), or to a package-qualified
    /// call resolved against a real user package (see the `MethodCall`
    /// handling) — mapped to an exact reference to the symbol it came
    /// from, matching the rigor a same-file declaration already gets.
    pub cross_file_refs: HashMap<Span, CrossFileRef>,
    /// Every symbol visible unqualified in this file purely because of a
    /// real `import a.b.c`/`import a.b.*`, or a bare `@file:Import`'s
    /// package-less flat namespace — see `CrossFileImports`. Exposed for a
    /// caller that needs "what's visible here" as a whole, not just
    /// whether one particular name resolves (e.g. `ksl-lsp`'s completion,
    /// or its hover/goto-definition "no local declaration at all"
    /// fallback) — `resolve_name`/the `Call` arm already do the equivalent
    /// per-name lookup internally via `imports`, which this simply
    /// flattens into one list.
    pub imported_symbols: Vec<crate::package_index::PackageMember>,
}

impl ResolveResult {
    /// The `CrossFileRef` (if any) whose own recorded span covers
    /// `(line, col)` — the same "at this position" query style every
    /// `SymbolTable::*_at` method already uses, just against
    /// `cross_file_refs` instead of one of `table`'s own span lists (this
    /// map lives on `ResolveResult` itself, not `SymbolTable`, since it's
    /// keyed by expression/field/method span rather than by declared
    /// symbol). `ksl-lsp`'s hover/goto-definition consult this once their
    /// own same-file `occurrence_at` lookup comes up empty, so a `.field`/
    /// `.method()` use on a cross-file class instance — which has no local
    /// `Symbol` index of its own to record an occurrence against — still
    /// resolves to something real.
    pub fn cross_file_ref_at(&self, line: u32, col: u32) -> Option<&CrossFileRef> {
        self.cross_file_refs.iter().find(|(span, _)| span.contains_line_col(line, col)).map(|(_, r)| r)
    }
}

/// An exact reference to a symbol resolved from outside this file — see
/// `ResolveResult::cross_file_refs`. `file: None` means a synthetic stdlib
/// package member (e.g. `kotlin.test.assertEquals`), which has no source
/// file of its own, matching `package_index::PackageMember::file`'s same
/// convention. Carries the full matched `Symbol` (not just its `def_span`)
/// so a caller needing display info — `ksl-lsp`'s hover, in particular,
/// which has no other way to describe a cross-file class field/method use
/// site — doesn't need a second lookup back into a `PackageIndex` it may
/// not even have a class name to search by.
#[derive(Debug, Clone)]
pub struct CrossFileRef {
    pub file: Option<PathBuf>,
    pub def_span: Span,
    pub symbol: Symbol,
}

/// Resolves `program` with no cross-file knowledge beyond the stdlib
/// (`package_index::PackageIndex::new()`'s stdlib-only seeding) — a
/// convenience wrapper over `resolve_with_imports` for callers with no
/// filesystem access at all (this module's own unit tests, `static_check`).
/// A script whose imports are all satisfied by real stdlib packages
/// resolves identically either way; one with a real user/file-backed
/// import needs `resolve_with_imports` with a real `PackageIndex` instead.
pub fn resolve(program: &Program) -> ResolveResult {
    resolve_with_imports(program, &crate::package_index::PackageIndex::new())
}

/// Like `resolve`, but with real cross-file `import a.b.c`/`import a.b.*`/
/// `@file:Import` resolution against `packages` — a `PackageIndex` built by
/// a caller with real filesystem access (`importer.rs` for execution,
/// `ksl-lsp`'s analysis pipeline for the editor). Every symbol an import
/// brings into scope is resolved for real here: an import naming a package
/// or member that doesn't exist in `packages` is a diagnostic at the
/// import's own span, and a name that *does* resolve gets an exact
/// `CrossFileRef` recorded (see `ResolveResult::cross_file_refs`) — no
/// blanket leniency for any import shape, matching the rigor of a local
/// declaration exactly. See `CrossFileImports` for the resolution mechanism
/// itself.
pub fn resolve_with_imports(program: &Program, packages: &crate::package_index::PackageIndex) -> ResolveResult {
    let mut diagnostics = Vec::new();
    let imports = CrossFileImports::build(program, packages, &mut diagnostics);
    let mut r = Resolver {
        table: SymbolTable {
            scopes: Vec::new(),
            symbols: Vec::new(),
            occurrences: Vec::new(),
            qualified_refs: Vec::new(),
            method_refs: Vec::new(),
            method_receiver_types: Vec::new(),
            name_refs: Vec::new(),
            named_arg_labels: Vec::new(),
            dangling_ifs: Vec::new(),
            class_members: HashMap::new(),
            expr_types: Vec::new(),
        },
        diagnostics,
        type_errors: Vec::new(),
        resolved_calls: HashMap::new(),
        cross_file_refs: HashMap::new(),
        imports,
        packages,
        generic_params: Vec::new(),
        // Real predpass: every top-level `class`'s name, known *before*
        // `walk_stmts` starts — lets a type annotation reference a class
        // declared later in the same file (`fun f(p: Point)` appearing
        // before `class Point(...)`), exactly the forward-reference
        // freedom a top-level `fun` already has for its own name (that
        // one's handled by `define`ing it before its body is walked, but a
        // *type annotation* referencing a class needs the name known
        // ahead of the whole file, not just ahead of one declaration's own
        // body). Top-level only, not recursive into nested bodies: a
        // locally-scoped class (legal — see `interpreter.rs`'s local-class
        // tests) is a much rarer forward-reference case, deliberately not
        // handled by this predpass.
        known_classes: program
            .stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::ClassDecl { name, .. } => Some(Rc::from(name.as_str())),
                _ => None,
            })
            .collect(),
        class_info: HashMap::new(),
        current_class: None,
        return_type_stack: Vec::new(),
        narrowed: HashSet::new(),
    };
    let global = r.push_scope(None, 0, FILE_SPAN, ScopeKind::Other);
    r.walk_stmts(&program.stmts, global, FILE_END);
    let imported_symbols = r.imports.by_name.into_values().flatten().collect();
    ResolveResult {
        table: r.table,
        diagnostics: r.diagnostics,
        type_errors: r.type_errors,
        resolved_calls: r.resolved_calls,
        cross_file_refs: r.cross_file_refs,
        imported_symbols,
    }
}

/// Precise, per-file cross-file *bare-name* import resolution — replaces
/// the old `imported_names`/`has_file_imports`/`has_wildcard_import`
/// blanket leniency. Built once, up front, from `program.imports_use`
/// (`import a.b.c` / `import a.b.*`) and `program.imports`'s flat/package-
/// less namespace (see `package_index::PackageIndex::flat_members`),
/// against the caller-supplied `PackageIndex` — real, filesystem-backed
/// knowledge of what every reachable file/stdlib package actually exports.
/// Every entry here is a *real*, verified symbol: an `import` naming a
/// package or member that doesn't exist is a diagnostic at the import's own
/// span (pushed directly into the `diagnostics` vec passed to `build`),
/// never silent leniency. A *qualified* call (`a.b.c(...)`, not brought in
/// by any `import`) is resolved separately, directly against `packages`
/// (see the `MethodCall` handling in `walk_expr_expected`) — this struct
/// only covers names visible *unqualified*.
struct CrossFileImports {
    by_name: HashMap<String, Vec<crate::package_index::PackageMember>>,
}

impl CrossFileImports {
    fn build(program: &Program, packages: &crate::package_index::PackageIndex, diagnostics: &mut Vec<RuntimeError>) -> Self {
        let mut by_name: HashMap<String, Vec<crate::package_index::PackageMember>> = HashMap::new();
        for use_ in &program.imports_use {
            if use_.wildcard {
                let package = use_.path.join(".");
                if !packages.has_package(&package) {
                    diagnostics.push(RuntimeError::new(format!("unknown package '{package}'"), use_.span));
                    continue;
                }
                for member in packages.members_of(&package) {
                    by_name.entry(member.symbol.name.clone()).or_default().push(member.clone());
                }
            } else {
                // Checked by the parser: at least 2 segments, the last one
                // the member name, everything before it the package name.
                let member_name = use_.path.last().expect("import path has a member segment").clone();
                let package = use_.path[..use_.path.len() - 1].join(".");
                if !packages.has_package(&package) {
                    diagnostics.push(RuntimeError::new(format!("unknown package '{package}'"), use_.span));
                    continue;
                }
                match packages.find(&package, &member_name) {
                    Some(found) => by_name.entry(member_name).or_default().push(found.clone()),
                    None => diagnostics.push(RuntimeError::new(
                        format!("undefined name '{}'", use_.path.join(".")),
                        use_.span,
                    )),
                }
            }
        }
        // Bare `@file:Import` with no `import` statement: every package-
        // less imported file's top-level names are visible unqualified —
        // matches `Interpreter::run_program`'s own flat/package-less
        // namespace exactly (a single shared `globals` env regardless of
        // which file provided a name), so this is real, verified
        // resolution too, not a separate leniency — an import that brought
        // in nothing (or nothing at all) simply contributes no candidates.
        for member in packages.flat_members() {
            by_name.entry(member.symbol.name.clone()).or_default().push(member.clone());
        }
        CrossFileImports { by_name }
    }
}

struct Resolver<'a> {
    table: SymbolTable,
    diagnostics: Vec<RuntimeError>,
    type_errors: Vec<RuntimeError>,
    resolved_calls: HashMap<Span, Span>,
    cross_file_refs: HashMap<Span, CrossFileRef>,
    /// Real, precomputed cross-file bare-name resolution — see
    /// `CrossFileImports`.
    imports: CrossFileImports,
    /// The real, filesystem-backed cross-file package index this file was
    /// resolved with — consulted directly (not through `imports`) for a
    /// *qualified* call (`a.b.c(...)`), which names its target package
    /// outright rather than going through an `import` statement. See the
    /// `MethodCall` handling in `walk_expr_expected`.
    packages: &'a crate::package_index::PackageIndex,
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
    /// Every `class` declared in this file's own static shape (fields'/
    /// methods' real, checkable signatures) — see `ClassInfo`'s own doc
    /// comment. Populated as each `ClassDecl` is walked (`walk_class_decl`),
    /// not by the up-front `known_classes` predpass (which only needs
    /// names, for type-annotation resolution) — a class referenced in a
    /// type annotation *before* its own `ClassDecl` is walked resolves to a
    /// real `Type::Class`, but a field/method access on such a not-yet-
    /// walked class (a real, accepted limitation, not fixed this phase)
    /// won't find anything here yet.
    class_info: HashMap<Rc<str>, ClassInfo>,
    /// The class whose constructor body/property initializer/`init`
    /// block/method body is currently being walked, if any — `None`
    /// everywhere else. This is what makes implicit `this` real: a bare
    /// name that isn't a local at all (`resolve_name`) or a bare call
    /// (`ExprKind::Call`) falls back to this class's own fields/methods,
    /// matching real Kotlin's own implicit-receiver rule (a local of the
    /// same name always shadows a field — checked first, unconditionally,
    /// by the ordinary scope-chain lookup this only ever runs *after*).
    /// Saved and restored around each class member's own body walk
    /// (`walk_class_decl`/`walk_class_method`), the same pattern
    /// `return_type_stack`/`generic_params` already use for their own
    /// enclosing-context state — a single `Option`, not a stack, since
    /// classes can't nest yet (no local-class-inside-a-method-body
    /// interaction to worry about: a local class's own body walk saves
    /// and restores this exactly like a top-level one would).
    current_class: Option<Rc<str>>,
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

impl<'a> Resolver<'a> {
    fn push_scope(&mut self, parent: Option<usize>, depth: u32, bounds: Span, kind: ScopeKind) -> usize {
        self.table.scopes.push(ScopeData { parent, depth, bounds, names: HashMap::new(), kind });
        self.table.scopes.len() - 1
    }

    /// Creates a child scope for a construct that runs in its own
    /// `env.child()` at runtime (function/lambda bodies, loop/branch/block
    /// bodies — see the module doc comment), with its extent set up front:
    /// from `own_start` (this construct's own span — the whole `fun (...) {
    /// ... }`/`while (...) { ... }`/lambda/etc., not just its opening
    /// keyword or brace: `parser::Parser::parse_stmt` and its `if`/`when`/
    /// lambda counterparts widen every one of these to their own real
    /// closing `}` — see `parser::Parser::prev_span`'s doc comment) to
    /// whichever comes first, textually: `own_start`'s own end, or
    /// `following`'s start (wherever the next sibling construct begins).
    ///
    /// The `min` matters: `following`'s start is *always* a safe upper
    /// bound (nothing inside this construct can be textually after the
    /// next sibling begins), but on its own it over-extends this scope
    /// through the *entire gap* after this construct's own closing `}` —
    /// blank lines, trailing whitespace — right up to that next sibling,
    /// incorrectly treating that gap as still "inside" this construct.
    /// `own_start`'s own end is the tighter, correct bound whenever it's
    /// available and earlier (the normal case for anything reaching this
    /// function, all of which now carry a real closing span) — falling
    /// back to `following` only protects against a construct whose own
    /// span end isn't reliable for some as-yet-undiscovered reason, never
    /// making bounds wider than they already were.
    fn open_scope(&mut self, parent: usize, own_start: Span, following: Span, kind: ScopeKind) -> usize {
        let depth = self.table.scopes[parent].depth + 1;
        let own_end = (own_start.end_line, own_start.end_col);
        let following_start = (following.line, following.col);
        let (end_line, end_col) = if own_end <= following_start { own_end } else { following_start };
        let bounds = Span::with_end(own_start.line, own_start.col, end_line, end_col);
        self.push_scope(Some(parent), depth, bounds, kind)
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
        param_docs: Vec<Option<String>>,
        param_has_default: Vec<bool>,
        doc: Option<String>,
    ) -> usize {
        // Real overloading — several `Fun` declarations sharing one name —
        // is legal *only* when every existing entry under this name is
        // also a `Fun` and none of them has this exact same parameter-type
        // signature already (a genuine conflict, matching real Kotlin's
        // "conflicting overloads" error, not "already declared" — the
        // wording differs on purpose, since these are different mistakes:
        // one is redeclaring a name, the other is redeclaring a signature).
        // Anything else sharing a name with `kind`/an existing non-`Fun`
        // entry is the original, unconditional "already declared" case.
        let existing = self.table.scopes[scope].names.get(name).cloned().unwrap_or_default();
        if !existing.is_empty() {
            let all_fun = kind == SymbolKind::Fun && existing.iter().all(|&i| self.table.symbols[i].kind == SymbolKind::Fun);
            if all_fun {
                if existing.iter().any(|&i| self.table.symbols[i].param_types == param_types) {
                    self.diagnostics.push(RuntimeError::new(
                        format!(
                            "conflicting overload of '{name}': a function with this exact signature is already declared in this scope"
                        ),
                        def_span,
                    ));
                }
            } else {
                self.diagnostics.push(RuntimeError::new(
                    format!("'{name}' is already declared in this scope"),
                    def_span,
                ));
            }
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
            param_docs,
            param_has_default,
            doc,
        });
        self.table.scopes[scope].names.entry(name.to_string()).or_default().push(idx);
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
        // Implicit `this` (see `current_class`'s own doc comment): only
        // consulted once the ordinary scope-chain lookup above has
        // already failed, so a local of the same name always shadows a
        // field/method, matching real Kotlin. A found *method* resolves
        // structurally to its `Type::Function` shape here (so a bare
        // call like `foo()` isn't wrongly flagged as an undefined name by
        // this same function, reached via `ExprKind::Call`'s own initial
        // `walk_expr(callee)`) — the real, checkable call signature
        // (named-argument checking included) is built separately, by the
        // `Call` arm itself, via `class_method_signature`.
        if let Some(class_name) = self.current_class.clone() {
            if let Some(info) = self.class_info.get(class_name.as_ref()) {
                if let Some(f) = info.fields.iter().find(|f| f.name == name) {
                    self.table.occurrences.push((use_span, f.symbol_idx));
                    return f.ty.clone();
                }
                if let Some(m) = info.methods.iter().find(|m| m.name == name) {
                    self.table.occurrences.push((use_span, m.symbol_idx));
                    return Type::Function(m.param_types.clone(), Box::new(m.return_type.clone()));
                }
            }
            // Every class implicitly extends `Any` — a bare `toString()`/
            // `equals(...)`/`hashCode()` inside a method resolves to it
            // too. No resolver `Symbol` exists for a builtin method, so no
            // `occurrences` entry to record here.
            if let Some((ret, params, ..)) = crate::stdlib::builtin_method_signature("Any", name) {
                return Type::Function(params, Box::new(ret));
            }
        }
        if crate::stdlib::is_known(name) {
            return Type::Unknown;
        }
        // A name brought in by a real `import`/`@file:Import` — see
        // `CrossFileImports`. More than one candidate (a real cross-file
        // overload group) resolves to its *last* entry here, the same
        // documented simplification `resolve_symbol_index` already applies
        // to a local overload group for a non-call use — a real call site
        // gets true per-overload resolution instead, in the `Call` arm.
        if let Some(candidates) = self.imports.by_name.get(name) {
            if let Some(found) = candidates.last() {
                self.cross_file_refs.insert(
                    use_span,
                    CrossFileRef { file: found.file.clone(), def_span: found.symbol.def_span, symbol: found.symbol.clone() },
                );
                return found.symbol.ty.clone();
            }
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

    /// The *last*-declared symbol sharing `name`, from the nearest scope
    /// that declares it at all — a deliberate simplification for every
    /// non-call use (hover, `Assign`, generic name resolution): picking
    /// one of several real overloads to represent "the" symbol for these
    /// purposes is inherently lossy, but none of them need true per-
    /// overload resolution the way an actual call does — see
    /// `resolve_symbols` for that.
    fn resolve_symbol_index(&self, scope: usize, name: &str) -> Option<usize> {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            if let Some(indices) = self.table.scopes[s].names.get(name) {
                return indices.last().copied();
            }
            cur = self.table.scopes[s].parent;
        }
        None
    }

    /// Every symbol sharing `name` from the nearest scope that declares it
    /// at all — almost always exactly one, more than one only for a real
    /// local overload group (see `ScopeData::names`'s doc comment). Used
    /// only by `Call`, which needs the *whole* group to resolve which
    /// overload actually applies to a given call's argument types.
    fn resolve_symbols(&self, scope: usize, name: &str) -> Vec<&Symbol> {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            if let Some(indices) = self.table.scopes[s].names.get(name) {
                return indices.iter().map(|&idx| &self.table.symbols[idx]).collect();
            }
            cur = self.table.scopes[s].parent;
        }
        Vec::new()
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
    fn walk_block(&mut self, stmts: &[Stmt], parent: usize, open_at: Span, following: Span, kind: ScopeKind) -> Type {
        let child = self.open_scope(parent, open_at, following, kind);
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
                self.define(scope, name, kind, *name_span, *mutable, sig, ty, Type::Unknown, Vec::new(), Vec::new(), Vec::new(), Vec::new(), doc);
                Type::Unit
            }
            StmtKind::Assign { name, op, value } => {
                let value_ty = self.walk_expr(value, scope, following);
                match self.resolve_symbol(scope, name) {
                    Some(sym) => {
                        let target_ty = sym.ty.clone();
                        self.resolve_name(scope, name, stmt.span);
                        // Compound assignment (`+=` etc.) is out of scope
                        // for v1 — its result type would need the same
                        // widening rules as arithmetic, and isn't a plain
                        // "is the RHS compatible" check the way `=` is.
                        // `target_ty` is whatever the declaration's own
                        // type is — explicit hint or inferred from its
                        // initializer, no distinction (see `ValDecl`
                        // above) — so this enforces reassignment for both
                        // forms.
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
                    }
                    // Not a local at all — implicit `this.<name>` fallback
                    // (see `current_class`'s doc comment), with exactly
                    // the same mutability/type checking `FieldAssign`'s
                    // own real handling does (a local of this name always
                    // wins, per the `Some(sym)` arm above, matching real
                    // Kotlin).
                    None => {
                        let field = self.current_class.clone().and_then(|c| {
                            self.class_info.get(c.as_ref()).and_then(|info| {
                                info.fields.iter().find(|f| f.name == *name).map(|f| (f.ty.clone(), f.mutable, f.symbol_idx))
                            })
                        });
                        match field {
                            Some((field_ty, mutable, symbol_idx)) => {
                                self.touch(scope, stmt.span);
                                self.table.occurrences.push((stmt.span, symbol_idx));
                                if !mutable {
                                    self.type_errors
                                        .push(RuntimeError::new(format!("cannot reassign val '{name}'"), stmt.span));
                                }
                                if *op == AssignOp::Set
                                    && field_ty != Type::Unknown
                                    && value_ty != Type::Unknown
                                    && !types::is_assignable(&value_ty, &field_ty)
                                {
                                    self.type_errors.push(RuntimeError::new(
                                        format!(
                                            "cannot assign {} to field '{name}' of type {}",
                                            value_ty.name(),
                                            field_ty.name()
                                        ),
                                        stmt.span,
                                    ));
                                }
                            }
                            // Not a field either — a real cross-file `val`/
                            // `var` (`import`/`@file:Import`) gets the same
                            // mutability/type checking as a local or field
                            // reassignment above; `resolve_name` itself only
                            // resolves the *type*, with no such check (its
                            // callers up to now have all been reads).
                            None => {
                                let cross = self.imports.by_name.get(name).and_then(|c| c.last()).map(|found| {
                                    (found.symbol.ty.clone(), found.symbol.mutable, found.file.clone(), found.symbol.clone())
                                });
                                match cross {
                                    Some((target_ty, mutable, file, symbol)) => {
                                        let def_span = symbol.def_span;
                                        self.touch(scope, stmt.span);
                                        self.cross_file_refs.insert(stmt.span, CrossFileRef { file, def_span, symbol });
                                        if !mutable {
                                            self.type_errors.push(RuntimeError::new(
                                                format!("cannot reassign val '{name}'"),
                                                stmt.span,
                                            ));
                                        }
                                        if *op == AssignOp::Set
                                            && target_ty != Type::Unknown
                                            && value_ty != Type::Unknown
                                            && !types::is_assignable(&value_ty, &target_ty)
                                        {
                                            self.type_errors.push(RuntimeError::new(
                                                format!(
                                                    "cannot assign {} to var '{name}' of type {}",
                                                    value_ty.name(),
                                                    target_ty.name()
                                                ),
                                                stmt.span,
                                            ));
                                        }
                                    }
                                    None => {
                                        self.resolve_name(scope, name, stmt.span);
                                    }
                                }
                            }
                        }
                    }
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
                // Same source `walk_params` below pulls each param's own
                // `@param` text from — computed here too so the *function*
                // symbol can hand a specific parameter's doc to a named-
                // argument call site (see `walk_call_args`) without needing
                // the whole combined `fun_doc` block.
                let param_docs: Vec<Option<String>> = params
                    .iter()
                    .map(|p| parsed_doc.as_ref().and_then(|d| d.param(&p.name)).map(str::to_string))
                    .collect();
                let param_has_default: Vec<bool> = params.iter().map(|p| p.default.is_some()).collect();
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
                    param_docs,
                    param_has_default,
                    fun_doc,
                );
                let fn_scope = self.open_scope(scope, stmt.span, following, ScopeKind::FunctionBoundary);
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
                self.walk_block(body, scope, stmt.span, following, ScopeKind::Loop);
                self.narrowed = ambient;
                Type::Unit
            }
            StmtKind::DoWhile { body, cond } => {
                // Unlike `While`, `cond` is walked in the *body's own*
                // scope (opened once, shared by both), not `scope` — real
                // Kotlin/Java semantics: `do { val x = 1 } while (x > 0)`
                // sees `x`, since the body always runs before `cond` is
                // ever checked. No narrowing carried into the body either:
                // unlike `while`, the body isn't gated by `cond` being true
                // (it always runs at least once).
                let do_scope = self.open_scope(scope, stmt.span, following, ScopeKind::Loop);
                self.walk_stmts(body, do_scope, following);
                self.walk_expr(cond, do_scope, following);
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
                let for_scope = self.open_scope(scope, stmt.span, following, ScopeKind::Loop);
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
            StmtKind::ClassDecl { name, name_span, ctor_params, members, doc_comment } => {
                self.walk_class_decl(name, *name_span, ctor_params, members, doc_comment.as_deref(), scope, following, stmt.span);
                Type::Unit
            }
            StmtKind::FieldAssign { receiver, field, field_span, op, value } => {
                let recv_ty = self.walk_expr(receiver, scope, following);
                let value_ty = self.walk_expr(value, scope, following);
                if recv_ty != Type::Unknown {
                    match self.class_field_type(&recv_ty, field) {
                        Some((field_ty, mutable)) => {
                            if let Some(idx) = self.class_field_symbol_idx(&recv_ty, field) {
                                self.table.occurrences.push((*field_span, idx));
                            } else if let Some(cross_ref) = self.class_field_cross_file_ref(&recv_ty, field) {
                                self.cross_file_refs.insert(*field_span, cross_ref);
                            }
                            if !mutable {
                                self.type_errors.push(RuntimeError::new(format!("cannot reassign val '{field}'"), *field_span));
                            }
                            if *op == AssignOp::Set
                                && field_ty != Type::Unknown
                                && value_ty != Type::Unknown
                                && !types::is_assignable(&value_ty, &field_ty)
                            {
                                self.type_errors.push(RuntimeError::new(
                                    format!("cannot assign {} to field '{field}' of type {}", value_ty.name(), field_ty.name()),
                                    stmt.span,
                                ));
                            }
                        }
                        None => {
                            self.type_errors.push(RuntimeError::new(
                                format!("'{}' has no field '{field}'", recv_ty.name()),
                                *field_span,
                            ));
                        }
                    }
                }
                Type::Unit
            }
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
            self.define(fn_scope, &p.name, SymbolKind::Param, p.name_span, false, sig, ty, Type::Unknown, Vec::new(), Vec::new(), Vec::new(), Vec::new(), doc);
        }
    }

    /// Synthesizes a `Symbol` for a class member's own declaration site (a
    /// field or a method) and records its occurrence there — mirrors
    /// `named_arg_labels`'s existing "synthesize a `Symbol`, don't
    /// register it in any scope's `names`" pattern: a field/method is only
    /// ever reached through `this.name`/`obj.name`, dispatched via
    /// `class_info`, never as a bare, scope-visible name the way a
    /// top-level `fun`'s own name is — registering it into `scope.names`
    /// too would wrongly make it resolvable unqualified inside its own
    /// class. Wiring up hover/goto-def for a *use* site (`this.field`,
    /// `obj.method()`) is a later step of this phase (`ksl-lsp`) — this
    /// only covers the declaration itself.
    #[allow(clippy::too_many_arguments)]
    fn define_class_member(
        &mut self,
        kind: SymbolKind,
        name: &str,
        def_span: Span,
        mutable: bool,
        signature: String,
        ty: Type,
        return_type: Type,
        param_types: Vec<Type>,
        param_names: Vec<String>,
        param_docs: Vec<Option<String>>,
        param_has_default: Vec<bool>,
        doc: Option<String>,
    ) -> usize {
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
            param_docs,
            param_has_default,
            doc,
        });
        self.table.occurrences.push((def_span, idx));
        idx
    }

    /// Real static integration for a `class` declaration (this phase's
    /// plan, §4/§6 step 5) — the class's own name (a bare-visible `Symbol`,
    /// kind `Class`), then its full static shape into `self.class_info` in
    /// two passes: every field's/method's own *signature* first (from
    /// annotations only, no bodies walked, no diagnostics — mirrors
    /// `StmtKind::FunDecl`'s own quiet `param_types`/`ret_ty` computed
    /// before `define()`), so one member referencing another (either
    /// order — `this.other()` calling a method declared later in the same
    /// class) resolves correctly; then real bodies (property
    /// initializers/`init` blocks/method bodies), each with real
    /// diagnostics and, for an inferred (unannotated) property/method
    /// return type, a patch back into `class_info` once the real type is
    /// known — again mirroring `FunDecl`'s own after-the-body patch.
    #[allow(clippy::too_many_arguments)]
    fn walk_class_decl(
        &mut self,
        name: &str,
        name_span: Span,
        ctor_params: &[ClassParam],
        members: &[ClassMember],
        doc_comment: Option<&str>,
        scope: usize,
        following: Span,
        own_span: Span,
    ) {
        let doc = doc_comment.map(crate::doc_comment::parse).map(|d| d.summary).filter(|s| !s.is_empty());
        let ctor_sig_params = ctor_params
            .iter()
            .map(|cp| match &cp.param.type_annotation {
                Some(ann) => format!("{}: {}", cp.param.name, type_annotation_display(ann)),
                None => cp.param.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        // A bare reference to the class's own name (`val c = Point`, no
        // call) is the class *value* itself — `Type::Unknown` at runtime
        // (see `Type::from_value`'s `Value::Class` arm), deliberately not
        // `Type::Class(name)`, which is reserved for a real *instance*'s
        // type (`this`, and a constructed `Point(...)` call's result).
        // `param_types`/`param_names` are patched in below, once the
        // constructor's own real signature is computed — `ksl-lsp`'s
        // completion reads them straight off this `Symbol` (`named_arg_
        // completions`) to offer `paramName =` inside `Point(...)`'s own
        // argument list, the same way it already does for an ordinary
        // `fun`'s `Symbol`.
        let class_idx = self.define(
            scope,
            name,
            SymbolKind::Class,
            name_span,
            false,
            format!("class {name}({ctor_sig_params})"),
            Type::Unknown,
            Type::Unknown,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            doc,
        );

        let mut fields = Vec::new();
        for cp in ctor_params {
            if let Some(mutable) = cp.field {
                let ty = cp
                    .param
                    .type_annotation
                    .as_ref()
                    .map(|a| types::resolve_type_annotation(a, &self.generic_params, &self.known_classes))
                    .unwrap_or(Type::Unknown);
                // `symbol_idx` is a placeholder here — patched below, once
                // the ctor param's own real `Symbol` is defined into
                // `ctor_scope` (Phase 2 defines it; Phase 1 only computes
                // signatures).
                fields.push(ClassFieldInfo { name: cp.param.name.clone(), ty, mutable, symbol_idx: 0 });
            }
        }
        for m in members {
            if let ClassMember::Property { name: fname, type_annotation, mutable, .. } = m {
                let ty = type_annotation
                    .as_ref()
                    .map(|a| types::resolve_type_annotation(a, &self.generic_params, &self.known_classes))
                    .unwrap_or(Type::Unknown);
                // Patched below too, once `define_class_member` runs for
                // this property in Phase 2.
                fields.push(ClassFieldInfo { name: fname.clone(), ty, mutable: *mutable, symbol_idx: 0 });
            }
        }
        let mut methods = Vec::new();
        for m in members {
            if let ClassMember::Method(method_stmt) = m {
                if let StmtKind::FunDecl { name: mname, return_type, params, .. } = &method_stmt.kind {
                    let ret_ty = return_type
                        .as_ref()
                        .map(|a| types::resolve_type_annotation(a, &self.generic_params, &self.known_classes))
                        .unwrap_or(Type::Unknown);
                    let param_types = params
                        .iter()
                        .map(|p| {
                            p.type_annotation
                                .as_ref()
                                .map(|a| types::resolve_type_annotation(a, &self.generic_params, &self.known_classes))
                                .unwrap_or(Type::Unknown)
                        })
                        .collect();
                    let param_names = params.iter().map(|p| p.name.clone()).collect();
                    let param_has_default: Vec<bool> = params.iter().map(|p| p.default.is_some()).collect();
                    methods.push(ClassMethodInfo {
                        name: mname.clone(),
                        return_type: ret_ty,
                        param_types,
                        param_names,
                        param_docs: vec![None; params.len()],
                        param_has_default,
                        // Patched in `walk_class_method`, once its real
                        // `define_class_member` call returns a real index.
                        symbol_idx: 0,
                    });
                }
            }
        }
        let ctor_param_types: Vec<Type> = ctor_params
            .iter()
            .map(|cp| {
                cp.param
                    .type_annotation
                    .as_ref()
                    .map(|a| types::resolve_type_annotation(a, &self.generic_params, &self.known_classes))
                    .unwrap_or(Type::Unknown)
            })
            .collect();
        let ctor_param_names: Vec<String> = ctor_params.iter().map(|cp| cp.param.name.clone()).collect();
        let ctor_param_has_default: Vec<bool> = ctor_params.iter().map(|cp| cp.param.default.is_some()).collect();
        self.table.symbols[class_idx].param_types = ctor_param_types.clone();
        self.table.symbols[class_idx].param_names = ctor_param_names.clone();
        self.table.symbols[class_idx].param_has_default = ctor_param_has_default.clone();
        self.class_info.insert(
            Rc::from(name),
            ClassInfo { fields, methods, ctor_param_types, ctor_param_names, ctor_param_has_default },
        );

        // The constructor's own scope (ctor params, bare — property
        // initializers/`init` blocks see them directly, matching
        // `construct_object`'s `ctor_env`) and each method's own body
        // scope (built by `walk_class_method`) are *siblings*, both
        // direct children of `scope` — not one nested inside the other —
        // so a method body never sees the primary constructor's own
        // (non-promoted) parameters, matching runtime exactly (a method
        // call's own frame is `class.closure.child()`, never `ctor_env`).
        // `this` is defined separately in each one (cheap, harmless
        // duplication) rather than shared through a common parent scope,
        // for the same reason.
        let ctor_scope = self.open_scope(scope, own_span, following, ScopeKind::Other);
        self.define(
            ctor_scope,
            "this",
            SymbolKind::Val,
            name_span,
            false,
            format!("this: {name}"),
            Type::Class(Rc::from(name)),
            Type::Unknown,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        );
        for cp in ctor_params {
            let ty = self.resolve_type_annotation_here(cp.param.type_annotation.as_ref());
            if cp.param.type_annotation.is_none() {
                self.type_errors.push(RuntimeError::new(
                    format!("constructor parameter '{}' must have an explicit type annotation", cp.param.name),
                    cp.param.name_span,
                ));
            }
            let sig = match &ty {
                Type::Unknown => format!("param {}", cp.param.name),
                t => format!("param {}: {}", cp.param.name, t.name()),
            };
            let param_idx = self.define(
                ctor_scope,
                &cp.param.name,
                SymbolKind::Param,
                cp.param.name_span,
                cp.field == Some(true),
                sig,
                ty,
                Type::Unknown,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            );
            // A promoted field (`val`/`var` ctor param) has no separate
            // declaration site of its own — `this.x`'s own use sites point
            // back at the ctor param's real symbol directly (see
            // `ClassFieldInfo::symbol_idx`'s doc comment).
            if cp.field.is_some() {
                if let Some(info) = self.class_info.get_mut(name) {
                    if let Some(f) = info.fields.iter_mut().find(|f| f.name == cp.param.name) {
                        f.symbol_idx = param_idx;
                    }
                }
                self.table.class_members.entry(name.to_string()).or_default().push(param_idx);
            }
        }

        // Implicit `this` (see `current_class`'s own doc comment) is in
        // effect for every member body below — property initializers,
        // `init` blocks, and (via `walk_class_method`) every method body —
        // restored to whatever it was before (always `None` today, since
        // classes can't nest, but this stays correct if that ever
        // changes).
        let outer_class = self.current_class.replace(Rc::from(name));
        for m in members {
            match m {
                ClassMember::Property { name: fname, name_span: fspan, type_annotation, mutable, init, doc_comment } => {
                    let expected = type_annotation.as_ref().map(|a| self.resolve_type_annotation_here(Some(a)));
                    let init_ty = self.walk_expr_expected(init, ctor_scope, following, expected.as_ref());
                    let field_ty = match &expected {
                        Some(t) => {
                            if init_ty != Type::Unknown && *t != Type::Unknown && !types::is_assignable(&init_ty, t) {
                                self.type_errors.push(RuntimeError::new(
                                    format!(
                                        "property '{fname}' has type {}, but its initializer has type {}",
                                        t.name(),
                                        init_ty.name()
                                    ),
                                    init.span,
                                ));
                            }
                            t.clone()
                        }
                        None => {
                            if init_ty == Type::Unknown {
                                self.type_errors.push(RuntimeError::new(
                                    format!("cannot infer type of property '{fname}'; add an explicit type annotation"),
                                    *fspan,
                                ));
                            }
                            init_ty
                        }
                    };
                    if let Some(info) = self.class_info.get_mut(name) {
                        if let Some(f) = info.fields.iter_mut().find(|f| f.name == *fname) {
                            f.ty = field_ty.clone();
                        }
                    }
                    let sig = match &field_ty {
                        Type::Unknown => format!("{} {fname}", if *mutable { "var" } else { "val" }),
                        t => format!("{} {fname}: {}", if *mutable { "var" } else { "val" }, t.name()),
                    };
                    let member_doc =
                        doc_comment.as_deref().map(crate::doc_comment::parse).map(|d| d.summary).filter(|s| !s.is_empty());
                    let field_idx = self.define_class_member(
                        SymbolKind::Field,
                        fname,
                        *fspan,
                        *mutable,
                        sig,
                        field_ty,
                        Type::Unknown,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        member_doc,
                    );
                    if let Some(info) = self.class_info.get_mut(name) {
                        if let Some(f) = info.fields.iter_mut().find(|f| f.name == *fname) {
                            f.symbol_idx = field_idx;
                        }
                    }
                    self.table.class_members.entry(name.to_string()).or_default().push(field_idx);
                }
                ClassMember::Init(stmts) => {
                    self.walk_stmts(stmts, ctor_scope, following);
                }
                ClassMember::Method(method_stmt) => {
                    self.walk_class_method(method_stmt, name, scope, following);
                }
            }
        }
        self.current_class = outer_class;
    }

    /// A class method's own body — near-identical to `StmtKind::FunDecl`'s
    /// own handling (same return-type inference, same doc-comment
    /// building), except the method's own name gets a synthesized,
    /// non-scope-registered `Symbol` (`define_class_member`, not
    /// `define` — see its doc comment) rather than a bare-callable one,
    /// and its own body scope additionally carries a real `this` (typed
    /// `Type::Class(class_name)`). An inferred (unannotated) return type
    /// is patched back into `self.class_info[class_name]`'s own entry
    /// once known, mirroring `FunDecl`'s identical patch of its own
    /// symbol.
    fn walk_class_method(&mut self, stmt: &Stmt, class_name: &str, scope: usize, following: Span) {
        let StmtKind::FunDecl { name, name_span, params, return_type, body, doc_comment } = &stmt.kind else {
            return;
        };
        let parsed_doc = doc_comment.as_deref().map(crate::doc_comment::parse);
        let fun_doc = parsed_doc.as_ref().and_then(|d| {
            if d.summary.is_empty() && d.returns.is_none() && d.params.is_empty() {
                None
            } else {
                let mut parts = Vec::new();
                if !d.summary.is_empty() {
                    parts.push(d.summary.clone());
                }
                if !d.params.is_empty() {
                    let mut block = String::from("**Parameters:**");
                    for (pname, desc) in &d.params {
                        block.push_str(&format!("\n- `{pname}` — {desc}"));
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
        let param_types: Vec<Type> = params
            .iter()
            .map(|p| match &p.type_annotation {
                Some(ann) => types::resolve_type_annotation(ann, &self.generic_params, &self.known_classes),
                None => Type::Unknown,
            })
            .collect();
        let param_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
        let param_docs: Vec<Option<String>> =
            params.iter().map(|p| parsed_doc.as_ref().and_then(|d| d.param(&p.name)).map(str::to_string)).collect();
        let param_has_default: Vec<bool> = params.iter().map(|p| p.default.is_some()).collect();
        let has_return_annotation = return_type.is_some();

        let method_idx = self.define_class_member(
            SymbolKind::Method,
            name,
            *name_span,
            false,
            format!("fun {name}({})", param_list(params)),
            Type::Function(param_types.clone(), Box::new(ret_ty.clone())),
            ret_ty.clone(),
            param_types.clone(),
            param_names.clone(),
            param_docs.clone(),
            param_has_default,
            fun_doc,
        );
        if let Some(info) = self.class_info.get_mut(class_name) {
            if let Some(m) = info.methods.iter_mut().find(|m| m.name == *name) {
                m.symbol_idx = method_idx;
            }
        }
        self.table.class_members.entry(class_name.to_string()).or_default().push(method_idx);

        let fn_scope = self.open_scope(scope, stmt.span, following, ScopeKind::FunctionBoundary);
        self.define(
            fn_scope,
            "this",
            SymbolKind::Val,
            *name_span,
            false,
            format!("this: {class_name}"),
            Type::Class(Rc::from(class_name)),
            Type::Unknown,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        );
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
                if matches!(body, FunBody::Expr(_))
                    && body_ty != Type::Unknown
                    && declared != Type::Unknown
                    && !types::is_assignable(&body_ty, &declared)
                {
                    self.type_errors.push(RuntimeError::new(
                        format!("return type mismatch: expected {}, got {}", declared.name(), body_ty.name()),
                        stmt.span,
                    ));
                }
                declared
            }
            ReturnTypeContext::Collect(mut seen) => {
                let falls_through = match body {
                    FunBody::Expr(_) => true,
                    FunBody::Block(stmts) => !stmts.last().map(always_returns).unwrap_or(false),
                };
                if falls_through {
                    seen.push(body_ty.clone());
                }
                match reconcile_types(&seen) {
                    Reconciled::Agreed(t) => t,
                    Reconciled::Nothing => {
                        self.type_errors.push(RuntimeError::new(
                            format!("cannot infer return type of method '{name}'; add an explicit return type annotation"),
                            *name_span,
                        ));
                        Type::Unknown
                    }
                    Reconciled::Disagreement(a, b) => {
                        self.type_errors.push(RuntimeError::new(
                            format!("method '{name}' returns both {} and {}", a.name(), b.name()),
                            stmt.span,
                        ));
                        Type::Unknown
                    }
                }
            }
        };
        if let Some(info) = self.class_info.get_mut(class_name) {
            if let Some(m) = info.methods.iter_mut().find(|m| m.name == *name) {
                m.return_type = final_ret_ty.clone();
            }
        }
        // The signature built at `define_class_member()` time above (before
        // the body was walked) couldn't have known this yet — mirrors
        // `StmtKind::FunDecl`'s identical patch of its own symbol's
        // signature string, for the same reason (an unannotated return
        // type is only known after walking the body).
        self.table.symbols[method_idx].signature = match &final_ret_ty {
            Type::Unknown => format!("fun {name}({})", param_list(params)),
            t => format!("fun {name}({}): {}", param_list(params), t.name()),
        };
        // The method's own `Symbol` (found via `SymbolTable::class_members`,
        // which — unlike `class_info` — survives past this single resolve
        // call, e.g. into `PackageIndex`'s cross-file class shapes) needs
        // this patched too, not just `class_info`'s private copy — an
        // earlier version of this function only patched the two above,
        // leaving `class_members()`'s own `Symbol.return_type` stale at
        // `Type::Unknown` for any method with an inferred (unannotated)
        // return type.
        self.table.symbols[method_idx].return_type = final_ret_ty;
    }

    /// The full checkable signature of `t.method(...)` for a real value
    /// receiver (a package-qualified path never reaches this — see
    /// `ExprKind::MethodCall`'s own handling) — unifies dispatch for a
    /// builtin receiver (`Int`, `String`, ...), a same-file user class
    /// (`Type::Class(name)` found in `self.class_info`), and an *imported*
    /// class (found instead via `self.packages.find_class` — its own
    /// `PackageMember.class_members`, the cross-file counterpart of
    /// `class_info` populated once at indexing time, see
    /// `package_index::PackageMember`'s doc comment): either way, a
    /// user class's own method wins if declared, else falls through to
    /// `Any`'s native methods (every user class implicitly extends `Any`,
    /// single hop — see `ClassInfo`'s doc comment), exactly mirroring
    /// `class::lookup_method`'s runtime rule. `None` for an `Unknown`
    /// receiver (deferred, not an error) or a receiver type with no such
    /// method anywhere in its chain.
    fn class_method_signature(&self, t: &Type, method: &str) -> Option<CallSignature> {
        let t = match t {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        if *t == Type::Unknown {
            return None;
        }
        let name = t.name();
        if let Some(info) = self.class_info.get(name.as_str()) {
            if let Some(m) = info.methods.iter().find(|m| m.name == method) {
                return Some((
                    m.return_type.clone(),
                    m.param_types.clone(),
                    m.param_names.clone(),
                    m.param_docs.clone(),
                    m.param_has_default.clone(),
                ));
            }
            return builtin_method_signature_as_call_signature("Any", method);
        }
        if let Some(pm) = self.packages.find_class(&name) {
            if let Some(m) = pm.class_members.as_ref().and_then(|members| members.iter().find(|m| m.name == method)) {
                return Some((
                    m.return_type.clone(),
                    m.param_types.clone(),
                    m.param_names.clone(),
                    m.param_docs.clone(),
                    m.param_has_default.clone(),
                ));
            }
            return builtin_method_signature_as_call_signature("Any", method);
        }
        builtin_method_signature_as_call_signature(&name, method)
    }

    /// `(field's type, mutable)` for `t.field`/`t.field = ...` — same-file
    /// `class_info` first, then an imported class's own shape via
    /// `self.packages.find_class` (see `class_method_signature`'s
    /// identical two-tier lookup, just without the `Any` fallback — `Any`
    /// has no fields, so a class's own miss here is always `None`).
    fn class_field_type(&self, t: &Type, field: &str) -> Option<(Type, bool)> {
        let t = match t {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        let name = t.name();
        if let Some(info) = self.class_info.get(name.as_str()) {
            return info.fields.iter().find(|f| f.name == field).map(|f| (f.ty.clone(), f.mutable));
        }
        self.packages
            .find_class(&name)?
            .class_members
            .as_ref()?
            .iter()
            .find(|f| f.name == field)
            .map(|f| (f.ty.clone(), f.mutable))
    }

    /// The cross-file `CrossFileRef` for `t.field`, when `field` is only
    /// found through an *imported* class's shape (never the local,
    /// same-file `class_info` — `class_field_symbol_idx` already covers
    /// that case with a real, same-file `Symbol` index) — lets a
    /// `this.field`/`obj.field` use site on an imported instance still get
    /// real goto-def/hover, exactly like a plain cross-file function call
    /// already does (see `cross_file_refs`'s own doc comment).
    fn class_field_cross_file_ref(&self, t: &Type, field: &str) -> Option<CrossFileRef> {
        let t = match t {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        let name = t.name();
        if self.class_info.contains_key(name.as_str()) {
            return None;
        }
        let pm = self.packages.find_class(&name)?;
        let member = pm.class_members.as_ref()?.iter().find(|m| m.name == field)?;
        Some(CrossFileRef { file: pm.file.clone(), def_span: member.def_span, symbol: member.clone() })
    }

    /// Same as `class_field_cross_file_ref`, for a method (`t.method(...)`).
    fn class_method_cross_file_ref(&self, t: &Type, method: &str) -> Option<CrossFileRef> {
        let t = match t {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        let name = t.name();
        if self.class_info.contains_key(name.as_str()) {
            return None;
        }
        let pm = self.packages.find_class(&name)?;
        let member = pm.class_members.as_ref()?.iter().find(|m| m.name == method)?;
        Some(CrossFileRef { file: pm.file.clone(), def_span: member.def_span, symbol: member.clone() })
    }

    /// The field's own declaration-site `Symbol` index, for recording a
    /// real `occurrences` entry at a `this.field`/`obj.field` *use* site —
    /// see `ClassFieldInfo::symbol_idx`'s doc comment. `None` in exactly
    /// the same cases `class_field_type` returns `None`.
    fn class_field_symbol_idx(&self, t: &Type, field: &str) -> Option<usize> {
        let t = match t {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        let name = t.name();
        self.class_info.get(name.as_str()).and_then(|info| info.fields.iter().find(|f| f.name == field).map(|f| f.symbol_idx))
    }

    /// The method's own declaration-site `Symbol` index — `None` both when
    /// no such method exists at all *and* when it resolved only through
    /// `Any`'s native fallback (which has no resolver-side `Symbol` of its
    /// own to point at; see `class_method_signature`'s identical fallback).
    fn class_method_symbol_idx(&self, t: &Type, method: &str) -> Option<usize> {
        let t = match t {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        let name = t.name();
        self.class_info.get(name.as_str()).and_then(|info| info.methods.iter().find(|m| m.name == method).map(|m| m.symbol_idx))
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
            self.define(fn_scope, &p.name, SymbolKind::Param, p.name_span, false, sig, inferred.clone(), Type::Unknown, Vec::new(), Vec::new(), Vec::new(), Vec::new(), None);
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
        let lambda_scope = self.open_scope(scope, expr.span, following, ScopeKind::FunctionBoundary);
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
    /// recorded), just with no expected type propagated. Its 4th element is
    /// one `@param` description per declared parameter (same order as the
    /// other three), used below to give a named argument's own label a
    /// hover `Symbol` of its own. Returns each argument's own name (if any)
    /// and inferred type, for `check_call_signature` below to check against
    /// `signature`.
    fn walk_call_args(
        &mut self,
        args: &[Arg],
        signature: Option<&CallSignature>,
        scope: usize,
        following: Span,
    ) -> Vec<(Option<String>, Type)> {
        let mut arg_infos: Vec<(Option<String>, Type)> = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let named_param_idx = a.name.as_ref().and_then(|n| {
                signature.and_then(|(_, _, param_names, _, _)| param_names.iter().position(|p| p == n))
            });
            let expected_param_ty = signature.and_then(|(_, param_types, _, _, _)| match &a.name {
                Some(_) => named_param_idx.and_then(|i| param_types.get(i)),
                None => param_types.get(i),
            });
            let t = self.walk_expr_expected(&a.value, scope, following, expected_param_ty);
            // A named argument's own label (`name` in `f(name = ...)`) gets
            // a synthesized `Symbol` for hover — built here, not derived
            // from `occurrence_at`, since it doesn't refer to any locally
            // visible declaration of its own (the callee's parameter might
            // be declared in another file entirely, or be a stdlib/cross-
            // file placeholder with no real name to match at all).
            if let (Some(name), Some(name_span), Some(idx)) = (&a.name, a.name_span, named_param_idx) {
                if let Some((_, param_types, _, param_docs, _)) = signature {
                    if let Some(param_ty) = param_types.get(idx) {
                        let sig = match param_ty {
                            Type::Unknown => format!("param {name}"),
                            t => format!("param {name}: {}", t.name()),
                        };
                        let doc = param_docs.get(idx).cloned().flatten();
                        self.table.named_arg_labels.push((
                            name_span,
                            Symbol {
                                name: name.clone(),
                                kind: SymbolKind::Param,
                                def_span: name_span,
                                mutable: false,
                                signature: sig,
                                ty: param_ty.clone(),
                                return_type: Type::Unknown,
                                param_types: Vec::new(),
                                param_names: Vec::new(),
                                param_docs: Vec::new(),
                                param_has_default: Vec::new(),
                                doc,
                            },
                        ));
                    }
                }
            }
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
        signature: Option<CallSignature>,
        span: Span,
    ) -> Type {
        let Some((ret_ty, param_types, param_names, _, param_has_default)) = signature else {
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
        // Collected up front (rather than filtering `claimed` lazily) so the
        // loop below can mark each consumed slot `claimed[i] = true` as it
        // goes — needed by the arity check further down, which must see
        // every slot a *positional* argument filled, not just a named one.
        let remaining_indices: Vec<usize> = (0..param_types.len()).filter(|&i| !claimed[i]).collect();
        let mut remaining = remaining_indices.into_iter();
        for (arg_name, arg_ty) in arg_infos {
            if arg_name.is_some() {
                continue;
            }
            let Some(i) = remaining.next() else { continue };
            claimed[i] = true;
            if let Some(param_ty) = param_types.get(i) {
                if *arg_ty != Type::Unknown && *param_ty != Type::Unknown && !types::is_assignable(arg_ty, param_ty) {
                    self.type_errors.push(RuntimeError::new(
                        format!("argument {} has type {}, expected {}", i + 1, arg_ty.name(), param_ty.name()),
                        span,
                    ));
                }
            }
        }
        // Arity — every required (no `= default`) parameter must have been
        // claimed by some argument, and there can't be more arguments than
        // declared parameters — only checked when `param_names` are real,
        // per-parameter names (a stdlib call or a `(Int) -> Int`-typed
        // value's placeholder blank names are deliberately never arity-
        // checked here, matching `walk_call_args`/the two passes above,
        // which already treat a blank name as "no real identity to match" —
        // see `CallSignature`'s own doc comment). Mirrors `bind_params`'s
        // identical runtime checks (`interpreter.rs`), word for word, so a
        // call that fails statically here would have failed the same way
        // at runtime otherwise.
        let has_real_names = !param_names.is_empty() && param_names.iter().all(|n| !n.is_empty());
        if has_real_names {
            if arg_infos.len() > param_types.len() {
                self.type_errors.push(RuntimeError::new(
                    format!("too many arguments: expected at most {}, got {}", param_types.len(), arg_infos.len()),
                    span,
                ));
            }
            for i in 0..param_types.len() {
                if !claimed[i] && !param_has_default.get(i).copied().unwrap_or(false) {
                    self.type_errors.push(RuntimeError::new(format!("missing argument '{}'", param_names[i]), span));
                }
            }
        }
        ret_ty
    }

    /// Real, Kotlin-style overload resolution for a call to a stdlib name
    /// with more than one overload (`abs`/`max`/`min`/`sqrt` today) — by
    /// arity and actual argument type, via `types::resolve_overload`, not
    /// a hardcoded name list tied to numeric types. Shared by `Call`
    /// (bare name) and `MethodCall` (fully package-qualified path) so a
    /// stdlib overload set resolves identically either way — `entries`
    /// comes from `stdlib::stdlib_overloads`/`stdlib_qualified_overloads`
    /// respectively.
    ///
    /// - No overload accepts this many arguments: always a real error —
    ///   arity is a syntactic fact, `Type::Unknown` can't excuse it.
    /// - Zero type-compatible overloads at the right arity, and every
    ///   argument's own type is concrete (no `Unknown` anywhere): a
    ///   genuine mismatch, reported the same way.
    /// - Exactly one type-compatible overload: its parameters are already
    ///   known compatible, so nothing left to flag — just its return type.
    /// - More than one still-possible match (typically because an
    ///   argument's own type is `Unknown`): if they all happen to agree
    ///   on return type regardless of which actually applies, that's safe
    ///   to report (matches `sqrt`'s two overloads, both `Double`);
    ///   otherwise `Type::Unknown`, deferred, no error — can't be sure
    ///   it's really wrong.
    fn resolve_stdlib_overload(
        &mut self,
        name: &str,
        entries: &[&'static crate::stdlib::BuiltinSymbol],
        arg_infos: &[(Option<String>, Type)],
        span: Span,
    ) -> Type {
        let arg_types: Vec<Type> = arg_infos.iter().map(|(_, t)| t.clone()).collect();
        let candidate_params: Vec<Vec<Type>> =
            entries.iter().map(|s| s.params.iter().map(crate::stdlib::BuiltinParam::real_type).collect()).collect();
        let candidate_param_slices: Vec<&[Type]> = candidate_params.iter().map(Vec::as_slice).collect();
        match types::resolve_overload(&candidate_param_slices, &arg_types) {
            types::OverloadResolution::Unique(i) => entries[i].return_type.clone(),
            types::OverloadResolution::Ambiguous(is) => {
                let first_ret = entries[is[0]].return_type.clone();
                if is.iter().all(|&i| entries[i].return_type == first_ret) { first_ret } else { Type::Unknown }
            }
            types::OverloadResolution::ArityMismatch => {
                self.type_errors.push(RuntimeError::new(
                    format!("no overload of '{name}' accepts {} argument(s)", arg_types.len()),
                    span,
                ));
                Type::Unknown
            }
            types::OverloadResolution::NoMatch => {
                if arg_types.iter().all(|t| *t != Type::Unknown) {
                    let arg_list = arg_types.iter().map(Type::name).collect::<Vec<_>>().join(", ");
                    self.type_errors.push(RuntimeError::new(
                        format!("no overload of '{name}' matches argument types ({arg_list})"),
                        span,
                    ));
                }
                Type::Unknown
            }
        }
    }

    /// Same idea as `resolve_stdlib_overload`, for a call to more than one
    /// local `fun` sharing one name (real user-defined overloading — see
    /// `ScopeData::names`'s doc comment) — the exact same
    /// `types::resolve_overload` core, just fed from `Symbol.param_types`/
    /// `return_type` instead of `BuiltinSymbol`, so the mechanism is
    /// identical regardless of whether the function being called is a
    /// builtin or user-declared. The one addition: a `Unique` match also
    /// records `call_span -> chosen.def_span` in `resolved_calls` — the
    /// decision a later execution pass can use to skip redoing this same
    /// search by runtime argument type (see `ResolveResult::resolved_calls`'s
    /// doc comment).
    /// `candidates` is `(param_types, return_type, def_span)` per local
    /// overload — cloned out of the resolver's own `Symbol` table by the
    /// caller (rather than borrowed as `&Symbol`) purely so this can take
    /// `&mut self` (for `type_errors`/`resolved_calls`) without the
    /// borrow checker seeing a conflicting live borrow of `self.table`.
    fn resolve_user_overload(
        &mut self,
        name: &str,
        candidates: &[(Vec<Type>, Type, Span)],
        arg_infos: &[(Option<String>, Type)],
        span: Span,
    ) -> Type {
        let arg_types: Vec<Type> = arg_infos.iter().map(|(_, t)| t.clone()).collect();
        let candidate_params: Vec<&[Type]> = candidates.iter().map(|(p, _, _)| p.as_slice()).collect();
        match types::resolve_overload(&candidate_params, &arg_types) {
            types::OverloadResolution::Unique(i) => {
                self.resolved_calls.insert(span, candidates[i].2);
                candidates[i].1.clone()
            }
            types::OverloadResolution::Ambiguous(is) => {
                let first_ret = candidates[is[0]].1.clone();
                if is.iter().all(|&i| candidates[i].1 == first_ret) { first_ret } else { Type::Unknown }
            }
            types::OverloadResolution::ArityMismatch => {
                self.type_errors.push(RuntimeError::new(
                    format!("no overload of '{name}' accepts {} argument(s)", arg_types.len()),
                    span,
                ));
                Type::Unknown
            }
            types::OverloadResolution::NoMatch => {
                if arg_types.iter().all(|t| *t != Type::Unknown) {
                    let arg_list = arg_types.iter().map(Type::name).collect::<Vec<_>>().join(", ");
                    self.type_errors.push(RuntimeError::new(
                        format!("no overload of '{name}' matches argument types ({arg_list})"),
                        span,
                    ));
                }
                Type::Unknown
            }
        }
    }

    /// Same idea as `resolve_stdlib_overload`/`resolve_user_overload`, for a
    /// call to a name brought in by a real cross-file `import`/
    /// `@file:Import` (see `CrossFileImports`) — or a qualified call
    /// (`a.b.c(...)`) resolved directly against `self.packages` — with more
    /// than one overload in its source package. Same `types::resolve_overload`
    /// core, fed from `PackageMember::symbol`. A `Unique` match records
    /// `call_span -> (file, def_span)` in `cross_file_refs` — the cross-file
    /// counterpart of `resolve_user_overload`'s `resolved_calls` entry.
    fn resolve_cross_file_overload(
        &mut self,
        name: &str,
        candidates: &[crate::package_index::PackageMember],
        arg_infos: &[(Option<String>, Type)],
        span: Span,
    ) -> Type {
        let arg_types: Vec<Type> = arg_infos.iter().map(|(_, t)| t.clone()).collect();
        let candidate_params: Vec<Vec<Type>> = candidates.iter().map(|m| m.symbol.param_types.clone()).collect();
        let candidate_param_slices: Vec<&[Type]> = candidate_params.iter().map(Vec::as_slice).collect();
        match types::resolve_overload(&candidate_param_slices, &arg_types) {
            types::OverloadResolution::Unique(i) => {
                let m = &candidates[i];
                self.cross_file_refs.insert(
                    span,
                    CrossFileRef { file: m.file.clone(), def_span: m.symbol.def_span, symbol: m.symbol.clone() },
                );
                m.symbol.return_type.clone()
            }
            types::OverloadResolution::Ambiguous(is) => {
                let first_ret = candidates[is[0]].symbol.return_type.clone();
                if is.iter().all(|&i| candidates[i].symbol.return_type == first_ret) { first_ret } else { Type::Unknown }
            }
            types::OverloadResolution::ArityMismatch => {
                self.type_errors.push(RuntimeError::new(
                    format!("no overload of '{name}' accepts {} argument(s)", arg_types.len()),
                    span,
                ));
                Type::Unknown
            }
            types::OverloadResolution::NoMatch => {
                if arg_types.iter().all(|t| *t != Type::Unknown) {
                    let arg_list = arg_types.iter().map(Type::name).collect::<Vec<_>>().join(", ");
                    self.type_errors.push(RuntimeError::new(
                        format!("no overload of '{name}' matches argument types ({arg_list})"),
                        span,
                    ));
                }
                Type::Unknown
            }
        }
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
    /// Thin wrapper over `walk_expr_expected_inner` — records every
    /// expression's own span and computed `Type` into `self.table.
    /// expr_types` (`ksl-lsp`'s completion reads this, via `SymbolTable::
    /// type_ending_before`, to find "what's the static type of whatever
    /// sits right before this `.`", entirely from the AST rather than
    /// re-parsing text). Wrapping the whole function this way — rather
    /// than pushing at the tail of its own body — is what makes this
    /// correct even for `ExprKind::Call`'s own early `return`s (real
    /// overload resolution shortcuts several hundred lines into that
    /// arm): `return` only ever unwinds the *inner* function, so every
    /// exit path still passes back through this wrapper exactly once
    /// before reaching any caller. Every recursive call inside `_inner`'s
    /// own body still says `self.walk_expr`/`self.walk_expr_expected` (its
    /// pre-rename name) — after this split, that name refers to *this*
    /// wrapper, so nested sub-expressions get recorded too, not just the
    /// outermost one.
    fn walk_expr_expected(&mut self, expr: &Expr, scope: usize, following: Span, expected: Option<&Type>) -> Type {
        let ty = self.walk_expr_expected_inner(expr, scope, following, expected);
        self.table.expr_types.push((expr.span, ty.clone()));
        ty
    }

    fn walk_expr_expected_inner(&mut self, expr: &Expr, scope: usize, following: Span, expected: Option<&Type>) -> Type {
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
            // `receiver.field` — this resolver has no notion of packages, so
            // it can't tell a local field access apart from a package-
            // qualified reference any more than the interpreter can at
            // parse time (see `ExprKind::FieldAccess`'s doc comment).
            // Mirrors `Interpreter::dotted_path_if_unresolved`'s approach:
            // if the whole chain is bare identifiers and its base name does
            // *not* resolve to a real local declaration, record it in
            // `qualified_refs` (never flagged as undefined — deferred to
            // `ksl-lsp`'s cross-file package index, or to the interpreter
            // at runtime) instead of walking it as an ordinary expression.
            // A receiver that *is* a real local looks up the field for
            // real via `class_field_type` (a real `Type::Class` receiver —
            // including `this`) — "has no field" only when the receiver's
            // own type is concretely known and genuinely has none (an
            // `Unknown` receiver is an already-reported problem elsewhere,
            // never a second cascading diagnostic here).
            ExprKind::FieldAccess { receiver, field, field_span, safe } => {
                let result_ty = match self.dotted_path_if_not_local(expr, scope) {
                    Some(path) => {
                        self.table.qualified_refs.push((expr.span, path));
                        Type::Unknown
                    }
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
                        // `class_field_type` unwraps `Nullable` itself, so
                        // a safe (`?.`) access on a nullable receiver still
                        // finds its field, wrapped `Nullable` again below.
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
                            if let Some(idx) = self.class_field_symbol_idx(&recv_ty, field) {
                                self.table.occurrences.push((*field_span, idx));
                            } else if let Some(cross_ref) = self.class_field_cross_file_ref(&recv_ty, field) {
                                self.cross_file_refs.insert(*field_span, cross_ref);
                            }
                            self.class_field_type(&recv_ty, field).map(|(ty, _)| ty).unwrap_or(Type::Unknown)
                        } else if recv_ty == Type::Unknown {
                            Type::Unknown
                        } else {
                            match self.class_field_type(&recv_ty, field) {
                                Some((ty, _)) => {
                                    if let Some(idx) = self.class_field_symbol_idx(&recv_ty, field) {
                                        self.table.occurrences.push((*field_span, idx));
                                    } else if let Some(cross_ref) = self.class_field_cross_file_ref(&recv_ty, field) {
                                        self.cross_file_refs.insert(*field_span, cross_ref);
                                    }
                                    ty
                                }
                                None => {
                                    self.type_errors.push(RuntimeError::new(
                                        format!("'{}' has no field '{field}'", recv_ty.name()),
                                        expr.span,
                                    ));
                                    Type::Unknown
                                }
                            }
                        }
                    }
                };
                // A safe (`?.`) access's own result is nullable whenever it
                // produced a real type — matches `MethodCall`'s identical
                // rule (`s?.length()` is `Int?`, not `Int`).
                if *safe && result_ty != Type::Unknown {
                    Type::Nullable(Box::new(result_ty))
                } else {
                    result_ty
                }
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
                // both call forms are the same function. More than one
                // overload (e.g. `kotlin.math.abs(-5)`) gets real overload
                // resolution instead, exactly like the bare-name `Call`
                // arm.
                let overloads: Vec<&'static crate::stdlib::BuiltinSymbol> = qualified_path
                    .as_ref()
                    .map(|path| crate::stdlib::stdlib_qualified_overloads(path))
                    .unwrap_or_default();
                // Not a stdlib package/member at all — try a real,
                // filesystem-backed user package next (`self.packages`),
                // with the exact same rigor: an unresolved package or
                // member is now a genuine diagnostic at the call's own
                // span, not silent `Type::Unknown` leniency — matching the
                // "no allowances anywhere" mandate for cross-file
                // resolution (see `resolve_with_imports`'s doc comment).
                let user_candidates: Vec<crate::package_index::PackageMember> = if !overloads.is_empty() {
                    Vec::new()
                } else {
                    match &qualified_path {
                        Some(path) => {
                            let (package_segments, member) = path.split_at(path.len() - 1);
                            let package = package_segments.join(".");
                            let member = &member[0];
                            if self.packages.has_package(&package) {
                                let found: Vec<_> = self
                                    .packages
                                    .members_of(&package)
                                    .iter()
                                    .filter(|m| m.symbol.name == *member)
                                    .cloned()
                                    .collect();
                                if found.is_empty() {
                                    self.diagnostics.push(RuntimeError::new(
                                        format!("undefined name '{}'", path.join(".")),
                                        *method_span,
                                    ));
                                }
                                found
                            } else {
                                self.diagnostics.push(RuntimeError::new(
                                    format!("unknown package '{package}'"),
                                    *method_span,
                                ));
                                Vec::new()
                            }
                        }
                        None => Vec::new(),
                    }
                };
                let use_overload_resolution = overloads.len() > 1 || user_candidates.len() > 1;
                // The receiver-type-dispatched case (a real value receiver,
                // not a package-qualified path — `recv_ty` is `Some(..)`)
                // — computed *before* `walk_call_args` runs, same as the
                // qualified-path/stdlib signature just below, so a class
                // method's named arguments get checked by name exactly
                // like a stdlib/cross-file call's already do (this phase's
                // own plan flags this ordering as risk #6: getting it
                // wrong silently drops named-argument checking for every
                // method call).
                let class_method_signature: Option<CallSignature> =
                    recv_ty.as_ref().and_then(|t| self.class_method_signature(t, method));
                // A real user-defined method (not `Any`'s native
                // fallback, which has no resolver-side `Symbol` to point
                // at — see `class_method_symbol_idx`'s doc comment) gets a
                // real `occurrences` entry at its own call-site span, the
                // same way any other declared symbol's use does.
                if let Some(idx) = recv_ty.as_ref().and_then(|t| self.class_method_symbol_idx(t, method)) {
                    self.table.occurrences.push((*method_span, idx));
                } else if let Some(cross_ref) =
                    recv_ty.as_ref().and_then(|t| self.class_method_cross_file_ref(t, method))
                {
                    self.cross_file_refs.insert(*method_span, cross_ref);
                }
                let signature = if use_overload_resolution {
                    None
                } else {
                    qualified_path
                        .as_ref()
                        .and_then(|path| {
                            crate::stdlib::stdlib_qualified_call_types(path).map(|(param_types, ret_ty)| {
                                let names = vec![String::new(); param_types.len()];
                                let docs = vec![None; param_types.len()];
                                let has_default = vec![false; param_types.len()];
                                (ret_ty, param_types, names, docs, has_default)
                            })
                        })
                        .or_else(|| {
                            user_candidates.first().map(|m| {
                                let ret_ty = if m.symbol.kind == SymbolKind::Class {
                                    Type::Class(Rc::from(m.symbol.name.as_str()))
                                } else {
                                    m.symbol.return_type.clone()
                                };
                                (
                                    ret_ty,
                                    m.symbol.param_types.clone(),
                                    m.symbol.param_names.clone(),
                                    m.symbol.param_docs.clone(),
                                    m.symbol.param_has_default.clone(),
                                )
                            })
                        })
                        .or_else(|| class_method_signature.clone())
                };
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
                            // case already returned above) — `class_
                            // method_signature` unwraps `Nullable` itself
                            // (`String?`'s methods are exactly `String`'s —
                            // matches Kotlin's own safe-call semantics),
                            // covering both a real builtin receiver and a
                            // user class (falling through to `Any`).
                            if class_method_signature.is_none() {
                                self.type_errors.push(RuntimeError::new(
                                    format!("'{}' has no method '{method}'", t.name()),
                                    *method_span,
                                ));
                            }
                            self.check_call_signature(&arg_infos, class_method_signature, *method_span)
                        }
                        // The qualified-path case (`kotlin.test.assertFalse(...)`,
                        // or a real user package's `a.b.c(...)`): `signature`/
                        // `overloads`/`user_candidates` is populated exactly
                        // when the path resolved to real member(s) above —
                        // this checks `arg_infos` against it (single
                        // overload) or resolves among several (multiple
                        // overloads) and returns the real return type. An
                        // unresolved path already got its own diagnostic
                        // above (unknown package / undefined name) — falls
                        // back to `Type::Unknown` here, same as any other
                        // already-reported problem.
                        None => {
                            let Some(path) = &qualified_path else { unreachable!() };
                            let name = path.last().unwrap();
                            if overloads.len() > 1 {
                                self.resolve_stdlib_overload(name, &overloads, &arg_infos, *method_span)
                            } else if user_candidates.len() > 1 {
                                self.resolve_cross_file_overload(name, &user_candidates, &arg_infos, *method_span)
                            } else {
                                let ty = self.check_call_signature(&arg_infos, signature, *method_span);
                                if let Some(m) = user_candidates.first() {
                                    self.cross_file_refs.insert(
                                        *method_span,
                                        CrossFileRef { file: m.file.clone(), def_span: m.symbol.def_span, symbol: m.symbol.clone() },
                                    );
                                }
                                ty
                            }
                        }
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
                    // `String`/`List` deliberately asymmetric here,
                    // matching `apply_binary`'s own runtime rules exactly
                    // (`interpreter.rs`'s `Add` arm): a `String` on the
                    // *left* concatenates anything (real Kotlin's
                    // `String.plus(other: Any?)`) — a `String` on the
                    // right with a non-`String` left has no such
                    // operator, at runtime or here. `List` only combines
                    // with another `List`, on *either* side, but never a
                    // non-`List` — `(List, List)` covers both directions
                    // by construction (no separate one-sided arm needed).
                    // A previous, symmetric `(_, Type::String) | (_,
                    // Type::List)` version of this let `2 + ""` and
                    // `5 + listOf(1, 2)` type-check with no error and only
                    // fail at runtime — a real static/runtime split found
                    // via direct testing, fixed here.
                    BinaryOp::Add => match (&l, &r) {
                        (Type::Int, Type::Int) => Type::Int,
                        (Type::Double, Type::Double) | (Type::Double, Type::Int) | (Type::Int, Type::Double) => {
                            Type::Double
                        }
                        (Type::String, _) => Type::String,
                        (Type::List, Type::List) => Type::List,
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

                // A bare name with more than one candidate — several local
                // `fun` overloads (real user-defined overloading — see
                // `ScopeData::names`'s doc comment), or, absent any local
                // symbol at all, more than one stdlib overload (`abs`/
                // `max`/`min`/`sqrt`) — gets real overload resolution below
                // instead of a single collapsed signature, decided up
                // front so no expected-type is propagated into a lambda
                // argument for it (which overload even applies isn't known
                // before the arguments are walked anyway). Local overloads
                // take priority over a same-named stdlib function, exactly
                // like the single-candidate path below already does via
                // `resolve_symbol` before `stdlib_call_types`.
                let local_overload_count = match &callee.kind {
                    ExprKind::Name(name) => {
                        let syms = self.resolve_symbols(scope, name);
                        if !syms.is_empty() && syms.iter().all(|s| s.kind == SymbolKind::Fun) {
                            syms.len()
                        } else {
                            0
                        }
                    }
                    _ => 0,
                };
                let use_local_overload_resolution = local_overload_count > 1;

                let overloads: Vec<&'static crate::stdlib::BuiltinSymbol> = if use_local_overload_resolution {
                    Vec::new()
                } else {
                    match &callee.kind {
                        ExprKind::Name(name) if self.resolve_symbol(scope, name).is_none() => {
                            crate::stdlib::stdlib_overloads(name)
                        }
                        _ => Vec::new(),
                    }
                };
                let use_stdlib_overload_resolution = overloads.len() > 1;

                // A cross-file candidate group — a name brought in by a
                // real `import`/`@file:Import` (see `CrossFileImports`) —
                // is only consulted once neither a local symbol nor a
                // stdlib function/overload set exists under this name,
                // same precedence `resolve_name` already uses (stdlib
                // default-imports always win a same-name collision).
                let cross_candidates: Vec<crate::package_index::PackageMember> =
                    if use_local_overload_resolution || !overloads.is_empty() {
                        Vec::new()
                    } else {
                        match &callee.kind {
                            ExprKind::Name(name) if self.resolve_symbol(scope, name).is_none() => {
                                self.imports.by_name.get(name).cloned().unwrap_or_default()
                            }
                            _ => Vec::new(),
                        }
                    };
                let use_cross_overload_resolution = cross_candidates.len() > 1;
                let use_overload_resolution =
                    use_local_overload_resolution || use_stdlib_overload_resolution || use_cross_overload_resolution;

                // A checkable signature comes from a local `fun` symbol, a
                // non-overloaded stdlib function, or a non-overloaded
                // cross-file import, in that order — a `BuiltinSymbol`'s/
                // `Symbol`'s own signature field is a free-text display
                // string, not this. Params with no real names (stdlib and
                // cross-file alike) use `String::new()` placeholders — any
                // named-arg call against one falls into the "unknown
                // parameter" case below, a minor, documented limitation.
                // Resolved *before* walking the arguments (unlike every
                // other subexpression here) so a lambda argument whose
                // matching parameter is itself a function type
                // (`apply({ x -> x * 2 }, 5)`) can infer its own parameter
                // types from that — see `walk_expr_expected`.
                let signature = if use_overload_resolution {
                    None
                } else {
                    match &callee.kind {
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
                                    Some((
                                        s.return_type.clone(),
                                        s.param_types.clone(),
                                        s.param_names.clone(),
                                        s.param_docs.clone(),
                                        s.param_has_default.clone(),
                                    ))
                                } else if s.kind == SymbolKind::Class {
                                    // Constructing an instance — `Point(3,
                                    // 4)` — not a bare reference to the
                                    // class itself (`s.ty`, unlike this
                                    // call's own result, stays
                                    // `Type::Unknown`; see `walk_class_decl`'s
                                    // doc comment on why). The primary
                                    // constructor's real parameter types/
                                    // names live in `class_info`, not on
                                    // the class's own `Symbol`.
                                    self.class_info.get(name.as_str()).map(|info| {
                                        (
                                            Type::Class(Rc::from(name.as_str())),
                                            info.ctor_param_types.clone(),
                                            info.ctor_param_names.clone(),
                                            vec![None; info.ctor_param_types.len()],
                                            info.ctor_param_has_default.clone(),
                                        )
                                    })
                                } else if let Type::Function(params, ret) = &s.ty {
                                    let names = vec![String::new(); params.len()];
                                    let docs = vec![None; params.len()];
                                    let has_default = vec![false; params.len()];
                                    Some(((**ret).clone(), params.clone(), names, docs, has_default))
                                } else {
                                    None
                                }
                            })
                            .or_else(|| {
                                // Implicit `this`: `name` isn't a local at
                                // all (the `and_then` above only reaches
                                // here when `resolve_symbol` found
                                // nothing), but the enclosing class (if
                                // any) has a method by that name — same
                                // precedence `resolve_name` already
                                // establishes (local shadows field/method,
                                // checked first; this is only ever a
                                // fallback). Gives `foo()` real named-
                                // argument checking against `this.foo`'s
                                // actual signature, not just a suppressed
                                // "undefined name".
                                self.current_class
                                    .clone()
                                    .and_then(|c| self.class_method_signature(&Type::Class(c), name))
                            })
                            .or_else(|| {
                                crate::stdlib::stdlib_call_types(name)
                                    .map(|(param_types, ret_ty)| {
                                        let names = vec![String::new(); param_types.len()];
                                        let docs = vec![None; param_types.len()];
                                        let has_default = vec![false; param_types.len()];
                                        (ret_ty, param_types, names, docs, has_default)
                                    })
                            })
                            .or_else(|| {
                                cross_candidates.first().map(|m| {
                                    let ret_ty = if m.symbol.kind == SymbolKind::Class {
                                        Type::Class(Rc::from(m.symbol.name.as_str()))
                                    } else {
                                        m.symbol.return_type.clone()
                                    };
                                    (
                                        ret_ty,
                                        m.symbol.param_types.clone(),
                                        m.symbol.param_names.clone(),
                                        m.symbol.param_docs.clone(),
                                        m.symbol.param_has_default.clone(),
                                    )
                                })
                            }),
                        _ => None,
                    }
                };

                let arg_infos = self.walk_call_args(args, signature.as_ref(), scope, following);

                if use_local_overload_resolution {
                    let ExprKind::Name(name) = &callee.kind else { unreachable!() };
                    // Cloned out of the `Symbol` table (not borrowed as
                    // `&Symbol`) so this doesn't hold a live borrow of
                    // `self.table` across the `&mut self` call below.
                    let candidates: Vec<(Vec<Type>, Type, Span)> = self
                        .resolve_symbols(scope, name)
                        .iter()
                        .map(|s| (s.param_types.clone(), s.return_type.clone(), s.def_span))
                        .collect();
                    return self.resolve_user_overload(name, &candidates, &arg_infos, expr.span);
                }
                if use_stdlib_overload_resolution {
                    let ExprKind::Name(name) = &callee.kind else { unreachable!() };
                    return self.resolve_stdlib_overload(name, &overloads, &arg_infos, expr.span);
                }
                if use_cross_overload_resolution {
                    let ExprKind::Name(name) = &callee.kind else { unreachable!() };
                    return self.resolve_cross_file_overload(name, &cross_candidates, &arg_infos, expr.span);
                }

                let ty = self.check_call_signature(&arg_infos, signature, expr.span);
                if let Some(m) = cross_candidates.first() {
                    self.cross_file_refs.insert(
                        expr.span,
                        CrossFileRef { file: m.file.clone(), def_span: m.symbol.def_span, symbol: m.symbol.clone() },
                    );
                }
                ty
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
                let then_ty = self.walk_block(then_branch, scope, expr.span, following, ScopeKind::Other);
                self.narrowed = ambient.clone();
                let else_ty = match else_branch {
                    Some(eb) => {
                        self.narrowed.extend(neg.iter().copied());
                        let ty = self.walk_block(eb, scope, expr.span, following, ScopeKind::Other);
                        self.narrowed = ambient.clone();
                        ty
                    }
                    None => {
                        self.table.dangling_ifs.push((expr.span, following));
                        Type::Unit
                    }
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
                    let arm_ty = self.walk_block(&arm.body, scope, expr.span, following, ScopeKind::Other);
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
            ExprKind::Block(stmts) => self.walk_block(stmts, scope, expr.span, following, ScopeKind::Other),
            ExprKind::NullLit => Type::Null,
            ExprKind::Elvis { lhs, rhs } => {
                let l = self.walk_expr(lhs, scope, following);
                let r = self.walk_expr(rhs, scope, following);
                // `lhs` is the bare literal `null` (always null, not just
                // possibly-null) — the elvis can never take the lhs
                // branch, so the whole expression's type is simply
                // `rhs`'s, with no join needed at all (real Kotlin: `null`
                // alone is `Nothing?`, and `Nothing?` joined with any `T`
                // is just `T`). Previously this fell through to the
                // `unwrapped_l == Type::Unknown` case below regardless of
                // `r`, so e.g. `null ?: "OK"` inferred `Unknown` instead
                // of `String` — a real bug (`fun box() = null ?: null ?:
                // "OK"` couldn't infer a return type at all), not a
                // deliberate simplification.
                if l == Type::Null {
                    r
                } else {
                    let unwrapped_l = match &l {
                        Type::Nullable(inner) => (**inner).clone(),
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
            // `this` is a real symbol, defined once per constructor/`init`
            // scope and once per method's own body scope (see
            // `walk_class_decl`/`walk_class_method`) — ordinary name
            // resolution handles it exactly like any other local. The
            // parser already rejects `this` anywhere outside a class
            // member (`class_member_depth`), so reaching here with no
            // `this` in scope is not a real user-facing case.
            ExprKind::This => self.resolve_name(scope, "this", expr.span),
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

    /// A `PackageIndex` seeded with the stdlib plus one fake user package,
    /// `util.math` (a real `fun square(x: Int): Int`, no backing file) —
    /// lets a test exercise real, precise cross-file import resolution
    /// without spinning up a temp-file harness (see `importer.rs`'s own
    /// `TempDir` tests for real multi-file coverage). Matches the
    /// `util.math.square` shape already used throughout this file's own
    /// examples/comments as "some real function this resolver can't see
    /// declared locally".
    fn package_index_with_util_math() -> crate::package_index::PackageIndex {
        let mut packages = crate::package_index::PackageIndex::new();
        packages.add_member(
            "util.math".to_string(),
            Symbol {
                name: "square".to_string(),
                kind: SymbolKind::Fun,
                def_span: Span::default(),
                mutable: false,
                signature: "fun square(x: Int): Int".to_string(),
                ty: Type::Function(vec![Type::Int], Box::new(Type::Int)),
                return_type: Type::Int,
                param_types: vec![Type::Int],
                param_names: vec!["x".to_string()],
                param_docs: Vec::new(),
                param_has_default: vec![false],
                doc: None,
            },
            None,
            None,
        );
        packages
    }

    fn resolve_src_with_util_math(src: &str) -> ResolveResult {
        let tokens = lexer::tokenize(src).unwrap();
        let program = parser::parse(&tokens).unwrap();
        resolve_with_imports(&program, &package_index_with_util_math())
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
        // exists via `import util.math.square`, resolved for real against a
        // `PackageIndex` that genuinely has it — so it must NOT be flagged,
        // unlike a real typo.
        let result = resolve_src_with_util_math("import util.math.square\nprintln(square(5))\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn bare_file_import_no_longer_blanket_suppresses_undefined_names() {
        // `@file:Import` used to dump an unknown set of names into scope,
        // suppressing undefined-name checking for the *whole file* — real,
        // precise resolution replaced that (see
        // `resolve_with_imports`/`CrossFileImports`): with no real
        // `PackageIndex` backing `greeter.ksl` (this test has no
        // filesystem access at all — see `importer.rs`'s own `TempDir`
        // tests for the real multi-file, genuinely-resolving case), `greet`
        // is now correctly flagged, exactly like any other undefined name.
        let result = resolve_src("@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n");
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("greet"));
    }

    #[test]
    fn import_alone_does_not_suppress_other_genuine_typos() {
        // `import`ing a real, resolvable name doesn't affect any other
        // name's own checking — an unrelated typo elsewhere is still
        // flagged, exactly as if `square` had been declared locally.
        let result = resolve_src_with_util_math("import util.math.square\nprintln(totallyNotAThing)\n");
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("totallyNotAThing"));
    }

    #[test]
    fn wildcard_import_of_an_unknown_package_is_a_diagnostic() {
        let result = resolve_src("import a.b.*\nprintln(1)\n");
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("unknown package 'a.b'"), "{}", result.diagnostics[0].message);
    }

    #[test]
    fn named_import_of_a_nonexistent_member_in_a_real_package_is_a_diagnostic() {
        let result = resolve_src_with_util_math("import util.math.cube\nprintln(1)\n");
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("undefined name 'util.math.cube'"), "{}", result.diagnostics[0].message);
    }

    #[test]
    fn wildcard_import_of_a_real_package_brings_in_every_member_precisely() {
        let mut packages = package_index_with_util_math();
        packages.add_member(
            "util.math".to_string(),
            Symbol {
                name: "cube".to_string(),
                kind: SymbolKind::Fun,
                def_span: Span::default(),
                mutable: false,
                signature: "fun cube(x: Int): Int".to_string(),
                ty: Type::Function(vec![Type::Int], Box::new(Type::Int)),
                return_type: Type::Int,
                param_types: vec![Type::Int],
                param_names: vec!["x".to_string()],
                param_docs: Vec::new(),
                param_has_default: vec![false],
                doc: None,
            },
            None,
            None,
        );
        let tokens = lexer::tokenize("import util.math.*\nval a = square(2)\nval b = cube(2)\nprintln(nope)\n").unwrap();
        let program = parser::parse(&tokens).unwrap();
        let result = resolve_with_imports(&program, &packages);
        // Every real member is visible unqualified and type-checks...
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        // ...but a genuine typo unrelated to the wildcard import is still
        // flagged, exactly as strictly as a local declaration would be.
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("nope"));
    }

    #[test]
    fn wildcard_imported_call_with_a_mismatched_argument_type_is_flagged() {
        let result = resolve_src_with_util_math("import util.math.*\nsquare(\"hi\")\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("expected Int"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn cross_file_import_records_an_exact_reference_to_the_resolved_symbol() {
        // The bare callee name itself (`resolve_name`, via `walk_expr`) and
        // the call expression as a whole (the `Call` arm's own single-
        // candidate bookkeeping) each record their own entry — both point
        // at the same real symbol, just at different use-site spans (the
        // name's own span vs. the whole call's), matching how a local call
        // similarly touches both `table.occurrences` and, for a real
        // overload, `resolved_calls`.
        let tokens = lexer::tokenize("import util.math.square\nsquare(1)\n").unwrap();
        let program = parser::parse(&tokens).unwrap();
        let result = resolve_with_imports(&program, &package_index_with_util_math());
        assert!(!result.cross_file_refs.is_empty(), "{:?}", result.cross_file_refs);
        for found in result.cross_file_refs.values() {
            assert!(found.file.is_none()); // the fake util.math package has no backing file
            assert_eq!(found.def_span, Span::default());
        }
    }

    #[test]
    fn qualified_call_into_an_unresolved_user_package_is_now_a_diagnostic() {
        // Full strictness for a qualified *call*: an unresolved package is
        // a real diagnostic now, not silent `Type::Unknown` leniency.
        let result = resolve_src("util.math.square(5)\n");
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("unknown package 'util.math'"), "{}", result.diagnostics[0].message);
    }

    #[test]
    fn qualified_call_into_a_real_user_package_gets_real_argument_checking() {
        let tokens = lexer::tokenize("util.math.square(\"hi\")\n").unwrap();
        let program = parser::parse(&tokens).unwrap();
        let result = resolve_with_imports(&program, &package_index_with_util_math());
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("expected Int"), "{}", result.type_errors[0].message);
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
    fn type_ending_before_finds_a_constructor_calls_own_type() {
        // "val o = P(1)" is 12 characters — the widened `Call` span
        // (`parser.rs`'s `parse_postfix`) ends right after its own `)`,
        // at column 13 (1-based, one-past-the-last-character).
        let result = resolve_src("class P(val x: Int)\nval o = P(1)\n");
        assert_eq!(result.table.type_ending_before(2, 14), Some(&Type::Class(Rc::from("P"))));
        // Querying right at the `Call`'s own boundary (its `end_col` is
        // 13, so `< 13` excludes it) still correctly finds the *inner*
        // `1` argument's own type (`Int`, ending at column 12) — proof
        // `max_by_key` genuinely prefers whichever recorded expression is
        // closest to the query position, not just the outermost one
        // unconditionally.
        assert_eq!(result.table.type_ending_before(2, 13), Some(&Type::Int));
    }

    #[test]
    fn type_ending_before_finds_a_bare_names_own_type() {
        // "println(x)" — "println(" is 8 characters, "x" spans columns
        // 9..10 (end_col 10, one-past).
        let result = resolve_src("val x = 5\nprintln(x)\n");
        assert_eq!(result.table.type_ending_before(2, 11), Some(&Type::Int));
    }

    #[test]
    fn type_ending_before_is_none_with_nothing_recorded_there() {
        let result = resolve_src("val x = 5\nprintln(x)\n");
        assert_eq!(result.table.type_ending_before(2, 1), None);
        assert_eq!(result.table.type_ending_before(99, 1), None);
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
        let src = "fun a() {\n    val onlyInA = 1\n}\n\nfun b() {\n    val onlyInB = 1\n}\n";
        let result = resolve_src(src);
        let names: HashSet<&str> =
            result.table.symbols_in_scope(6, 5).into_iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains("onlyInB"), "{names:?}");
        assert!(!names.contains("onlyInA"), "{names:?}");
    }

    #[test]
    fn symbols_in_scope_does_not_leak_into_the_blank_gap_after_its_own_closing_brace() {
        // Regression test: `open_scope` used to bound a scope by "up to
        // wherever the next sibling begins," which swallowed the entire
        // gap after a construct's own closing `}` (blank lines, trailing
        // whitespace) into that construct's own scope — invisible until a
        // cursor actually sits in that gap (e.g. typing a brand new
        // statement there) and asks what's in scope. Fixed by widening
        // every block-bodied construct's own span to its real closing `}`
        // (see `parser::Parser::parse_stmt`) and having `open_scope` prefer
        // that over the next sibling's start whenever it's tighter.
        let src = "fun test(coolParam: String) {}\n\ncoo\n";
        let result = resolve_src(src);
        // Line 2 is the blank gap between `test`'s closing `}` and `coo`.
        let on_gap_line: HashSet<&str> =
            result.table.symbols_in_scope(2, 1).into_iter().map(|s| s.name.as_str()).collect();
        assert!(!on_gap_line.contains("coolParam"), "{on_gap_line:?}");
        // Line 3 (`coo`, mid-typing a new statement) must not see it either.
        let on_next_stmt: HashSet<&str> =
            result.table.symbols_in_scope(3, 4).into_iter().map(|s| s.name.as_str()).collect();
        assert!(!on_next_stmt.contains("coolParam"), "{on_next_stmt:?}");
        // Sanity check: `coolParam` is still visible from genuinely inside
        // `test`'s own body.
        let src2 = "fun test(coolParam: String) {\n    println(coolParam)\n}\n";
        let result2 = resolve_src(src2);
        let inside_body: HashSet<&str> =
            result2.table.symbols_in_scope(2, 13).into_iter().map(|s| s.name.as_str()).collect();
        assert!(inside_body.contains("coolParam"), "{inside_body:?}");
    }

    #[test]
    fn inside_a_loop_at_finds_a_while_and_a_for_body() {
        let result = resolve_src("while (true) {\n    println(1)\n}\n");
        assert!(result.table.inside_a_loop_at(2, 5));

        let result2 = resolve_src("for (i in 1..3) {\n    println(i)\n}\n");
        assert!(result2.table.inside_a_loop_at(2, 5));
    }

    #[test]
    fn inside_a_loop_at_stays_false_outside_any_loop() {
        let result = resolve_src("println(1)\n");
        assert!(!result.table.inside_a_loop_at(1, 5));
    }

    #[test]
    fn inside_a_loop_at_stays_false_past_the_loops_own_closing_brace() {
        // The blank gap right after the loop's own closing `}`, and
        // anything genuinely past it, must not count as still "inside" it
        // -- same real-closing-extent fix `symbols_in_scope_does_not_leak_
        // into_the_blank_gap_after_its_own_closing_brace` covers.
        let result = resolve_src("while (true) {\n    println(1)\n}\n\nprintln(2)\n");
        assert!(!result.table.inside_a_loop_at(4, 1), "the blank gap after the loop");
        assert!(!result.table.inside_a_loop_at(5, 1), "well past the loop");
    }

    #[test]
    fn inside_a_loop_at_is_false_inside_a_nested_function_or_lambda() {
        // Matches `parser::Parser::loop_depth`'s own reset at a fun/lambda
        // boundary -- a loop enclosing the function/lambda from the
        // *outside* doesn't make break/continue inside it legal.
        let result =
            resolve_src("while (true) {\n    fun f() {\n        println(1)\n    }\n}\n");
        assert!(!result.table.inside_a_loop_at(3, 9), "{:?}", "inside the nested fun");

        let result2 = resolve_src("while (true) {\n    val f = {\n        1\n    }\n}\n");
        assert!(!result2.table.inside_a_loop_at(3, 9), "inside the nested lambda");
    }

    #[test]
    fn inside_a_loop_at_is_true_inside_an_if_nested_in_a_loop() {
        // An `if`/`when` branch isn't itself loop-relevant, but doesn't
        // reset loop context either -- keep searching past it.
        let result = resolve_src("while (true) {\n    if (true) {\n        break\n    }\n}\n");
        assert!(result.table.inside_a_loop_at(3, 9));
    }

    #[test]
    fn inside_a_loop_at_is_true_inside_a_do_whiles_own_body() {
        let result = resolve_src("do {\n    break\n} while (true)\n");
        assert!(result.table.inside_a_loop_at(2, 5));
    }

    #[test]
    fn do_whiles_condition_resolves_a_name_declared_in_its_own_body() {
        // Real Kotlin/Java semantics: `cond` is resolved in the body's own
        // scope, not the enclosing one, since the body always runs before
        // `cond` is ever checked -- must NOT be an "undefined name"
        // diagnostic. See `interpreter.rs`'s matching runtime test.
        let result = resolve_src("do {\n    val x = 1\n} while (x > 0)\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn do_whiles_body_local_does_not_leak_past_the_loop() {
        let result = resolve_src("do {\n    val x = 1\n} while (true)\nprintln(x)\n");
        assert!(!result.diagnostics.is_empty(), "'x' must not be visible after the loop");
    }

    #[test]
    fn else_allowed_at_is_true_right_after_a_dangling_ifs_own_close() {
        let result = resolve_src("if (true) {\n    1\n}\n");
        // "}" on line 3 is a single char -- its own span ends at column 2.
        assert!(result.table.else_allowed_at(3, 2), "right after the closing brace");
        assert!(result.table.else_allowed_at(3, 3), "a bit further into the gap, still fine");
    }

    #[test]
    fn else_allowed_at_is_false_after_an_if_that_already_has_an_else() {
        let result = resolve_src("if (true) {\n    1\n} else {\n    2\n}\n");
        for line in 1..6 {
            for col in 1..10 {
                assert!(!result.table.else_allowed_at(line, col), "line={line} col={col}");
            }
        }
    }

    #[test]
    fn else_allowed_at_is_false_once_a_real_next_statement_has_started() {
        let result = resolve_src("if (true) {\n    1\n}\nprintln(2)\n");
        assert!(result.table.else_allowed_at(3, 2), "still in the gap");
        // "println(2)" starts at (4, 1) -- once a real next statement's own
        // span begins, the gap is over.
        assert!(!result.table.else_allowed_at(4, 1), "println's own start");
    }

    #[test]
    fn else_allowed_at_is_false_after_an_unrelated_blocks_close() {
        let result = resolve_src("while (true) {\n    1\n}\n");
        for line in 1..4 {
            for col in 1..10 {
                assert!(!result.table.else_allowed_at(line, col), "line={line} col={col}");
            }
        }
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
    fn missing_required_argument_is_flagged_statically() {
        let result = resolve_src("fun greet(name: String): String {\n    return name\n}\nprintln(greet())\n");
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("missing argument 'name'")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn too_many_positional_arguments_is_flagged_statically() {
        let result = resolve_src("fun greet(name: String): String {\n    return name\n}\nprintln(greet(\"a\", \"b\"))\n");
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("too many arguments: expected at most 1, got 2")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn a_default_parameter_left_out_is_not_flagged() {
        // Proves the default actually suppresses the new arity check, not
        // just that this particular call happens to slip through some
        // other way.
        let result = resolve_src("fun greet(name: String = \"World\"): String {\n    return name\n}\nprintln(greet())\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn class_constructor_missing_a_required_argument_is_flagged_statically() {
        let result = resolve_src("class Point(val x: Int, val y: Int)\nval p = Point(1)\n");
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("missing argument 'y'")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn class_method_missing_a_required_argument_is_flagged_statically() {
        // Exact shape of the user-reported bug: a cross-file-class-shaped
        // method call missing its one required argument used to run all
        // the way to a runtime "missing argument" error instead of being
        // caught here, statically, before execution.
        let result = resolve_src(
            "class Point(val x: Int, val y: Int) {\n    fun distanceSquared(other: Point): Int {\n        val dx = x - other.x\n        val dy = y - other.y\n        return dx * dx + dy * dy\n    }\n}\nval p = Point(3, 4)\np.distanceSquared()\n",
        );
        assert!(
            result.type_errors.iter().any(|e| e.message.contains("missing argument 'other'")),
            "{:?}",
            result.type_errors
        );
    }

    #[test]
    fn stdlib_call_with_fewer_args_than_its_full_parameter_list_is_never_flagged() {
        // `assertEquals(actual, expected, message = ...)` — stdlib
        // signatures always use blank-name placeholders (`stdlib_call_
        // types` has no real parameter names to offer), so the new arity
        // check must never fire for one, regardless of how many optional
        // trailing parameters it actually has.
        let result = resolve_src("import kotlin.test.*\nassertEquals(1, 1)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn unknown_typed_argument_is_never_flagged() {
        // An unresolved qualified call (`util.math` doesn't exist in this
        // zero-context `PackageIndex`) is its own genuine diagnostic now —
        // full strictness for qualified calls — but its *type* is still
        // `Type::Unknown`, so it must never additionally be flagged as a
        // type mismatch, matching the "Unknown is always compatible" rule.
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(1, util.math.square(2))\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("unknown package 'util.math'"), "{}", result.diagnostics[0].message);
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

    // ---- real stdlib overload resolution (replaces the old
    // is_numeric_polymorphic special case) ----

    #[test]
    fn abs_and_max_still_infer_the_real_argument_type() {
        // The behavior the old numeric-only special case got right —
        // must still work exactly the same via real overload resolution.
        let result = resolve_src("import kotlin.math.*\nval x: Double = abs(-5)\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("expected Double, got Int"), "{}", result.type_errors[0].message);

        let result = resolve_src("import kotlin.math.*\nval x: Double = abs(-5.5)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);

        let result = resolve_src("import kotlin.math.*\nval x: Int = max(3, 7)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn sqrt_with_an_unknown_typed_argument_still_infers_double() {
        // Ambiguous overload (Unknown could be either Int or Double), but
        // both of sqrt's overloads agree on Double regardless — must
        // still resolve, not fall back to Unknown.
        let src = "import kotlin.math.*\nimport util.math.square\nval x: Int = sqrt(square(2))\n";
        let result = resolve_src(src);
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("expected Int, got Double"), "{}", result.type_errors[0].message);
    }

    #[test]
    fn abs_with_a_non_numeric_argument_is_a_real_type_error() {
        // The gap the old numeric-only special case had: a String didn't
        // pass `is_numeric`'s filter, so the whole call silently became
        // Unknown — no error at all. Real overload resolution must catch
        // this the same way any other type mismatch is caught.
        let result = resolve_src("import kotlin.math.*\nabs(\"hello\")\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("no overload of 'abs' matches argument types (String)"),
            "{}",
            result.type_errors[0].message
        );
    }

    #[test]
    fn abs_with_the_wrong_arity_is_a_real_type_error() {
        let result = resolve_src("import kotlin.math.*\nabs(1, 2)\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("no overload of 'abs' accepts 2 argument(s)"),
            "{}",
            result.type_errors[0].message
        );
    }

    #[test]
    fn qualified_overloaded_stdlib_call_now_gets_real_resolution() {
        // A genuinely new capability: `kotlin.math.abs(...)` used to be
        // entirely unchecked (Type::Unknown, no dispatch at all) — now it
        // resolves exactly like the bare, imported form.
        let result = resolve_src("val x: Double = kotlin.math.abs(-5)\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(result.type_errors[0].message.contains("expected Double, got Int"), "{}", result.type_errors[0].message);

        let result = resolve_src("kotlin.math.abs(\"hello\")\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("no overload of 'abs' matches argument types (String)"),
            "{}",
            result.type_errors[0].message
        );
    }

    // ---- real user-defined function overloading ----

    #[test]
    fn distinguishable_fun_overloads_are_accepted_with_no_diagnostic() {
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: String): String = \"string\"\n";
        let result = resolve_src(src);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn identical_signature_fun_overload_is_a_conflicting_overload_diagnostic() {
        let src = "fun show(x: Int): String = \"a\"\nfun show(x: Int): String = \"b\"\n";
        let result = resolve_src(src);
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("conflicting overload of 'show'"), "{}", result.diagnostics[0].message);
    }

    #[test]
    fn a_fun_overload_and_a_val_with_the_same_name_is_still_already_declared() {
        // Real overloading is only ever legal between several `Fun`
        // symbols — a `Fun`/`Val` clash is the original, unconditional
        // "already declared" case, unchanged.
        let src = "val show = 1\nfun show(x: Int): String = \"a\"\n";
        let result = resolve_src(src);
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert!(result.diagnostics[0].message.contains("'show' is already declared in this scope"), "{}", result.diagnostics[0].message);
    }

    #[test]
    fn call_to_an_overloaded_fun_resolves_the_right_return_type() {
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: Boolean): Int = 1\nval a: String = show(5)\nval b: Int = show(true)\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn call_to_an_overloaded_fun_with_no_matching_signature_is_flagged() {
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: Boolean): Int = 1\nshow(\"nope\")\n";
        let result = resolve_src(src);
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("no overload of 'show' matches argument types (String)"),
            "{}",
            result.type_errors[0].message
        );
    }

    #[test]
    fn call_to_an_overloaded_fun_records_the_resolved_target_span() {
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: Boolean): Int = 1\nshow(5)\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(result.resolved_calls.len(), 1, "{:?}", result.resolved_calls);
        let (call_span, target_span) = result.resolved_calls.iter().next().unwrap();
        assert_eq!(call_span.line, 3);
        // The `Int` overload is declared on line 1.
        assert_eq!(target_span.line, 1);
    }

    #[test]
    fn named_argument_matching_type_is_not_flagged() {
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(a = 1, b = 2)\n";
        let result = resolve_src(src);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn named_arg_label_at_hovers_the_calls_matching_parameter() {
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(a = 1, b = 2)\n";
        let result = resolve_src(src);
        // "add(a = 1, b = 2)" -- "a" starts at column 5 on line 4.
        let (span, sym) = result.table.named_arg_label_at(4, 5).expect("a should hover");
        assert_eq!(sym.name, "a");
        assert_eq!(sym.signature, "param a: Int");
        assert_eq!(span.line, 4);
        // "b" starts at column 12 on the same line.
        let (_, sym_b) = result.table.named_arg_label_at(4, 12).expect("b should hover");
        assert_eq!(sym_b.name, "b");
    }

    #[test]
    fn named_arg_label_at_shows_the_calls_param_doc() {
        // Doc comments are only attached to declarations by
        // `parser::parse_with_docs` (see `crate::parse_only`) — plain
        // `parser::parse` (what `resolve_src` above uses) never attaches
        // one, so this test builds its own `Program` instead.
        let src = "/**\n * Adds two numbers.\n * @param a the first number\n */\nfun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(a = 1, b = 2)\n";
        let program = crate::parse_only(src).expect("should parse");
        let result = resolve(&program);
        // "add(a = 1, b = 2)" is on line 8, "a" starts at column 5.
        let (_, sym) = result.table.named_arg_label_at(8, 5).expect("a should hover");
        assert_eq!(sym.doc.as_deref(), Some("the first number"));
    }

    #[test]
    fn unknown_named_argument_is_flagged() {
        // `c` is unknown *and* `b` never got a real argument (the unknown
        // named one doesn't count towards it) — both diagnosed, not just
        // the first one found.
        let src = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nadd(a = 1, c = 2)\n";
        let result = resolve_src(src);
        assert_eq!(result.type_errors.len(), 2, "{:?}", result.type_errors);
        assert!(result.type_errors.iter().any(|e| e.message.contains("unknown parameter 'c'")), "{:?}", result.type_errors);
        assert!(result.type_errors.iter().any(|e| e.message.contains("missing argument 'b'")), "{:?}", result.type_errors);
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

    // ---- `+`'s String/List asymmetry (matches apply_binary's own
    // runtime rules exactly — regression tests for a real static/runtime
    // split: these two used to type-check with no error and only fail at
    // runtime) ----

    #[test]
    fn string_plus_anything_infers_string_string_must_be_on_the_left() {
        let result = resolve_src("println(\"\" + 2)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn int_plus_string_is_a_type_error_not_silently_string() {
        let result = resolve_src("println(2 + \"\")\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("unsupported operand types for '+': Int and String"),
            "{}",
            result.type_errors[0].message
        );
    }

    #[test]
    fn list_plus_list_infers_list() {
        let result = resolve_src("println(listOf(1) + listOf(2))\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn int_plus_list_is_a_type_error_not_silently_list() {
        let result = resolve_src("println(5 + listOf(1, 2))\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("unsupported operand types for '+': Int and List"),
            "{}",
            result.type_errors[0].message
        );
    }

    #[test]
    fn list_plus_int_is_also_a_type_error() {
        let result = resolve_src("println(listOf(1) + 2)\n");
        assert_eq!(result.type_errors.len(), 1, "{:?}", result.type_errors);
        assert!(
            result.type_errors[0].message.contains("unsupported operand types for '+': List and Int"),
            "{}",
            result.type_errors[0].message
        );
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

    /// Regression test: a bare-literal-`null` lhs used to make the whole
    /// elvis infer `Unknown` regardless of `rhs`'s real type — since a
    /// literal `null` lhs can *only* ever take the rhs branch, the correct
    /// type is simply `rhs`'s, no join needed. This specific chain
    /// (`fun box() = null ?: null ?: "OK"`) is a vendored Kotlin box test
    /// (`kotlin-box-pending/elvis/nullNullOk.ksl`) that couldn't infer a
    /// return type at all before this fix.
    #[test]
    fn elvis_with_a_literal_null_lhs_infers_the_rhs_type_not_unknown() {
        let result = resolve_src("fun box() = null ?: null ?: \"OK\"\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert_eq!(result.table.symbol_at(1, 5).unwrap().return_type, Type::String);
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

    #[test]
    fn constructing_a_class_infers_its_instance_type() {
        let result = resolve_src("class Point(val x: Int, val y: Int)\nval p = Point(1, 2)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        let (_, symbol) = result.table.occurrence_at(2, 5).expect("p should resolve");
        assert_eq!(symbol.ty, Type::Class(Rc::from("Point")));
    }

    #[test]
    fn constructor_named_arguments_are_checked_by_name() {
        let result = resolve_src("class Point(val x: Int, val y: Int)\nval p = Point(y = 2, x = 1)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);

        let bad = resolve_src("class Point(val x: Int, val y: Int)\nval p = Point(z = 2, x = 1)\n");
        assert!(bad.type_errors.iter().any(|e| e.message.contains("unknown parameter 'z'")), "{:?}", bad.type_errors);
    }

    #[test]
    fn constructor_argument_type_mismatch_is_flagged() {
        let result = resolve_src("class Point(val x: Int)\nval p = Point(\"nope\")\n");
        assert!(result.type_errors.iter().any(|e| e.message.contains("argument 1")), "{:?}", result.type_errors);
    }

    #[test]
    fn field_access_and_assignment_are_checked() {
        let ok = resolve_src("class Point(val x: Int, var y: Int)\nval p = Point(1, 2)\nfun f() {\n    p.y = 3\n}\n");
        assert!(ok.type_errors.is_empty(), "{:?}", ok.type_errors);

        let unknown_field = resolve_src("class Point(val x: Int)\nval p = Point(1)\nprintln(p.z)\n");
        assert!(unknown_field.type_errors.iter().any(|e| e.message.contains("'Point' has no field 'z'")), "{:?}", unknown_field.type_errors);

        let reassign_val = resolve_src("class Point(val x: Int)\nval p = Point(1)\nfun f() {\n    p.x = 2\n}\n");
        assert!(reassign_val.type_errors.iter().any(|e| e.message.contains("cannot reassign val 'x'")), "{:?}", reassign_val.type_errors);

        let wrong_type = resolve_src("class Point(var x: Int)\nval p = Point(1)\nfun f() {\n    p.x = \"nope\"\n}\n");
        assert!(wrong_type.type_errors.iter().any(|e| e.message.contains("cannot assign")), "{:?}", wrong_type.type_errors);
    }

    #[test]
    fn user_defined_method_calls_are_checked_including_named_arguments() {
        let result = resolve_src(
            "class Greeter(val name: String) {\n    fun greet(prefix: String): String = prefix + this.name\n}\nval g = Greeter(\"World\")\nprintln(g.greet(prefix = \"Hi \"))\n",
        );
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);

        let unknown_method = resolve_src("class Point(val x: Int)\nval p = Point(1)\np.bark()\n");
        assert!(unknown_method.type_errors.iter().any(|e| e.message.contains("'Point' has no method 'bark'")), "{:?}", unknown_method.type_errors);
    }

    #[test]
    fn class_members_reports_a_methods_real_inferred_return_type_not_unknown() {
        // Regression test: `walk_class_method` used to patch the inferred
        // return type onto `class_info`'s own private copy and the
        // display `signature` string, but not the method's own `Symbol`
        // (the one `SymbolTable::class_members` — and, via it,
        // `PackageIndex`'s cross-file class shapes — actually hands out),
        // leaving it stuck at `Type::Unknown`.
        let result = resolve_src("class C {\n    fun bark() = \"woof\"\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        let bark = result.table.class_members("C").find(|s| s.name == "bark").expect("bark should be a class member");
        assert_eq!(bark.return_type, Type::String, "{:?}", bark.return_type);
    }

    #[test]
    fn a_class_with_no_to_string_override_still_resolves_the_inherited_any_method() {
        // No `toString`/`equals`/`hashCode` declared on `Point` at all —
        // every user class implicitly extends the real builtin `Any` (see
        // `ClassInfo`'s doc comment), so these must still resolve, with
        // `Any`'s own real return types, not be flagged as unknown methods.
        let result = resolve_src(
            "class Point(val x: Int)\nval p = Point(1)\nval s: String = p.toString()\nval h: Int = p.hashCode()\nval b: Boolean = p.equals(p)\n",
        );
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn this_resolves_to_the_enclosing_classs_instance_type() {
        let result = resolve_src("class Point(val x: Int) {\n    fun getX(): Int = this.x\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn implicit_this_a_method_can_bare_reference_a_promoted_field() {
        let result = resolve_src("class Point(val x: Int) {\n    fun getX(): Int = x\n}\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn implicit_this_a_method_can_bare_reference_a_body_property() {
        let result = resolve_src("class C {\n    val a: Int = 5\n    fun f(): Int = a\n}\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn implicit_this_a_method_can_bare_call_another_method_with_named_arguments_checked() {
        let result = resolve_src("class C {\n    fun foo(step: Int): Int = step\n    fun bar(): Int = foo(step = 1)\n}\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);

        let bad = resolve_src("class C {\n    fun foo(step: Int): Int = step\n    fun bar(): Int = foo(nope = 1)\n}\n");
        assert!(bad.type_errors.iter().any(|e| e.message.contains("unknown parameter 'nope'")), "{:?}", bad.type_errors);
    }

    #[test]
    fn implicit_this_a_property_can_bare_reference_another_property() {
        let result = resolve_src("class C {\n    val a: Int = 5\n    val b: Int = a + 1\n}\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn implicit_this_bare_assignment_to_a_var_field_is_checked() {
        let ok = resolve_src("class C {\n    var count: Int = 0\n    fun bump() {\n        count = count + 1\n    }\n}\n");
        assert!(ok.diagnostics.is_empty(), "{:?}", ok.diagnostics);
        assert!(ok.type_errors.is_empty(), "{:?}", ok.type_errors);

        let reassign_val = resolve_src("class C {\n    val a: Int = 5\n    fun f() {\n        a = 6\n    }\n}\n");
        assert!(reassign_val.type_errors.iter().any(|e| e.message.contains("cannot reassign val 'a'")), "{:?}", reassign_val.type_errors);

        let wrong_type = resolve_src("class C {\n    var a: Int = 5\n    fun f() {\n        a = \"nope\"\n    }\n}\n");
        assert!(wrong_type.type_errors.iter().any(|e| e.message.contains("cannot assign")), "{:?}", wrong_type.type_errors);
    }

    #[test]
    fn implicit_this_bare_inc_dec_on_a_var_field_is_accepted() {
        let result = resolve_src("class C {\n    var count: Int = 0\n    fun bump() {\n        count++\n    }\n}\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn implicit_this_a_method_can_bare_call_the_inherited_any_methods() {
        let result = resolve_src("class C {\n    fun describe(): String = toString()\n}\n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn implicit_this_a_local_of_the_same_name_shadows_the_field() {
        // Real Kotlin's own rule: a local variable/parameter always wins
        // over a same-named field — reaching it would need explicit
        // `this.x`. Here the local `x` (a `String` param) shadows the
        // `Int` field, so returning it as `Int` is a real type error —
        // proof the *local*, not the field, was what actually resolved.
        let result = resolve_src("class Point(val x: Int) {\n    fun f(x: String): Int = x\n}\n");
        assert!(result.type_errors.iter().any(|e| e.message.contains("return type mismatch")), "{:?}", result.type_errors);
    }

    #[test]
    fn constructor_parameters_are_bare_visible_in_the_constructors_own_scope_too() {
        // A ctor-only (non-promoted) parameter has no field at all — it's
        // only ever a real, bare binding *inside the constructor's own
        // scope* (property initializers, `init` blocks), matching
        // `construct_object`'s real `ctor_env` at runtime; a promoted one
        // is bare-visible there *and* (via implicit `this`) everywhere
        // else in the class.
        let result = resolve_src("class Greeting(prefix: String, val name: String) {\n    val text: String = prefix + name\n}\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn a_class_referenced_before_its_own_declaration_still_resolves() {
        // `known_classes`'s real predpass (top-level names, before
        // `walk_stmts` runs) — a function's own type annotation can name a
        // class declared later in the same file, resolving to a real
        // `Type::Class` rather than an "unknown type" error. A *member*
        // access through such a not-yet-walked class (e.g. calling a
        // method on `p` inside `describe`'s own body) is a real, accepted
        // limitation instead — `class_info` isn't populated until
        // `Point`'s own `ClassDecl` is walked, textually later in this
        // same file (see `Resolver::class_info`'s doc comment) — so this
        // only exercises the annotation itself, not a member access.
        let result = resolve_src("fun describe(p: Point) {}\nclass Point(val x: Int)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn one_methods_forward_reference_to_another_resolves() {
        // `walk_class_decl`'s own two-phase design: every method's
        // signature is known before any method body is walked, so `bar`
        // can call `foo` even though `foo` is declared later in the same
        // class.
        let result = resolve_src(
            "class C {\n    fun bar(): Int = this.foo()\n    fun foo(): Int = 1\n}\n",
        );
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }

    #[test]
    fn property_initializer_type_mismatch_is_flagged() {
        let result = resolve_src("class C {\n    val x: Int = \"nope\"\n}\n");
        assert!(result.type_errors.iter().any(|e| e.message.contains("property 'x'")), "{:?}", result.type_errors);
    }

    #[test]
    fn init_as_a_context_keyword_does_not_break_ordinary_use() {
        // `init` is only a keyword in class-member position, immediately
        // before `{` — a plain `val`/`fun` named `init` elsewhere is
        // unaffected (parser-level guarantee; this just confirms the
        // resolver sees a normal declaration in that case, not a parse
        // failure).
        let result = resolve_src("val init = 5\nprintln(init)\n");
        assert!(result.type_errors.is_empty(), "{:?}", result.type_errors);
    }
}
