use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Range, Url};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Jump target for the symbol at `position`, or `None` if there's nothing
/// to jump to there. Same-file declarations/uses resolve to a `Location` in
/// `uri` (the current document); a package-qualified `a.b.c` reference
/// resolves — via the cross-file package index, see
/// `analysis::Analysis::packages` — to a `Location` in whichever file
/// actually declares it; a bare name with no local declaration at all
/// falls back to a package-less `@file:Import`ed file's flat member (see
/// `PackageIndex::flat_member`); and, independent of all of the above, the
/// file name inside an `@file:Import("...")` itself jumps straight to that
/// file (its own start, since a whole file — not a declaration inside it —
/// is the target).
pub fn goto_definition(analysis: &Analysis, uri: &Url, position: Position) -> Option<GotoDefinitionResponse> {
    let line = position.line + 1;
    let col = position.character + 1;

    if let Some(target) = goto_import_target(analysis, line, col) {
        return Some(target);
    }

    if let Some(symbol) = analysis.resolved.table.symbol_at(line, col) {
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: span_to_range(symbol.def_span),
        }));
    }

    if let Some((_, path)) = analysis.resolved.table.qualified_ref_at(line, col) {
        let (package, member) = path.split_at(path.len() - 1);
        // A built-in package member (e.g. `kotlin.test.assertEquals`) has
        // no source file to jump to — `found.file` is `None`, same as a
        // plain (non-package) stdlib function already is for this
        // function's "last resort" branch below (see that branch's own
        // comment). Falls through rather than returning `None` outright,
        // matching this whole `if let` already not returning on a `find`
        // miss either.
        if let Some(found) = analysis.packages.find(&package.join("."), &member[0]) {
            if let Some(uri) = &found.file {
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: uri.clone(),
                    range: span_to_range(found.symbol.def_span),
                }));
            }
        }
    }

    // Last resort: a bare name with no local declaration of its own at
    // all — same fallback order as `hover::hover`'s final branch, minus
    // the stdlib case (a stdlib function has no source location to jump to).
    let name = analysis.resolved.table.name_ref_at(line, col)?;
    let found = analysis.packages.flat_member(name)?;
    let uri = found.file.clone()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: span_to_range(found.symbol.def_span),
    }))
}

/// If `(line, col)` falls within one of this document's own
/// `@file:Import("...")` path strings, resolves that path (the same way
/// `ksl_core::importer` would at runtime) and jumps to the start of that
/// file — or `None` if the path doesn't resolve to a real file, so a
/// broken import falls through to every other check instead of dead-ending
/// the whole lookup.
fn goto_import_target(analysis: &Analysis, line: u32, col: u32) -> Option<GotoDefinitionResponse> {
    let import = analysis.imports.iter().find(|i| i.path_span.contains_line_col(line, col))?;
    if ksl_core::importer::is_url(&import.path) {
        return None;
    }
    let base_dir = analysis.own_file.parent().unwrap_or_else(|| std::path::Path::new("."));
    let resolved_path = ksl_core::importer::resolve_path(base_dir, &import.path);
    let target_uri = Url::from_file_path(&resolved_path).ok()?;
    if !resolved_path.is_file() {
        return None;
    }
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: target_uri,
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_goto_def_{name}_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uri_for(path: &std::path::Path) -> Url {
        Url::from_file_path(fs::canonicalize(path).unwrap()).unwrap()
    }

    #[test]
    fn goto_definition_on_a_flat_cross_file_import_jumps_to_the_other_file() {
        let dir = temp_dir("flat");
        let greeter_path = dir.join("greeter.ksl");
        fs::write(&greeter_path, "fun greet(name: String): String {\n    return name\n}\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        let uri = uri_for(&main_path);
        // "println(" is 8 chars, so "greet" starts at column 9.
        let response = goto_definition(&a, &uri, Position::new(1, 8)).expect("greet should resolve");
        match response {
            GotoDefinitionResponse::Scalar(loc) => {
                assert_eq!(loc.uri, uri_for(&greeter_path));
                // Points at greet's own declaration, line 1.
                assert_eq!(loc.range.start.line, 0);
            }
            other => panic!("expected a scalar location, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn goto_definition_on_an_import_path_jumps_to_that_file() {
        let dir = temp_dir("import_path");
        let greeter_path = dir.join("greeter.ksl");
        fs::write(&greeter_path, "fun greet(): String = \"hi\"\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"greeter.ksl\")\nprintln(1)\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        let uri = uri_for(&main_path);
        // "@file:Import(\"" is 14 chars, so the path text starts at column 15.
        let response = goto_definition(&a, &uri, Position::new(0, 16)).expect("the path should resolve");
        match response {
            GotoDefinitionResponse::Scalar(loc) => {
                assert_eq!(loc.uri, uri_for(&greeter_path));
            }
            other => panic!("expected a scalar location, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn goto_definition_on_a_broken_import_path_stays_none() {
        let dir = temp_dir("broken_import_path");
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"nope.ksl\")\nprintln(1)\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        let uri = uri_for(&main_path);
        assert!(goto_definition(&a, &uri, Position::new(0, 16)).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn goto_definition_on_a_built_in_package_member_stays_none() {
        // `kotlin.test.assertEquals` is a real, resolvable qualified
        // reference (`package_index`'s built-in seeding), but has no
        // source file to jump to — must return `None`, not a broken URI
        // (`PackageMember::file` is `None` for it; see that field's own
        // doc comment), and must not panic.
        let dir = temp_dir("builtin_no_location");
        let main_path = dir.join("main.ksl");
        let text = "println(kotlin.test.assertEquals(1, 1))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        let uri = uri_for(&main_path);
        // "println(kotlin.test." is 21 chars, so "assertEquals" starts at
        // column 22 (0-based 21).
        assert!(goto_definition(&a, &uri, Position::new(0, 25)).is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
