//! Builds a cross-file index of `package`-declaring files reachable from a
//! document via `@file:Import`, for hover/goto-definition/completion on
//! `a.b.c` qualified names. `ksl_core::resolver` deliberately stays
//! single-file (see its module doc comment) — this is the multi-file layer
//! on top of it, entirely inside `ksl-lsp`.
//!
//! Recursively follows `@file:Import` the same way `ksl_core::importer`
//! does at runtime (relative to *each file's own* directory, via the same
//! `resolve_path`), but as a read-only, best-effort index rather than an
//! execution: a missing file, a parse error, or a network import is
//! silently skipped rather than surfaced as an error — an editor still
//! wants hover/completion to work for everything that *does* resolve, even
//! if one import is currently broken.
//!
//! Also seeds every package `ksl_core::stdlib::STDLIB_PACKAGES` describes
//! (`kotlin.io`, `kotlin.text`, `kotlin.math`, `kotlin.test`, ...) —
//! generically, by iterating that table, never by matching a package name
//! here (every stdlib symbol belongs to a real, named package there, no
//! flat/root special case). This is what makes a built-in package
//! indistinguishable, to every consumer of this index
//! (`completion`/`hover`/`goto_definition`), from a real user-declared
//! one: neither this module nor any of them special-cases any package name
//! anywhere.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ksl_core::ast::Program;
use ksl_core::error::Span;
use ksl_core::resolver::{self, Symbol};
use ksl_core::types::Type;
use tower_lsp::lsp_types::Url;

/// One package member: a top-level `fun`/`val`/`var` declared in some
/// `package`-declaring file, plus which file it came from (for goto-def) —
/// or a built-in package's member (see the module doc comment), which has
/// no source file at all, hence `Option`: `None` here means the same thing
/// `goto_definition.rs` already means by "a stdlib function has no source
/// location to jump to" for a plain (non-package) stdlib function.
pub struct PackageMember {
    pub symbol: Symbol,
    pub file: Option<Url>,
}

#[derive(Default)]
pub struct PackageIndex {
    members: HashMap<String, Vec<PackageMember>>,
    /// Top-level `fun`/`val`/`var` declarations of a *package-less*
    /// `@file:Import`ed file, keyed by their own plain name (no package
    /// qualifier) — these get flattened into the importing file's own
    /// global scope at runtime (see `importer::run_program`'s `None =>
    /// interp.globals.clone()` branch), so a name like `greet` used
    /// unqualified needs its own lookup here, distinct from `members`
    /// (which is keyed by `package.member` and only ever populated for a
    /// file that actually declares a `package`).
    flat: HashMap<String, Vec<PackageMember>>,
}

impl PackageIndex {
    pub fn members_of(&self, package: &str) -> &[PackageMember] {
        self.members.get(package).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn find(&self, package: &str, member: &str) -> Option<&PackageMember> {
        self.members_of(package).iter().find(|m| m.symbol.name == member)
    }

    pub fn package_names(&self) -> impl Iterator<Item = &String> {
        self.members.keys()
    }

    /// The flat (package-less) member named `name`, if any — see `flat`'s
    /// doc comment. First match wins on a same-named clash across two
    /// separate imports (rare, and no more precise than the runtime's own
    /// last-import-wins global-scope behavior would be anyway).
    pub fn flat_member(&self, name: &str) -> Option<&PackageMember> {
        self.flat.get(name).and_then(|v| v.first())
    }

    /// Every flat (package-less) member indexed, for completion.
    pub fn flat_members(&self) -> impl Iterator<Item = &PackageMember> {
        self.flat.values().flat_map(|v| v.iter())
    }
}

/// Indexes every `package`-declaring file reachable from `program` — both
/// `program` itself (so a file can qualify-reference its own package) and
/// everything transitively reachable through its `@file:Import`s.
/// `own_file` is the document's own path — needed both to resolve its
/// `@file:Import`s (relative to its own directory, matching
/// `Importer::run_top_level`) and to attribute its own `package` members to
/// the right file. If it doesn't canonicalize (no real backing file, e.g.
/// an unsaved buffer), the document's own declarations are silently not
/// self-indexed, but its imports are still resolved relative to its
/// literal (uncanonicalized) parent directory.
pub fn build_package_index(program: &Program, own_file: &Path) -> PackageIndex {
    let mut index = PackageIndex::default();

    // Built-ins first, unconditionally, independent of `program`/`own_file`
    // entirely — see the module doc comment. Every package is indexed the
    // same way, `default_imported` or not — a bare (default-imported)
    // stdlib function is *also* handled separately, by
    // `ksl-lsp::completion`/`hover`'s own
    // `stdlib::default_imported_symbols()` calls, so it's reachable both
    // unqualified and fully qualified, matching real Kotlin.
    for pkg in ksl_core::stdlib::STDLIB_PACKAGES {
        for sym in pkg.symbols {
            index.members.entry(pkg.name.to_string()).or_default().push(PackageMember {
                symbol: builtin_symbol(sym),
                file: None,
            });
        }
    }

    let mut visited = HashSet::new();
    let base_dir = own_file.parent().unwrap_or_else(|| Path::new("."));
    let own_canonical = fs::canonicalize(own_file).ok();
    if let Some(canonical) = &own_canonical {
        visited.insert(canonical.clone());
    }
    index_program(program, base_dir, &mut index, &mut visited, own_canonical.as_deref(), true);
    index
}

/// A `Symbol` for a built-in package member, with no real declaration site
/// to speak of — `def_span`/`ty`/`return_type`/`param_types`/`param_names`
/// all take the same "nothing more precise to say" defaults `ksl-core`
/// itself uses for a `Value::Native`'s own unknowable signature (see
/// `types::Type::from_value`'s `Value::Native` arm). `kind` is carried
/// through from `stdlib::BuiltinSymbol` rather than hardcoded — every entry
/// today is `SymbolKind::Fun`, but this needs no change if that ever stops
/// being true.
fn builtin_symbol(sym: &ksl_core::stdlib::BuiltinSymbol) -> Symbol {
    Symbol {
        name: sym.name.to_string(),
        kind: sym.kind,
        def_span: Span::default(),
        mutable: false,
        signature: ksl_core::stdlib::format_signature(sym),
        ty: Type::Function(Vec::new(), Box::new(Type::Unknown)),
        return_type: Type::Unknown,
        param_types: Vec::new(),
        param_names: Vec::new(),
        doc: Some(sym.doc.to_string()),
    }
}

fn index_program(
    program: &Program,
    base_dir: &Path,
    index: &mut PackageIndex,
    visited: &mut HashSet<PathBuf>,
    own_file: Option<&Path>,
    is_entry: bool,
) {
    match &program.package {
        Some(pkg) => {
            let package_name = pkg.path.join(".");
            // A canonical own-file path is needed for a Url; a document with
            // no real backing file (e.g. an unsaved buffer) simply can't be
            // goto-def'd into, so it's skipped rather than indexed with a
            // bogus URI.
            if let Some(file) = own_file.and_then(path_to_url) {
                let resolved = resolver::resolve(program);
                for symbol in resolved.table.top_level_symbols() {
                    index.members.entry(package_name.clone()).or_default().push(PackageMember {
                        symbol: symbol.clone(),
                        file: Some(file.clone()),
                    });
                }
            }
        }
        // A package-less file only gets flat-indexed when it's reached
        // *through* an `@file:Import` (`!is_entry`) — the entry document's
        // own top-level names are already resolved directly via its own
        // `analysis.resolved` (real locals, not a cross-file lookup), so
        // indexing them again here would be redundant, not wrong, but
        // needlessly duplicates the current document's live tree with a
        // separately-parsed one that's already stale the moment it's typed.
        None if !is_entry => {
            if let Some(file) = own_file.and_then(path_to_url) {
                let resolved = resolver::resolve(program);
                for symbol in resolved.table.top_level_symbols() {
                    index.flat.entry(symbol.name.clone()).or_default().push(PackageMember {
                        symbol: symbol.clone(),
                        file: Some(file.clone()),
                    });
                }
            }
        }
        None => {}
    }

    for import in &program.imports {
        if ksl_core::importer::is_url(&import.path) {
            continue;
        }
        let resolved_path = ksl_core::importer::resolve_path(base_dir, &import.path);
        let Ok(canonical) = fs::canonicalize(&resolved_path) else { continue };
        if !visited.insert(canonical.clone()) {
            continue; // already indexed (diamond import) or a cycle — skip.
        }
        let Ok(source) = fs::read_to_string(&canonical) else { continue };
        let Ok(imported_program) = ksl_core::parse_only(&source) else { continue };
        let child_base_dir = canonical.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        index_program(&imported_program, &child_base_dir, index, visited, Some(&canonical), false);
    }
}

fn path_to_url(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("ksl_lsp_pkg_index_test_{name}_{n}_{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            path
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn build(dir: &TempDir, entry_file: &str) -> PackageIndex {
        let entry_path = dir.path().join(entry_file);
        let src = fs::read_to_string(&entry_path).unwrap();
        let program = ksl_core::parse_only(&src).unwrap();
        build_package_index(&program, &entry_path)
    }

    #[test]
    fn build_package_index_always_seeds_built_in_packages() {
        // Independent of any real file at all — even a script with no
        // `package`/`@file:Import` whatsoever still has `kotlin.test`
        // indexed, generically, from `stdlib::STDLIB_PACKAGES`.
        let dir = TempDir::new("builtin_only");
        dir.write("main.ksl", "println(1)\n");

        let index = build(&dir, "main.ksl");
        assert!(index.package_names().any(|n| n == "kotlin.test"));
        let names: Vec<&str> = index.members_of("kotlin.test").iter().map(|m| m.symbol.name.as_str()).collect();
        assert!(names.contains(&"assertEquals"));
        assert!(names.contains(&"assertTrue"));
        assert!(names.contains(&"assertFalse"));
        assert!(names.contains(&"assertNull"));
        assert!(names.contains(&"assertNotNull"));
    }

    #[test]
    fn built_in_package_members_have_no_file() {
        let dir = TempDir::new("builtin_no_file");
        dir.write("main.ksl", "println(1)\n");

        let index = build(&dir, "main.ksl");
        let found = index.find("kotlin.test", "assertEquals").expect("assertEquals should be indexed");
        assert!(found.file.is_none());
    }

    #[test]
    fn a_real_packages_members_are_unaffected_by_built_in_seeding() {
        let dir = TempDir::new("real_package_unaffected");
        dir.write("pkg.ksl", "package a.b\nfun helper(): Int = 1\n");
        dir.write("main.ksl", "@file:Import(\"pkg.ksl\")\nprintln(1)\n");

        let index = build(&dir, "main.ksl");
        assert_eq!(index.members_of("a.b").len(), 1);
        assert!(index.members_of("a.b")[0].file.is_some());
    }

    #[test]
    fn indexes_a_directly_imported_package() {
        let dir = TempDir::new("direct");
        dir.write("pkg.ksl", "package util.math\nfun square(x: Int): Int = x * x\n");
        dir.write("main.ksl", "@file:Import(\"pkg.ksl\")\nprintln(util.math.square(5))\n");

        let index = build(&dir, "main.ksl");
        let member = index.find("util.math", "square").expect("square should be indexed");
        assert_eq!(member.symbol.signature, "fun square(x: Int): Int");
    }

    #[test]
    fn indexes_transitively_imported_packages() {
        let dir = TempDir::new("transitive");
        dir.write("deep.ksl", "package a.b\nfun helper(): Int = 1\n");
        dir.write("mid.ksl", "@file:Import(\"deep.ksl\")\nprintln(1)\n");
        dir.write("main.ksl", "@file:Import(\"mid.ksl\")\nprintln(1)\n");

        let index = build(&dir, "main.ksl");
        assert!(index.find("a.b", "helper").is_some());
    }

    #[test]
    fn a_file_can_reference_its_own_declared_package() {
        let dir = TempDir::new("self_package");
        dir.write("main.ksl", "package a.b\nfun greet(): String = \"hi\"\nprintln(a.b.greet())\n");

        let index = build(&dir, "main.ksl");
        assert!(index.find("a.b", "greet").is_some());
    }

    #[test]
    fn missing_or_broken_import_is_skipped_not_an_error() {
        let dir = TempDir::new("broken");
        dir.write("main.ksl", "@file:Import(\"nope.ksl\")\nprintln(1)\n");

        let index = build(&dir, "main.ksl"); // must not panic
        // No *real* user package — only the built-ins every index carries
        // regardless of the program indexed (see `missing_or_broken_import`'s
        // sibling test `build_package_index_always_seeds_built_in_packages`).
        assert!(index.members_of("a.b").is_empty());
    }

    #[test]
    fn package_less_import_flattens_its_top_level_names() {
        let dir = TempDir::new("flat");
        dir.write("greeter.ksl", "fun greet(name: String): String {\n    return \"Hello, $name!\"\n}\n");
        dir.write("main.ksl", "@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n");

        let index = build(&dir, "main.ksl");
        let member = index.flat_member("greet").expect("greet should be flat-indexed");
        assert_eq!(member.symbol.signature, "fun greet(name: String): String");
    }

    #[test]
    fn package_less_import_transitively_flattens_too() {
        let dir = TempDir::new("flat_transitive");
        dir.write("deep.ksl", "fun helper(): Int = 1\n");
        dir.write("mid.ksl", "@file:Import(\"deep.ksl\")\nprintln(1)\n");
        dir.write("main.ksl", "@file:Import(\"mid.ksl\")\nprintln(1)\n");

        let index = build(&dir, "main.ksl");
        assert!(index.flat_member("helper").is_some());
    }

    #[test]
    fn the_entry_documents_own_top_level_names_are_not_flat_indexed() {
        // Redundant, not wrong -- the entry document's own locals are
        // already covered by `analysis.resolved`, so re-indexing them here
        // from a separately-parsed (potentially stale) tree would be
        // pointless.
        let dir = TempDir::new("flat_entry");
        dir.write("main.ksl", "fun helper(): Int = 1\nprintln(helper())\n");

        let index = build(&dir, "main.ksl");
        assert!(index.flat_member("helper").is_none());
    }

    #[test]
    fn diamond_import_does_not_duplicate_members() {
        let dir = TempDir::new("diamond");
        dir.write("d.ksl", "package a.b\nfun helper(): Int = 1\n");
        dir.write("p1.ksl", "@file:Import(\"d.ksl\")\nprintln(1)\n");
        dir.write("p2.ksl", "@file:Import(\"d.ksl\")\nprintln(1)\n");
        dir.write("main.ksl", "@file:Import(\"p1.ksl\")\n@file:Import(\"p2.ksl\")\nprintln(1)\n");

        let index = build(&dir, "main.ksl");
        assert_eq!(index.members_of("a.b").len(), 1);
    }
}
