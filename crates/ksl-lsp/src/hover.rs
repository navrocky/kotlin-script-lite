use ksl_core::error::Span;
use ksl_core::resolver::{Symbol, SymbolKind};
use ksl_core::stdlib::{format_signature, BUILTIN_METHODS};
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Hover info for the symbol at `position` (0-based, LSP convention), or
/// `None` if there's no symbol there. Same-file declarations/uses are
/// checked first; a package-qualified `a.b.c` reference falls back to the
/// cross-file package index (see `analysis::Analysis::packages`); a `.`
/// method call that isn't a package reference (e.g. `s.uppercase()` where
/// `s` is a real local) falls back further to `BUILTIN_METHOD_SIGNATURES`;
/// finally, a bare name that never resolved to a local symbol at all (this
/// resolver is single-file, see its module doc comment) falls back to
/// `stdlib::default_imported_symbols()` (`println`, etc.) and then to
/// whatever this file's own `import`/`@file:Import` brought into bare
/// scope (`greet`, brought in unqualified — see
/// `resolver::ResolveResult::imported_symbols`).
pub fn hover(analysis: &Analysis, position: Position) -> Option<Hover> {
    let line = position.line + 1;
    let col = position.character + 1;

    if let Some((occurrence_span, symbol)) = analysis.resolved.table.occurrence_at(line, col) {
        return Some(build_hover(symbol, Some(occurrence_span), analysis.is_current, None));
    }

    // A named argument's own label at a call site (`name` in
    // `greet(name = "World")`) — never a real local declaration/use of its
    // own (see `named_arg_label_at`'s doc comment), so it's checked before
    // `qualified_ref_at`/`method_ref_at` below, none of which apply to it
    // either.
    if let Some((label_span, symbol)) = analysis.resolved.table.named_arg_label_at(line, col) {
        return Some(build_hover(symbol, Some(label_span), analysis.is_current, None));
    }

    if let Some((qualified_span, path)) = analysis.resolved.table.qualified_ref_at(line, col) {
        let (package, member) = path.split_at(path.len() - 1);
        let package = package.join(".");
        let found = analysis.packages.find(&package, &member[0])?;
        return Some(build_hover(&found.symbol, Some(qualified_span), analysis.is_current, Some(&package)));
    }

    // No qualified-name candidate at all at this position — either the
    // receiver is a real local (`s.uppercase()`) or not a dotted-name
    // chain to begin with (`"hi".uppercase()`); either way, try it as a
    // builtin method instead of giving up. When the receiver's type is
    // concretely known, it must also match the candidate's receiver
    // type — so a nonsensical `5.uppercase()` (an `Int` receiver)
    // correctly finds nothing here, rather than describing
    // `String.uppercase()` by name alone.
    if let Some(method) = analysis.resolved.table.method_ref_at(line, col) {
        let receiver_ty = analysis.resolved.table.method_receiver_type_at(line, col);
        // A `T?` receiver dispatches against `T`'s own builtin methods
        // (matches Kotlin's safe-call semantics — see the identical
        // unwrap in `completion.rs`), so unwrap one `Nullable` layer
        // before comparing names.
        let dispatch_name = receiver_ty.map(|t| match t {
            ksl_core::types::Type::Nullable(inner) => inner.name(),
            other => other.name(),
        });
        // Same chain-walk as `completion.rs`'s `builtin_method_completions`
        // — a concretely known receiver must match somewhere in its own
        // builtin-class chain (so `5.toString()` finds `Any`'s entry, not
        // just an exact `Int` one), an unknown one still matches by method
        // name alone (the "over-suggest" hedge).
        if let Some(m) = BUILTIN_METHODS.iter().find(|m| {
            m.symbol.name == method
                && match &dispatch_name {
                    Some(name) => ksl_core::stdlib::builtin_type_chain(name).contains(&m.receiver),
                    None => true,
                }
        }) {
            return Some(build_builtin_method_hover(&format_signature(&m.symbol), m.symbol.doc));
        }
    }

    // A bare name with no local declaration of its own at all —
    // `occurrence_at` above only covers names that resolve to a real local
    // symbol, so this is what's left for a stdlib function or a name this
    // file's own `import`/`@file:Import` brought into bare scope (real or
    // built-in package, wildcard or named, or a package-less flat
    // namespace member — see `resolver::ResolveResult::imported_symbols`).
    if let Some(name) = analysis.resolved.table.name_ref_at(line, col) {
        if let Some(sym) = ksl_core::stdlib::default_imported_symbols().find(|s| s.name == name) {
            return Some(build_stdlib_function_hover(&ksl_core::stdlib::format_signature(sym), sym.doc));
        }
        if let Some(found) = analysis.resolved.imported_symbols.iter().find(|m| m.symbol.name == name) {
            return Some(build_hover(&found.symbol, None, analysis.is_current, found.package.as_deref()));
        }
    }

    // Genuinely last resort: a `.field`/`.method()` use on a cross-file
    // class instance — covered by none of the checks above (no local
    // `Symbol` index for `occurrence_at`; not a dotted-name chain for
    // `qualified_ref_at`, since the receiver is a real local instance, not
    // an unresolved path; not a `BUILTIN_METHODS` match; and a field/method
    // name is never a bare `Name` expression, so `name_ref_at` doesn't see
    // it either) — but the resolver already recorded exactly which
    // cross-file symbol it came from (see `ResolveResult::cross_file_ref_at`'s
    // own doc comment). No package qualifier: a field isn't referenced as
    // `Point.x` the way a package-qualified function is.
    analysis.resolved.cross_file_ref_at(line, col).map(|cross_ref| build_hover(&cross_ref.symbol, None, analysis.is_current, None))
}

/// Inserts `package.` right before `name`'s first occurrence in
/// `signature` — e.g. `("fun square(x: Int): Int", "square", "util.math")`
/// becomes `"fun util.math.square(x: Int): Int"`. Every `signature` this is
/// ever called with is built by `resolver`/`stdlib` itself in the fixed
/// `<keyword> <name>(...)`/`<keyword> <name>: <type>` shape, so `name`'s
/// first occurrence is always its own declaration site, never some
/// unrelated substring — falls back to the bare signature, unqualified,
/// on the (never-expected-in-practice) chance it isn't found at all.
fn qualify_signature(signature: &str, name: &str, package: &str) -> String {
    match signature.find(name) {
        Some(idx) => format!("{}{package}.{name}{}", &signature[..idx], &signature[idx + name.len()..]),
        None => signature.to_string(),
    }
}

fn build_builtin_method_hover(signature: &str, doc: &str) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```ksl\n{signature}\n```\n\n*builtin method*\n\n{doc}"),
        }),
        range: None,
    }
}

fn build_stdlib_function_hover(signature: &str, doc: &str) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```ksl\n{signature}\n```\n\n*stdlib function*\n\n{doc}"),
        }),
        range: None,
    }
}

/// `symbol`'s hover, with its signature qualified by `package` (if any) —
/// `Some("util.math")` turns `fun square(x: Int): Int` into
/// `fun util.math.square(x: Int): Int`, matching how it's actually
/// referenced from outside its own package. `None` for a same-file
/// declaration/use (`occurrence_at`) or a named-argument label
/// (`named_arg_label_at`), neither of which has — or needs — a package
/// qualifier of its own.
fn build_hover(symbol: &Symbol, range_span: Option<Span>, is_current: bool, package: Option<&str>) -> Hover {
    let kind_label = match symbol.kind {
        SymbolKind::Val => "val",
        SymbolKind::Var => "var",
        SymbolKind::Fun => "function",
        SymbolKind::Param => "parameter",
        SymbolKind::Class => "class",
        SymbolKind::Field => "field",
        SymbolKind::Method => "method",
    };
    let signature = match package {
        Some(pkg) if !pkg.is_empty() => qualify_signature(&symbol.signature, &symbol.name, pkg),
        _ => symbol.signature.clone(),
    };
    let mut value = format!("```ksl\n{}\n```\n\n*{}*", signature, kind_label);
    if let Some(doc) = &symbol.doc {
        value.push_str("\n\n");
        value.push_str(doc);
    }
    if !is_current {
        value.push_str("\n\n_(from the last version of this file that parsed successfully)_");
    }

    Hover {
        contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
        // Highlights the actual token/expression under the cursor, not the
        // (possibly elsewhere, possibly cross-file) declaration. `None`
        // only for a flat cross-file member, which has no dedicated
        // occurrence span of its own at this call site (see
        // `SymbolTable::name_ref_at`'s doc comment) — same as a builtin
        // method hover, which has never highlighted a specific range either.
        range: range_span.map(span_to_range),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;
    use std::path::Path;

    fn hover_at(text: &str, line: u32, col: u32) -> Option<Hover> {
        let analysis =
            analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).expect("should parse");
        hover(&analysis, Position::new(line - 1, col - 1))
    }

    fn markdown_value(h: &Hover) -> &str {
        match &h.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("expected markup contents"),
        }
    }

    #[test]
    fn hover_on_a_stdlib_function_shows_its_signature() {
        let text = "println(\"hi\")\n";
        let h = hover_at(text, 1, 3).expect("println should hover");
        assert!(markdown_value(&h).contains("println"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("stdlib function"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_fun_shows_its_return_type() {
        // Annotated return type.
        let text = "fun greet(name: String): String {\n    return \"Hello, $name!\"\n}\nprintln(greet(\"x\"))\n";
        let h = hover_at(text, 1, 5).expect("greet should hover");
        assert!(markdown_value(&h).contains("fun greet(name: String): String"), "{}", markdown_value(&h));

        // Inferred (unannotated) return type.
        let text2 = "fun add(a: Int, b: Int) = a + b\nprintln(add(1, 2))\n";
        let h2 = hover_at(text2, 1, 5).expect("add should hover");
        assert!(markdown_value(&h2).contains("fun add(a: Int, b: Int): Int"), "{}", markdown_value(&h2));
    }

    #[test]
    fn hover_on_a_documented_fun_shows_its_summary_and_returns() {
        let text = "/**\n * Adds two numbers.\n * @param a the first number\n * @param b the second number\n * @return the sum\n */\nfun add(a: Int, b: Int): Int {\n    return a + b\n}\nprintln(add(1, 2))\n";
        let h = hover_at(text, 7, 5).expect("add should hover");
        let value = markdown_value(&h);
        assert!(value.contains("Adds two numbers."), "{value}");
        assert!(value.contains("**Returns:** the sum"), "{value}");
    }

    #[test]
    fn hover_on_a_documented_fun_also_lists_its_parameters() {
        // Hovering the *function* shows every `@param`'s description too,
        // not just its own summary/returns -- no need to hover each
        // parameter separately to see the full picture.
        let text = "/**\n * Adds two numbers.\n * @param a the first number\n * @param b the second number\n * @return the sum\n */\nfun add(a: Int, b: Int): Int {\n    return a + b\n}\nprintln(add(1, 2))\n";
        let h = hover_at(text, 7, 5).expect("add should hover");
        let value = markdown_value(&h);
        assert!(value.contains("**Parameters:**"), "{value}");
        assert!(value.contains("`a` — the first number"), "{value}");
        assert!(value.contains("`b` — the second number"), "{value}");
    }

    #[test]
    fn hover_on_a_fun_with_only_param_docs_and_no_summary_still_shows_them() {
        let text = "/**\n * @param n the number\n */\nfun square(n: Int): Int {\n    return n * n\n}\nprintln(square(2))\n";
        let h = hover_at(text, 4, 5).expect("square should hover");
        let value = markdown_value(&h);
        assert!(value.contains("**Parameters:**"), "{value}");
        assert!(value.contains("`n` — the number"), "{value}");
    }

    #[test]
    fn hover_on_a_documented_funs_parameter_shows_its_own_param_text_not_the_summary() {
        let text = "/**\n * Adds two numbers.\n * @param a the first number\n */\nfun add(a: Int, b: Int): Int {\n    return a + b\n}\n";
        // "    return a" -- "a" starts at column 12.
        let h = hover_at(text, 6, 12).expect("a should hover");
        let value = markdown_value(&h);
        assert!(value.contains("the first number"), "{value}");
        assert!(!value.contains("Adds two numbers"), "{value}");
    }

    #[test]
    fn hover_on_a_param_documented_without_star_per_line_prefixes() {
        // Not every doc comment uses the `* ` per-line style the other
        // tests here do -- plain indented text (no leading `*` at all) and
        // non-ASCII content must work identically.
        let text = "/**\n  Factorial.\n  \n  @param n the number\n*/\nfun factorial(n: Int): Int {\n    if (n <= 1) {\n        return 1\n    }\n    return n * factorial(n - 1)\n}\n";
        // Declaration site: "fun factorial(n: Int)" -- "n" at column 15.
        let h_decl = hover_at(text, 6, 15).expect("n should hover at its declaration");
        assert!(markdown_value(&h_decl).contains("the number"), "{}", markdown_value(&h_decl));
        // A use inside the body: "return n * factorial(n - 1)" -- first "n" at column 12.
        let h_use = hover_at(text, 10, 12).expect("n should hover at a use site");
        assert!(markdown_value(&h_use).contains("the number"), "{}", markdown_value(&h_use));
    }

    #[test]
    fn hover_on_a_documented_val_shows_its_summary() {
        let text = "/** The current count. */\nval count = 0\nprintln(count)\n";
        let h = hover_at(text, 2, 5).expect("count should hover");
        assert!(markdown_value(&h).contains("The current count."), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_an_undocumented_fun_shows_no_extra_doc_block() {
        let text = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nprintln(add(1, 2))\n";
        let h = hover_at(text, 1, 5).expect("add should hover");
        assert_eq!(markdown_value(&h), "```ksl\nfun add(a: Int, b: Int): Int\n```\n\n*function*");
    }

    #[test]
    fn hover_on_a_documented_cross_file_function_shows_its_doc() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_hover_doc_cross_file_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("greeter.ksl"),
            "/**\n * Greets someone by name.\n */\nfun greet(name: String): String {\n    return name\n}\n",
        )
        .unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        // "println(" is 8 chars, so "greet" starts at column 9.
        let h = hover(&a, Position::new(1, 8)).expect("greet should hover");
        assert!(markdown_value(&h).contains("Greets someone by name."), "{}", markdown_value(&h));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hover_on_a_flat_cross_file_import_resolves_to_the_other_file() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_hover_flat_test_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("greeter.ksl"), "fun greet(name: String): String {\n    return name\n}\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        // "println(" is 8 chars, so "greet" starts at column 9.
        let h = hover(&a, Position::new(1, 8)).expect("greet should hover");
        assert!(markdown_value(&h).contains("fun greet"), "{}", markdown_value(&h));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Regression test: a real (package-declaring) file's own wildcard
    /// `import a.b.*` used to be invisible to hover's own "no local
    /// declaration at all" fallback — only a *package-less* `@file:Import`'s
    /// flat namespace was ever consulted here.
    #[test]
    fn hover_on_a_name_brought_in_by_a_wildcard_import_of_a_real_package() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_hover_wildcard_pkg_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("greeter.ksl"),
            "package test\nfun greet(name: String): String {\n    return name\n}\n",
        )
        .unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"greeter.ksl\")\nimport test.*\nprintln(greet(\"World\"))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        // "println(" is 8 chars, so "greet" starts at column 9, on line 3.
        let h = hover(&a, Position::new(2, 8)).expect("greet should hover");
        // Fully qualified by its real package, even though it's referenced
        // bare here (via the wildcard `import test.*`) — see
        // `build_hover`'s `package` parameter.
        assert!(markdown_value(&h).contains("fun test.greet"), "{}", markdown_value(&h));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hover_on_a_local_still_resolves_its_declaration() {
        // `s.uppercase()`'s receiver `s` is a real local — hovering `s`
        // itself must still find its declaration, not fall through to the
        // builtin-method branch meant for `uppercase`.
        let text = "val s = \"hi\"\nprintln(s.uppercase())\n";
        let h = hover_at(text, 2, 9).expect("s should hover");
        assert!(markdown_value(&h).contains("val s"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_hintless_local_shows_its_inferred_type() {
        // `count` has no type annotation — hover must still show the type
        // worked out for it from the initializer, not a bare "var count".
        let text = "var count = 0\nprintln(count)\n";
        let h = hover_at(text, 1, 5).expect("count should hover");
        assert!(markdown_value(&h).contains("var count: Int"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_builtin_method_call_on_a_local_receiver() {
        let text = "val s = \"hi\"\nprintln(s.uppercase())\n";
        // "println(s." is 10 chars, so "uppercase" starts at column 11.
        let h = hover_at(text, 2, 11).expect("uppercase should hover");
        assert!(markdown_value(&h).contains("uppercase"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("builtin method"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_builtin_method_call_on_a_string_literal() {
        let text = "println(\"hi\".length())\n";
        // "println(\"hi\"." is 13 chars, so "length" starts at column 14.
        let h = hover_at(text, 1, 14).expect("length should hover");
        assert!(markdown_value(&h).contains("length"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_builtin_method_call_on_a_nullable_string_receiver_still_shows_the_signature() {
        // Regression test: `s`'s static type is `Type::Nullable(String)`,
        // whose `name()` is `"String?"` — the receiver-type match must
        // unwrap one `Nullable` layer, or a nonsensical exact-string
        // comparison against `BUILTIN_METHOD_SIGNATURES`'s bare `"String"`
        // entries would wrongly hide hover text for a perfectly legitimate
        // call on a nullable-typed receiver.
        let text = "fun f(s: String?) {\n    println(s.uppercase())\n}\n";
        // "    println(s." is 14 chars, so "uppercase" starts at column 15.
        let h = hover_at(text, 2, 15).expect("uppercase should hover");
        assert!(markdown_value(&h).contains("uppercase"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("builtin method"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_builtin_method_call_on_a_non_matching_receiver_type_stays_none() {
        // `n` is a real local of type `Int` — `.length()`/`.uppercase()`
        // etc. are `String`-only, so this must not show a misleading
        // "String.uppercase()" hover just because the method name matches.
        let text = "val n: Int = 5\nprintln(n.uppercase())\n";
        // "println(n." is 10 chars, so "uppercase" starts at column 11.
        let h = hover_at(text, 2, 11);
        assert!(h.is_none(), "{h:?}");
    }

    #[test]
    fn hover_on_an_unknown_package_qualified_call_stays_none() {
        // `a.b` never resolves to a real package here, and `greet` isn't a
        // builtin method name — must not show a misleading builtin-method
        // hover for it.
        let h = hover_at("println(a.b.greet(\"x\"))\n", 1, 14);
        assert!(h.is_none());
    }

    #[test]
    fn hover_on_a_fully_qualified_kotlin_test_call_shows_real_signature() {
        // `kotlin.test` is a synthetic package (`package_index`'s built-in
        // seeding, from `stdlib::BUILTIN_PACKAGES`) — before that, this hit
        // exactly the "unknown package" case above (`analysis.packages`
        // had no idea `kotlin.test` existed) and returned `None` too, even
        // though `kotlin.test.assertEquals(...)` is real, valid ksl.
        let h = hover_at("println(kotlin.test.assertEquals(1, 1))\n", 1, 25).expect("should have hover info");
        let text = markdown_value(&h);
        assert!(text.contains("assertEquals"), "{text}");
        // Fully qualified, not just the bare name — see `build_hover`'s
        // `package` parameter.
        assert!(text.contains("kotlin.test.assertEquals"), "{text}");
    }

    #[test]
    fn hover_on_a_package_qualified_call_to_a_real_user_package_shows_the_full_name() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_hover_qualified_pkg_name_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pkg.ksl"), "package util.math\nfun square(x: Int): Int {\n    return x * x\n}\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"pkg.ksl\")\nprintln(util.math.square(5))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        // "println(util.math." is 19 chars, so "square" starts at column 20.
        let h = hover(&a, Position::new(1, 19)).expect("square should hover");
        assert!(markdown_value(&h).contains("fun util.math.square(x: Int): Int"), "{}", markdown_value(&h));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hover_on_a_named_arguments_label_at_a_call_site_shows_its_parameter() {
        let text = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nprintln(add(a = 1, b = 2))\n";
        // "println(add(" is 12 chars, so "a" starts at column 13.
        let h = hover_at(text, 4, 13).expect("the 'a' label should hover");
        let value = markdown_value(&h);
        assert!(value.contains("param a: Int"), "{value}");
        assert!(value.contains("parameter"), "{value}");
        // "println(add(a = 1, " is 19 chars, so "b" starts at column 20.
        let h_b = hover_at(text, 4, 20).expect("the 'b' label should hover");
        assert!(markdown_value(&h_b).contains("param b: Int"), "{}", markdown_value(&h_b));
    }

    #[test]
    fn hover_on_a_named_arguments_label_shows_that_params_own_doc() {
        let text = "/**\n * Adds two numbers.\n * @param a the first number\n */\nfun add(a: Int, b: Int): Int {\n    return a + b\n}\nprintln(add(a = 1, b = 2))\n";
        // "println(add(" is 12 chars, so "a" starts at column 13, on line 8.
        let h = hover_at(text, 8, 13).expect("the 'a' label should hover");
        let value = markdown_value(&h);
        assert!(value.contains("the first number"), "{value}");
        assert!(!value.contains("Adds two numbers"), "{value}");
    }

    #[test]
    fn hover_on_a_positional_arguments_value_is_unaffected_by_named_arg_label_hover() {
        // A positional argument's *value* (`1`, not a label) must keep
        // resolving exactly as before -- it never has a `name_span` of its
        // own, so `named_arg_label_at` must not spuriously claim it.
        let text = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\nprintln(add(1, 2))\n";
        // "println(add(" is 12 chars, so "1" starts at column 13.
        let h = hover_at(text, 4, 13);
        assert!(h.is_none(), "{h:?}");
    }

    #[test]
    fn hover_on_a_promoted_field_use_shows_its_ctor_params_declaration() {
        // A promoted `val`/`var` ctor param has no separate field
        // declaration of its own — `p.x` points back at the *constructor
        // parameter's* own symbol (see `resolver::ClassFieldInfo::
        // symbol_idx`'s doc comment), so this shows "parameter", not
        // "field".
        let text = "class Point(val x: Int, val y: Int)\nval p = Point(1, 2)\nprintln(p.x)\n";
        // "println(p." is 10 chars, so "x" starts at column 11.
        let h = hover_at(text, 3, 11).expect("field use should hover");
        assert!(markdown_value(&h).contains("param x: Int"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("*parameter*"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_body_property_use_shows_a_real_field_declaration() {
        let text = "class Counter {\n    var count: Int = 0\n}\nval c = Counter()\nprintln(c.count)\n";
        // "println(c." is 10 chars, so "count" starts at column 11.
        let h = hover_at(text, 5, 11).expect("field use should hover");
        assert!(markdown_value(&h).contains("var count: Int"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("*field*"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_method_use_shows_its_signature() {
        let text = "class Point(val x: Int) {\n    fun bark(): String = \"woof\"\n}\nval p = Point(1)\np.bark()\n";
        // "p." is 2 chars, so "bark" starts at column 3.
        let h = hover_at(text, 5, 3).expect("method use should hover");
        assert!(markdown_value(&h).contains("fun bark(): String"), "{}", markdown_value(&h));
    }

    /// Cross-file counterpart of `hover_on_a_promoted_field_use_shows_its_ctor_params_declaration`
    /// — a `class` declared in one file, its field used from another via
    /// `@file:Import`, must still describe the real declaration (see
    /// `ResolveResult::cross_file_ref_at`'s own doc comment).
    #[test]
    fn hover_on_a_cross_file_promoted_field_use_shows_its_ctor_params_declaration() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_hover_cross_field_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("point.ksl"), "class Point(val x: Int, val y: Int)\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"point.ksl\")\nval p = Point(1, 2)\nprintln(p.x)\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        // "println(p." is 10 chars, so "x" starts at column 11, on line 3.
        let h = hover(&a, Position::new(2, 10)).expect("cross-file field use should hover");
        assert!(markdown_value(&h).contains("param x: Int"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("*parameter*"), "{}", markdown_value(&h));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Cross-file counterpart of `hover_on_a_method_use_shows_its_signature`.
    #[test]
    fn hover_on_a_cross_file_method_use_shows_its_signature() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_hover_cross_method_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("point.ksl"), "class Point(val x: Int) {\n    fun bark(): String = \"woof\"\n}\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"point.ksl\")\nval p = Point(1)\np.bark()\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        // "p." is 2 chars, so "bark" starts at column 3, on line 3.
        let h = hover(&a, Position::new(2, 2)).expect("cross-file method use should hover");
        assert!(markdown_value(&h).contains("fun bark(): String"), "{}", markdown_value(&h));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hover_on_this_shows_the_enclosing_classs_instance_type() {
        let text = "class Point(val x: Int) {\n    fun getX(): Int = this.x\n}\n";
        // "    fun getX(): Int = " is 22 chars, so "this" starts at column 23.
        let h = hover_at(text, 2, 23).expect("this should hover");
        assert!(markdown_value(&h).contains("this: Point"), "{}", markdown_value(&h));
    }
}

