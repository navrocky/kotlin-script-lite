use std::path::{Path, PathBuf};
use std::sync::Arc;

use ksl_core::ast::{ImportDecl, Program};
use ksl_core::resolver::{self, ResolveResult};

use crate::package_index::{self, PackageIndex};

/// A resolved document, ready for hover/goto-definition/completion queries.
pub struct Analysis {
    pub resolved: ResolveResult,
    /// Every `package`-declaring file reachable from this document (itself
    /// included, plus everything transitively `@file:Import`ed) — lets
    /// hover/goto-definition/completion answer `a.b.c` qualified-name
    /// queries, which `resolved` alone can't (it's single-file).
    pub packages: PackageIndex,
    /// This document's own `@file:Import(...)` declarations — needed by
    /// goto-definition to jump straight to the file one of them names
    /// (matched by `ImportDecl::path_span`), separate from `resolved`
    /// (which is scoped to expressions/statements, not import declarations).
    pub imports: Vec<ImportDecl>,
    /// This document's own path, as given to `analyze` — needed by
    /// completion/goto-definition for anything relative to the file on
    /// disk: resolving `@file:Import("...")` path completion candidates,
    /// and jumping to the file a partially- or fully-typed import path
    /// names. Not necessarily canonical (see `analyze`'s own doc comment).
    pub own_file: PathBuf,
    /// False if `text` itself didn't parse and this fell back to a
    /// previously cached tree — see the module doc comment on
    /// `document_store::DocumentState`.
    pub is_current: bool,
}

/// Parses `text`; on success, resolves it and returns that (also building
/// the package index reachable from `own_file`). On failure, falls back to
/// re-resolving `last_good` if given, so hover/goto-definition/completion
/// keep working against the last text that did parse rather than going
/// blank on every incomplete edit. Returns `None` only when `text` fails to
/// parse and there's no fallback tree yet (e.g. the very first keystroke of
/// a new, empty-then-broken document).
///
/// `own_file` is the document's own path, used both to resolve its
/// `@file:Import`s (relative to its own directory) and to attribute its own
/// `package` declaration's members to the right file for goto-definition;
/// see `package_index::build_package_index`. This does real filesystem I/O
/// (reading every transitively-imported file) — fine here since `analyze`
/// only runs on demand for a hover/goto-definition/completion request, not
/// on every keystroke like diagnostics.
///
/// Callers that also need to cache the newly-parsed tree as the document's
/// new "last good" (see `Backend::update_last_good_tree`) do their own
/// separate `parse_only` for that, rather than this function handing back
/// `program` — resolving is real work (a full AST walk plus, now, reading
/// every imported file), and running it on every keystroke just to maybe
/// cache a tree nobody asked to resolve yet would be wasted when no
/// hover/goto-definition/completion request is pending.
pub fn analyze(text: &str, last_good: Option<Arc<Program>>, own_file: &Path) -> Option<Analysis> {
    match ksl_core::parse_only(text) {
        Ok(program) => {
            let resolved = resolver::resolve(&program);
            let packages = package_index::build_package_index(&program, own_file);
            let imports = program.imports.clone();
            Some(Analysis { resolved, packages, imports, own_file: own_file.to_path_buf(), is_current: true })
        }
        Err(_) => last_good.map(|program| {
            let resolved = resolver::resolve(&program);
            let packages = package_index::build_package_index(&program, own_file);
            let imports = program.imports.clone();
            Analysis { resolved, packages, imports, own_file: own_file.to_path_buf(), is_current: false }
        }),
    }
}
