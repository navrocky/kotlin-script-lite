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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ksl_core::ast::Program;
use ksl_core::resolver::{self, Symbol};
use tower_lsp::lsp_types::Url;

/// One package member: a top-level `fun`/`val`/`var` declared in some
/// `package`-declaring file, plus which file it came from (for goto-def).
pub struct PackageMember {
    pub symbol: Symbol,
    pub file: Url,
}

#[derive(Default)]
pub struct PackageIndex {
    members: HashMap<String, Vec<PackageMember>>,
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
    let mut visited = HashSet::new();
    let base_dir = own_file.parent().unwrap_or_else(|| Path::new("."));
    let own_canonical = fs::canonicalize(own_file).ok();
    if let Some(canonical) = &own_canonical {
        visited.insert(canonical.clone());
    }
    index_program(program, base_dir, &mut index, &mut visited, own_canonical.as_deref());
    index
}

fn index_program(
    program: &Program,
    base_dir: &Path,
    index: &mut PackageIndex,
    visited: &mut HashSet<PathBuf>,
    own_file: Option<&Path>,
) {
    if let Some(pkg) = &program.package {
        let package_name = pkg.path.join(".");
        // A canonical own-file path is needed for a Url; a document with no
        // real backing file (e.g. an unsaved buffer) simply can't be
        // goto-def'd into, so it's skipped rather than indexed with a bogus URI.
        if let Some(file) = own_file.and_then(path_to_url) {
            let resolved = resolver::resolve(program);
            for symbol in resolved.table.top_level_symbols() {
                index.members.entry(package_name.clone()).or_default().push(PackageMember {
                    symbol: symbol.clone(),
                    file: file.clone(),
                });
            }
        }
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
        index_program(&imported_program, &child_base_dir, index, visited, Some(&canonical));
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
    fn indexes_a_directly_imported_package() {
        let dir = TempDir::new("direct");
        dir.write("pkg.ksl", "package util.math\nfun square(x: Int): Int = x * x\n");
        dir.write("main.ksl", "@file:Import(\"pkg.ksl\")\nprintln(util.math.square(5))\n");

        let index = build(&dir, "main.ksl");
        let member = index.find("util.math", "square").expect("square should be indexed");
        assert_eq!(member.symbol.signature, "fun square(x: Int)");
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
        assert!(index.package_names().next().is_none());
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
