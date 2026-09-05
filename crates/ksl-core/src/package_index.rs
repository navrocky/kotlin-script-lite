//! Builds a cross-file index of every symbol a file can reach through a
//! `package`-declaring file (via `@file:Import`) or a synthetic stdlib
//! package (`kotlin.io`, `kotlin.math`, `kotlin.test`, ...) — the shared
//! foundation both `resolver::resolve_with_imports` (real `import a.b.c`/
//! `import a.b.*` resolution, feeding `ksl-cli`/`run_source`) and
//! `ksl-lsp`'s hover/completion/goto-definition build on. `resolver`
//! deliberately stays single-file/IO-free on its own (see its module doc
//! comment) — this is the multi-file layer a caller with filesystem access
//! builds once and passes in.
//!
//! Recursively follows `@file:Import` the same way `importer` does at
//! runtime (relative to *each file's own* directory, via the same
//! `resolve_path`), but as a read-only, best-effort index rather than an
//! execution: a missing file, a parse error, or a network import is
//! silently skipped rather than surfaced as an error.
//!
//! Also seeds every package `stdlib::STDLIB_PACKAGES` describes —
//! generically, by iterating that table, never by matching a package name
//! here — so a built-in package is indistinguishable, to every consumer of
//! this index, from a real user-declared one.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::Program;
use crate::error::Span;
use crate::resolver::{self, Symbol};
use crate::types::Type;

/// One package member: a top-level `fun`/`val`/`var` declared in some
/// `package`-declaring file, plus which file it came from (for goto-def/
/// diagnostics) — or a built-in package's member, which has no source file
/// at all, hence `Option`.
#[derive(Clone)]
pub struct PackageMember {
    pub symbol: Symbol,
    pub file: Option<PathBuf>,
}

#[derive(Default)]
pub struct PackageIndex {
    members: HashMap<String, Vec<PackageMember>>,
    /// Top-level `fun`/`val`/`var` declarations of a *package-less*
    /// `@file:Import`ed file, keyed by their own plain name (no package
    /// qualifier) — these get flattened into the importing file's own
    /// global scope at runtime (see `importer::run_program`'s `None =>
    /// interp.globals.clone()` branch).
    flat: HashMap<String, Vec<PackageMember>>,
}

impl PackageIndex {
    /// A fresh index seeded with every stdlib package, no user files —
    /// what every caller with no real filesystem context (unit tests,
    /// `resolve()`'s zero-context convenience wrapper, `static_check`)
    /// should start from, since stdlib packages are always fully known
    /// regardless of context. Use `Default`/`PackageIndex::default()`
    /// directly only when a *truly* empty index (not even stdlib) is
    /// deliberately wanted.
    pub fn new() -> Self {
        let mut index = PackageIndex::default();
        for pkg in crate::stdlib::STDLIB_PACKAGES {
            for sym in pkg.symbols {
                index.add_member(pkg.name.to_string(), builtin_symbol(sym), None);
            }
        }
        index
    }

    pub(crate) fn add_member(&mut self, package: String, symbol: Symbol, file: Option<PathBuf>) {
        self.members.entry(package).or_default().push(PackageMember { symbol, file });
    }

    pub(crate) fn add_flat_member(&mut self, symbol: Symbol, file: Option<PathBuf>) {
        self.flat.entry(symbol.name.clone()).or_default().push(PackageMember { symbol, file });
    }

    pub fn members_of(&self, package: &str) -> &[PackageMember] {
        self.members.get(package).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn find(&self, package: &str, member: &str) -> Option<&PackageMember> {
        self.members_of(package).iter().find(|m| m.symbol.name == member)
    }

    pub fn package_names(&self) -> impl Iterator<Item = &String> {
        self.members.keys()
    }

    pub fn has_package(&self, package: &str) -> bool {
        self.members.contains_key(package)
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

/// A `Symbol` for a built-in package member, with no real declaration site
/// to speak of — `def_span`/`ty`/`return_type`/`param_types`/`param_names`
/// all take the same "nothing more precise to say" defaults `ksl-core`
/// itself uses for a `Value::Native`'s own unknowable signature.
fn builtin_symbol(sym: &crate::stdlib::BuiltinSymbol) -> Symbol {
    Symbol {
        name: sym.name.to_string(),
        kind: sym.kind,
        def_span: Span::default(),
        mutable: false,
        signature: crate::stdlib::format_signature(sym),
        ty: Type::Function(Vec::new(), Box::new(Type::Unknown)),
        return_type: Type::Unknown,
        param_types: Vec::new(),
        param_names: Vec::new(),
        doc: Some(sym.doc.to_string()),
    }
}

/// Indexes every `package`-declaring file reachable from `program` — both
/// `program` itself (so a file can qualify-reference its own package) and
/// everything transitively reachable through its `@file:Import`s. `base_dir`
/// is where `program`'s own relative `@file:Import`s resolve against;
/// `own_file`, when the document has a real backing file, additionally lets
/// its own top-level declarations be attributed to that file (a document
/// with no real backing file, e.g. an unsaved editor buffer, still has its
/// imports resolved, just not its own declarations self-indexed).
pub fn build_package_index(program: &Program, base_dir: &Path, own_file: Option<&Path>) -> PackageIndex {
    let mut index = PackageIndex::new();
    let mut visited = HashSet::new();
    let own_canonical = own_file.and_then(|f| fs::canonicalize(f).ok());
    if let Some(canonical) = &own_canonical {
        visited.insert(canonical.clone());
    }
    index_program(program, base_dir, &mut index, &mut visited, own_canonical.as_deref(), true);
    index
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
            if let Some(file) = own_file {
                let resolved = resolver::resolve(program);
                for symbol in resolved.table.top_level_symbols() {
                    index.add_member(package_name.clone(), symbol.clone(), Some(file.to_path_buf()));
                }
            }
        }
        // A package-less file only gets flat-indexed when it's reached
        // *through* an `@file:Import` (`!is_entry`) — the entry document's
        // own top-level names are already resolved directly (real locals),
        // so indexing them again here would be redundant.
        None if !is_entry => {
            if let Some(file) = own_file {
                let resolved = resolver::resolve(program);
                for symbol in resolved.table.top_level_symbols() {
                    index.add_flat_member(symbol.clone(), Some(file.to_path_buf()));
                }
            }
        }
        None => {}
    }

    for import in &program.imports {
        if crate::importer::is_url(&import.path) {
            continue;
        }
        let resolved_path = crate::importer::resolve_path(base_dir, &import.path);
        let Ok(canonical) = fs::canonicalize(&resolved_path) else { continue };
        if !visited.insert(canonical.clone()) {
            continue; // already indexed (diamond import) or a cycle — skip.
        }
        let Ok(source) = fs::read_to_string(&canonical) else { continue };
        let Ok(imported_program) = crate::parse_only(&source) else { continue };
        let child_base_dir = canonical.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        index_program(&imported_program, &child_base_dir, index, visited, Some(&canonical), false);
    }
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
            let dir = std::env::temp_dir().join(format!("ksl_core_pkg_index_test_{name}_{n}_{}", std::process::id()));
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
        let program = crate::parse_only(&src).unwrap();
        build_package_index(&program, dir.path(), Some(&entry_path))
    }

    #[test]
    fn build_package_index_always_seeds_built_in_packages() {
        let dir = TempDir::new("builtin_only");
        dir.write("main.ksl", "println(1)\n");

        let index = build(&dir, "main.ksl");
        assert!(index.package_names().any(|n| n == "kotlin.test"));
        let names: Vec<&str> = index.members_of("kotlin.test").iter().map(|m| m.symbol.name.as_str()).collect();
        assert!(names.contains(&"assertEquals"));
        assert!(names.contains(&"assertTrue"));
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
