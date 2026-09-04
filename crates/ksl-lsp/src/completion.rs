use ksl_core::resolver::SymbolKind;
use ksl_core::stdlib::{BUILTIN_METHOD_SIGNATURES, STDLIB_SIGNATURES};
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Documentation, Position};

use crate::analysis::Analysis;

/// From `TokenKind`'s keyword variants (`token.rs`) plus the two boolean
/// literals — not itself a `TokenKind`, but a reserved word from the
/// user's point of view.
const KEYWORDS: &[&str] = &[
    "fun", "val", "var", "if", "else", "when", "while", "for", "in", "return", "break",
    "continue", "true", "false",
];

/// Completion candidates at `position` (0-based, LSP convention) within
/// `text` (needed, alongside `analysis`, to detect a package-qualified
/// prefix already typed before the cursor — see `dotted_prefix_before_cursor`).
///
/// If the cursor sits right after `a.b.`: when `a.b` is a single bare
/// identifier that resolves to a local with a statically known type (e.g.
/// `s` in `s.`, where `s: String`), only that type's builtin methods are
/// offered — the concrete fix for the bug this filtering exists for
/// (offering `.length()`/`.uppercase()` after a receiver that obviously
/// isn't a `String`, e.g. a number). Otherwise (the type is `Unknown`, the
/// name doesn't resolve, or the prefix has multiple segments and so looks
/// package-shaped) there's no way to statically tell a local receiver from
/// a package apart (the same ambiguity `ExprKind::FieldAccess`/`MethodCall`
/// defer to evaluation time, see `docs/syntax.md`'s "Packages" section), so
/// both possibilities are offered together: `a.b`'s members if it's a known
/// package, and every builtin method in case `a.b` is actually a real
/// receiver value — this over-suggests until classes give real per-type
/// member info, but never under-suggests. Otherwise (no `.` prefix at all):
/// locals and outer-scope symbols visible there, every stdlib function,
/// every known package's full name, and language keywords — merged in that
/// order (most relevant first).
pub fn completion(analysis: &Analysis, text: &str, position: Position) -> Vec<CompletionItem> {
    let line_text = text.lines().nth(position.line as usize).unwrap_or("");
    let up_to_cursor: String = line_text.chars().take(position.character as usize).collect();

    // Checked first, ahead of every other (line-local) context below: a
    // KDoc tag (`@param`, `@return`, ...) can only ever appear inside a
    // `/** ... */` doc comment, which is usually multi-line — unlike the
    // other detectors here, this one needs the text from the *start of the
    // document*, not just the current line.
    if kdoc_tag_context(&text_up_to_position(text, position)).is_some() {
        return kdoc_tag_completions();
    }

    if let Some(ctx) = file_import_context(&up_to_cursor) {
        return match ctx {
            FileImportContext::AtHead => file_head_completion(),
            FileImportContext::AfterFileColon => import_keyword_completion(),
            FileImportContext::InsidePath(partial) => file_path_completions(&analysis.own_file, &partial),
        };
    }

    if type_position_before_cursor(&up_to_cursor) {
        return ksl_core::types::PRIMITIVE_TYPE_NAMES
            .iter()
            .map(|name| CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::CLASS),
                ..Default::default()
            })
            .collect();
    }

    let (prefix_path, _partial) = dotted_prefix_before_cursor(&up_to_cursor);

    if !prefix_path.is_empty() {
        // A single-segment prefix resolving to a local with a known
        // concrete type narrows the builtin-method suggestions to that
        // type; anything else (multi-segment, unresolved, or `Unknown`)
        // falls back to the over-suggesting "show everything" behavior.
        // A numeric-literal prefix (`1`, or the two-segment `1.5` shape)
        // is recognized directly — never a real identifier, so a symbol
        // lookup for it would always fail — instead of falling through to
        // the unfiltered case.
        let receiver_type = match prefix_path.as_slice() {
            [int_part] if is_digits(int_part) => Some(ksl_core::types::Type::Int),
            [int_part, frac_part] if is_digits(int_part) && is_digits(frac_part) => {
                Some(ksl_core::types::Type::Double)
            }
            [name] => {
                let line = position.line + 1;
                let col = position.character + 1;
                analysis
                    .resolved
                    .table
                    .symbols_in_scope(line, col)
                    .into_iter()
                    .find(|s| s.name == *name)
                    .map(|s| s.ty.clone())
                    .filter(|ty| *ty != ksl_core::types::Type::Unknown)
            }
            _ => None,
        };

        let package_name = prefix_path.join(".");
        let mut items: Vec<CompletionItem> = analysis
            .packages
            .members_of(&package_name)
            .iter()
            .map(|m| CompletionItem {
                label: m.symbol.name.clone(),
                kind: Some(symbol_completion_kind(m.symbol.kind)),
                detail: Some(m.symbol.signature.clone()),
                documentation: m.symbol.doc.clone().map(Documentation::String),
                ..Default::default()
            })
            .collect();
        items.extend(
            BUILTIN_METHOD_SIGNATURES
                .iter()
                .filter(|(recv_ty, ..)| match &receiver_type {
                    Some(ty) => ty.name() == *recv_ty,
                    None => true,
                })
                .map(|(_, name, signature, doc)| CompletionItem {
                    label: name.to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some(signature.to_string()),
                    documentation: Some(Documentation::String(doc.to_string())),
                    ..Default::default()
                }),
        );
        return items;
    }

    let line = position.line + 1;
    let col = position.character + 1;
    let mut items = Vec::new();

    for symbol in analysis.resolved.table.symbols_in_scope(line, col) {
        items.push(CompletionItem {
            label: symbol.name.clone(),
            kind: Some(symbol_completion_kind(symbol.kind)),
            detail: Some(symbol.signature.clone()),
            documentation: symbol.doc.clone().map(Documentation::String),
            ..Default::default()
        });
    }

    for (name, signature, doc) in STDLIB_SIGNATURES {
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(signature.to_string()),
            documentation: Some(Documentation::String(doc.to_string())),
            ..Default::default()
        });
    }

    for package_name in analysis.packages.package_names() {
        items.push(CompletionItem {
            label: package_name.clone(),
            kind: Some(CompletionItemKind::MODULE),
            ..Default::default()
        });
    }

    // Top-level names of a package-less `@file:Import`ed file — flattened
    // into this file's own global scope at runtime (see
    // `package_index::PackageIndex::flat_member`'s doc comment), so they're
    // offered right alongside this file's own locals, not under any
    // package-qualified prefix.
    for member in analysis.packages.flat_members() {
        items.push(CompletionItem {
            label: member.symbol.name.clone(),
            kind: Some(symbol_completion_kind(member.symbol.kind)),
            detail: Some(member.symbol.signature.clone()),
            documentation: member.symbol.doc.clone().map(Documentation::String),
            ..Default::default()
        });
    }

    for kw in KEYWORDS {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    items
}

/// Splits the text immediately before the cursor into a dotted-path prefix
/// (if any) and the partial identifier currently being typed — e.g.
/// `"util.math.sq"` -> (`["util", "math"]`, `"sq"`); `"util.math."` ->
/// (`["util", "math"]`, `""`); `"sq"` -> (`[]`, `"sq"`).
///
/// A qualified-name expression like `a.b.c` doesn't exist as an AST node
/// while it's mid-typed (the parser requires a complete identifier after
/// each `.`, so `a.b.` alone fails to parse at all) — so, unlike every
/// other completion source here, this can't be read off `analysis` and has
/// to look at the raw text instead. Operates on `char`s, matching how the
/// rest of this codebase already treats `Position::character` as a plain
/// character count rather than a true UTF-16 code unit count (see
/// `diagnostics::span_to_range`) — a pre-existing simplification, not a
/// new one introduced here.
fn dotted_prefix_before_cursor(text_before_cursor: &str) -> (Vec<String>, String) {
    let mut start = text_before_cursor.len();
    for (i, c) in text_before_cursor.char_indices().rev() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            start = i;
        } else {
            break;
        }
    }
    let chunk = &text_before_cursor[start..];
    // A chunk starting with a digit is a numeric literal (`1`, `1.5`, ...),
    // never a dotted *name* chain — identifiers can't start with a digit
    // (see `lexer.rs`'s `lex_ident_or_keyword` vs `lex_number`). It's
    // still returned as an ordinary prefix, though (not rejected/emptied
    // out): `completion()` recognizes an all-digit segment as a numeric
    // literal and assigns it a real `Int`/`Double` type directly (no
    // symbol to look up), so the existing type-aware filtering still
    // applies — an earlier version of this function instead returned an
    // empty prefix here, which fell through to the *general* completion
    // branch (locals/stdlib/keywords) and still offered `length`/etc. as
    // plain stdlib functions right after a number, just not flagged as
    // `.`-methods — the visible bug this comment now guards against.
    let mut parts: Vec<String> = chunk.split('.').map(String::from).collect();
    let partial = parts.pop().unwrap_or_default();
    (parts, partial)
}

/// Whether the cursor sits in type-annotation position: right after a `:`
/// or `->`, or anywhere inside the (possibly nested) type that follows one
/// — including inside a function type's own `(...)` parameter list, e.g.
/// `val f: (I` or `val f: (Int) -> I`. Scans the whole line up to the
/// cursor (every `:` in the grammar introduces a type — `val`/`var`/
/// parameter annotations and `fun` return types, `parser.rs`'s only three
/// `Colon` call sites — so hitting one anywhere is unambiguous), tracking
/// a single `in_type` flag plus a stack recording, for each currently-open
/// `(`, whether it's a function type's own parameter list (`TypeList` —
/// entered while already in type position, so its contents start in type
/// position too, and a `,` inside it stays in type position for the next
/// parameter) or an ordinary call/`fun`-decl parameter list (`NameList` —
/// entered *not* in type position, so its contents start out awaiting a
/// name/argument, and a `,` inside it goes back to that, not to type
/// position). The `NameList` case is why `fun f(a: Int, b` correctly does
/// *not* offer type completions while typing `b`'s name, even though an
/// earlier `:` already appeared on the same line — without tracking which
/// kind of list we're in, that earlier `:` alone would be indistinguishable
/// from actually still being inside `a`'s own type. No special handling is
/// needed for `->` itself: it's inert to this state machine, so `in_type`
/// simply stays whatever it already was on either side of it, which is
/// exactly right (a function type continues being "type position" straight
/// through its own arrow).
fn type_position_before_cursor(text_before_cursor: &str) -> bool {
    #[derive(Clone, Copy)]
    enum ParenKind {
        /// A function type's own parameter list — bare types only, no
        /// names (`(Int, String) -> Result`).
        TypeList,
        /// An ordinary call's arguments or a `fun`-decl's own parameter
        /// list — `name: Type` pairs, or bare argument expressions.
        NameList,
    }

    let mut in_type = false;
    let mut stack: Vec<ParenKind> = Vec::new();
    for c in text_before_cursor.chars() {
        match c {
            ':' => in_type = true,
            '=' | '{' => in_type = false,
            ',' => in_type = matches!(stack.last(), Some(ParenKind::TypeList)),
            '(' => {
                let kind = if in_type { ParenKind::TypeList } else { ParenKind::NameList };
                stack.push(kind);
                in_type = matches!(kind, ParenKind::TypeList);
            }
            ')' => in_type = matches!(stack.pop(), Some(ParenKind::TypeList)),
            _ => {}
        }
    }
    in_type
}

/// `true` for a non-empty run of ASCII digits — used to recognize a
/// dotted-prefix segment that's actually (part of) a numeric literal
/// rather than an identifier, so its type can be assigned directly
/// (`Int`, or `Double` for the two-segment `N.M` shape) instead of
/// attempting — and always failing — a symbol lookup for something that
/// was never a legal identifier to begin with.
fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn symbol_completion_kind(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::Val | SymbolKind::Var | SymbolKind::Param => CompletionItemKind::VARIABLE,
        SymbolKind::Fun => CompletionItemKind::FUNCTION,
    }
}

/// Where the cursor sits within an in-progress `@file:Import("...")` —
/// doesn't exist as an AST node while mid-typed (same reason
/// `dotted_prefix_before_cursor` operates on raw text: an incomplete
/// `@file:Import("` fails to parse at all), so this is a raw-text
/// recognizer, same approach as `type_position_before_cursor`.
enum FileImportContext {
    /// Right after `@` plus a partial prefix of `file` — e.g. `@f`, `@fil`.
    AtHead,
    /// Right after `@file:` plus a partial prefix of `Import` — e.g.
    /// `@file:Im`.
    AfterFileColon,
    /// Inside the path string's quotes, not yet closed — the partial path
    /// text typed so far (e.g. `"` -> `""`, `"imp` -> `"imp"`).
    InsidePath(String),
}

/// Recognizes the three `@file:Import("...")` positions above by looking
/// for the *longest* (most specific) matching suffix first — checking
/// `InsidePath` before `AfterFileColon` before `AtHead` matters because
/// each shorter pattern is also a substring of the longer ones (`@file:`
/// appears inside `@file:Import("`, and `@` inside `@file:`), so checking
/// in the other order would misfire on the shorter pattern before ever
/// reaching the real, more specific position.
fn file_import_context(up_to_cursor: &str) -> Option<FileImportContext> {
    if let Some(idx) = up_to_cursor.rfind("@file:Import(\"") {
        let after = &up_to_cursor[idx + "@file:Import(\"".len()..];
        if !after.contains('"') {
            return Some(FileImportContext::InsidePath(after.to_string()));
        }
    }
    if let Some(idx) = up_to_cursor.rfind("@file:") {
        let after = &up_to_cursor[idx + "@file:".len()..];
        // Empty `after` (cursor right on the `:`, nothing typed yet) must
        // still match — `"".chars().all(...)` is vacuously `true`, so no
        // separate `is_empty` guard is needed; the `@` branch below already
        // relies on the same vacuous-true behavior for its own empty case,
        // this just needed to match it.
        if after.chars().all(|c| c.is_ascii_alphabetic()) {
            return Some(FileImportContext::AfterFileColon);
        }
    }
    if let Some(idx) = up_to_cursor.rfind('@') {
        let after = &up_to_cursor[idx + 1..];
        if after.chars().all(|c| c.is_ascii_alphabetic()) {
            return Some(FileImportContext::AtHead);
        }
    }
    None
}

fn file_head_completion() -> Vec<CompletionItem> {
    vec![CompletionItem {
        label: "file".to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    }]
}

fn import_keyword_completion() -> Vec<CompletionItem> {
    vec![CompletionItem {
        label: "Import".to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        documentation: Some(Documentation::String(
            "the only supported `@file:` annotation".to_string(),
        )),
        ..Default::default()
    }]
}

/// File/directory completions for the partial path typed so far inside an
/// `@file:Import("...")` string, resolved the same way a real import path
/// is (`ksl_core::importer::resolve_path`, relative to `own_file`'s own
/// directory). Lists the directory implied by everything up to the last
/// `/` in `partial` (or `own_file`'s own directory, if there is no `/`
/// yet) — every entry in it, not just ones matching what's typed after
/// that: like every other completion source in this file, filtering
/// against the in-progress prefix is left to the editor/client, not done
/// here (see this module's own doc comment). A directory entry's label
/// carries a trailing `/` (so accepting it and continuing to type
/// naturally descends further); only `.ksl` files are offered (the only
/// files `@file:Import` can legally name), but every subdirectory is,
/// hidden ones (dotfiles) aside.
fn file_path_completions(own_file: &std::path::Path, partial: &str) -> Vec<CompletionItem> {
    let base_dir = own_file.parent().unwrap_or_else(|| std::path::Path::new("."));
    let dir_part = match partial.rfind('/') {
        Some(idx) => &partial[..idx],
        None => "",
    };
    let dir = ksl_core::importer::resolve_path(base_dir, dir_part);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };

    let mut items = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            items.push(CompletionItem {
                label: format!("{name}/"),
                kind: Some(CompletionItemKind::FOLDER),
                ..Default::default()
            });
        } else if name.ends_with(".ksl") {
            items.push(CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::FILE),
                ..Default::default()
            });
        }
    }
    items
}

/// All full lines of `text` up to `position.line`, plus the current line
/// truncated to `position.character` — unlike every other completion
/// context in this file (which only ever look at the current line), KDoc
/// tag detection needs the whole document up to the cursor, since a doc
/// comment is usually multi-line.
fn text_up_to_position(text: &str, position: Position) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i as u32 > position.line {
            break;
        }
        if i as u32 == position.line {
            out.extend(line.chars().take(position.character as usize));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Whether the cursor sits inside a still-open `/** ... */` doc comment,
/// right after an `@` (optionally with a partial tag name already typed,
/// e.g. `@par` while typing `@param`) — returns that partial text (`""` if
/// nothing's typed yet after the `@`). Finds the *last* `/**` before the
/// cursor; if a `*/` already closes it before the cursor too, we're not
/// inside it anymore. Known, harmless imprecision (in the spirit of this
/// file's other text-only heuristics): an empty `/**/` reads as "still
/// open" here, since this is a plain substring check, not the lexer's own
/// (correct) open/close scan — see `lexer.rs`'s doc comment on exactly
/// that distinction. Doesn't overlap with `file_import_context`: an `@`
/// a real KDoc comment would realistically contain never looks like
/// `@file:Import(...)`'s own shape, and vice versa.
fn kdoc_tag_context(text_up_to_cursor: &str) -> Option<String> {
    let last_open = text_up_to_cursor.rfind("/**")?;
    let after_open = &text_up_to_cursor[last_open + 3..];
    if after_open.contains("*/") {
        return None;
    }
    let last_at = after_open.rfind('@')?;
    let partial = &after_open[last_at + 1..];
    partial.chars().all(|c| c.is_ascii_alphabetic()).then(|| partial.to_string())
}

/// Every KDoc block tag (kotlinlang.org/docs/kotlin-doc.html) this project
/// recognizes — see `doc_comment.rs` for which ones are actually rendered
/// in hover today (`@param`/`@return`); the rest are still offered here so
/// completion doesn't undersell what's valid to write, even before a later
/// phase starts rendering them too.
const KDOC_TAGS: &[(&str, &str)] = &[
    ("param", "Documents a value parameter of a function."),
    ("return", "Documents the return value of a function."),
    ("constructor", "Documents the primary constructor of a class."),
    ("receiver", "Documents the receiver of an extension function or property."),
    ("property", "Documents a property of a class."),
    ("throws", "Documents an exception that can be thrown by a function."),
    ("exception", "Alias for `@throws`."),
    ("sample", "Embeds the body of a function into the documentation, as an example."),
    ("see", "Adds a link to another symbol, or to no symbol for a general link."),
    ("author", "Documents the author of the element."),
    ("since", "Documents the version this element was introduced in."),
    ("suppress", "Excludes an element from the generated documentation."),
];

fn kdoc_tag_completions() -> Vec<CompletionItem> {
    KDOC_TAGS
        .iter()
        .map(|(name, doc)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            documentation: Some(Documentation::String(doc.to_string())),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;
    use std::path::Path;

    fn labels(text: &str, position: Position) -> Vec<String> {
        let analysis = analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).expect("should parse");
        completion(&analysis, text, position).into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn includes_locals_stdlib_and_keywords() {
        let text = "val outer = 1\nfun f(p: Int): Int {\n    val inner = 2\n    return inner\n}\n";
        // position inside f's body, on the `return inner` line (0-based line 3).
        let names = labels(text, Position::new(3, 4));
        assert!(names.contains(&"inner".to_string()));
        assert!(names.contains(&"p".to_string()));
        assert!(names.contains(&"f".to_string()));
        assert!(names.contains(&"outer".to_string()));
        assert!(names.contains(&"println".to_string()));
        assert!(names.contains(&"fun".to_string()));
    }

    #[test]
    fn falls_back_to_last_good_tree_when_mid_typing() {
        let good = "fun f(p: Int): Int {\n    return p\n}\n";
        let path = Path::new("/tmp/nonexistent_test.ksl");
        let analysis = analysis::analyze(good, None, path).unwrap();
        assert!(!analysis.resolved.table.symbols_in_scope(2, 5).is_empty());

        let broken = format!("{good}val z = \n");
        let last_good = std::sync::Arc::new(ksl_core::parse_only(good).unwrap());
        let fallback = analysis::analyze(&broken, Some(last_good), path).expect("should fall back");
        assert!(!fallback.is_current);
        let names: Vec<String> =
            completion(&fallback, &broken, Position::new(1, 4)).into_iter().map(|i| i.label).collect();
        assert!(names.contains(&"p".to_string()));
    }

    #[test]
    fn completion_after_a_dot_offers_builtin_methods() {
        // `s` is a local `String` — its builtin methods are offered after
        // `.`. Uses a fully-typed line (`dotted_prefix_before_cursor` only
        // reads up to `position`, so the cursor placed right after "s."
        // still exercises the same prefix detection `s.` alone would,
        // without needing an incomplete-parse fallback tree).
        let text = "val s = \"hi\"\ns.uppercase()\n";
        let names = labels(text, Position::new(1, 2));
        assert!(names.contains(&"uppercase".to_string()), "{names:?}");
        assert!(names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_a_non_string_local_does_not_offer_string_methods() {
        // Regression test for the bug this filtering exists to fix: `n` is
        // a local of a statically known, non-`String` type — `.length()`/
        // `.uppercase()` etc. must not be offered for it.
        let text = "val n: Int = 5\nn.uppercase()\n";
        let names = labels(text, Position::new(1, 2));
        assert!(!names.contains(&"uppercase".to_string()), "{names:?}");
        assert!(!names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_an_int_literal_offers_nothing_string_shaped() {
        // The exact bug report: `1.length()` — `1` is an `Int` literal
        // (recognized directly, no symbol lookup involved), so no
        // `String` method, and nothing from the general branch either.
        let text = "1.length()\n";
        let names = labels(text, Position::new(0, 2)); // right after "1."
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn completion_after_a_numeric_literal_dot_does_not_offer_string_methods() {
        // Regression test for the real bug: right after `1.` (an `Int`
        // literal), nothing offered may be a `String` method — and,
        // critically, general completion items (locals, stdlib functions
        // as plain names, keywords) must not leak in either. An earlier
        // version of this fix only blocked the `.`-method-table branch,
        // which made the function fall through to the general branch —
        // `length` etc. still appeared in the list (just as a plain
        // stdlib function instead of a flagged `.`-method), which reads
        // identically to "1.length() is offered" in an editor.
        let text = "println(1.5)\n";
        let analysis = analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).expect("should parse");
        let items = completion(&analysis, text, Position::new(0, 10)); // right after "println(1."
        let labels: Vec<&String> = items.iter().map(|i| &i.label).collect();
        assert!(!labels.iter().any(|l| *l == "length"), "{labels:?}");
        assert!(!labels.iter().any(|l| *l == "uppercase"), "{labels:?}");
        assert!(!labels.iter().any(|l| *l == "fun"), "{labels:?}");
    }

    #[test]
    fn completion_after_at_offers_file_keyword() {
        // `@f` alone doesn't parse, so `completion()` is called directly
        // against a separate, validly-parsed `Analysis` — the `AtHead`
        // branch never touches `analysis.resolved` anyway (see
        // `file_import_context`'s doc comment).
        let a = analysis::analyze("println(1)\n", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let names: Vec<String> = completion(&a, "@f\n", Position::new(0, 2)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["file".to_string()]);
    }

    #[test]
    fn completion_after_file_colon_offers_import_keyword() {
        let a = analysis::analyze("println(1)\n", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let names: Vec<String> =
            completion(&a, "@file:Im\n", Position::new(0, 8)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["Import".to_string()]);
    }

    #[test]
    fn completion_right_after_file_colon_with_nothing_typed_yet_offers_import_keyword() {
        // The exact reported bug: right after typing `@file:`, before any
        // letter of "Import" — must still offer it, not just once a prefix
        // of "Import" has already been typed.
        let a = analysis::analyze("println(1)\n", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let names: Vec<String> =
            completion(&a, "@file:\n", Position::new(0, 6)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["Import".to_string()]);
    }

    #[test]
    fn completion_inside_the_import_path_string_offers_files_and_directories() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_completion_import_path_{n}_{}", std::process::id()));
        fs::create_dir_all(dir.join("helpers")).unwrap();
        fs::write(dir.join("greeter.ksl"), "fun greet(): String = \"hi\"\n").unwrap();
        fs::write(dir.join("notes.txt"), "not a script").unwrap();
        let main_path = dir.join("main.ksl");
        // A complete, valid file on disk (so `analyze` has a real tree to
        // work with) — `completion()`'s own `text` argument below is a
        // separate, mid-typed line; only `analysis.own_file` (the path,
        // not its contents) matters to this particular completion branch.
        fs::write(&main_path, "@file:Import(\"x.ksl\")\nprintln(1)\n").unwrap();
        let a = analysis::analyze("@file:Import(\"x.ksl\")\nprintln(1)\n", None, &main_path).expect("should parse");

        let mid_typing = "@file:Import(\"\n";
        let items = completion(&a, mid_typing, Position::new(0, 14)); // right after the opening quote
        let names: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
        assert!(names.contains(&"greeter.ksl".to_string()), "{names:?}");
        assert!(names.contains(&"helpers/".to_string()), "{names:?}");
        assert!(!names.contains(&"notes.txt".to_string()), "{names:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_import_context_recognizes_all_three_positions() {
        assert!(matches!(file_import_context("@f"), Some(FileImportContext::AtHead)));
        assert!(matches!(file_import_context("@"), Some(FileImportContext::AtHead)));
        // Right on the colon, nothing typed yet -- must still offer
        // "Import" (this was the reported bug: an `is_empty` guard here
        // meant `@file:` alone matched neither position).
        assert!(matches!(file_import_context("@file:"), Some(FileImportContext::AfterFileColon)));
        assert!(matches!(file_import_context("@file:Im"), Some(FileImportContext::AfterFileColon)));
        assert!(matches!(file_import_context("@file:Import(\"a/b"), Some(FileImportContext::InsidePath(p)) if p == "a/b"));
        // Already closed -- neither position applies anymore.
        assert!(file_import_context("@file:Import(\"a.ksl\")").is_none());
        assert!(file_import_context("println(").is_none());
    }

    #[test]
    fn completion_offers_a_flat_cross_file_imports_top_level_names() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_completion_flat_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("greeter.ksl"), "fun greet(name: String): String {\n    return name\n}\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        let names = completion(&a, text, Position::new(1, 8)).into_iter().map(|i| i.label).collect::<Vec<_>>();
        assert!(names.contains(&"greet".to_string()), "{names:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_after_at_inside_an_open_doc_comment_offers_kdoc_tags() {
        let a = analysis::analyze("println(1)\n", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let text = "/**\n * @\n */\nfun f() {}\n";
        // " * @" is 4 chars -- cursor right after the "@".
        let names: Vec<String> = completion(&a, text, Position::new(1, 4)).into_iter().map(|i| i.label).collect();
        assert!(names.contains(&"param".to_string()), "{names:?}");
        assert!(names.contains(&"return".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_partial_tag_name_still_offers_the_full_kdoc_tag_list() {
        let a = analysis::analyze("println(1)\n", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let text = "/**\n * @par\n */\nfun f() {}\n";
        // " * @par" is 7 chars -- cursor right after "par".
        let names: Vec<String> =
            completion(&a, text, Position::new(1, 7)).into_iter().map(|i| i.label).collect();
        assert!(names.contains(&"param".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_at_in_a_closed_doc_comment_does_not_offer_kdoc_tags() {
        let a = analysis::analyze("println(1)\n", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let text = "/** already closed */\n@file:Import(\"x.ksl\")\n";
        // Cursor right after the "@" of "@file:Import" on line 2 -- must be
        // recognized as the file-import head, not confused with kdoc tags.
        let names: Vec<String> =
            completion(&a, text, Position::new(1, 1)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["file".to_string()]);
    }

    #[test]
    fn completion_after_at_outside_any_doc_comment_does_not_offer_kdoc_tags() {
        let a = analysis::analyze("println(1)\n", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let names: Vec<String> =
            completion(&a, "@f\n", Position::new(0, 2)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["file".to_string()], "must still resolve to the file-import head, not kdoc tags");
    }

    #[test]
    fn kdoc_tag_context_recognizes_the_open_close_and_outside_cases() {
        assert!(kdoc_tag_context("/**\n * @").is_some());
        assert_eq!(kdoc_tag_context("/**\n * @par").unwrap(), "par");
        assert!(kdoc_tag_context("/** closed */\n@").is_none());
        assert!(kdoc_tag_context("@file:Import(\"x\")").is_none());
        assert!(kdoc_tag_context("").is_none());
    }

    #[test]
    fn completion_includes_documentation_for_a_documented_local_symbol() {
        let text = "/** The current count. */\nval count = 0\nprintln(count)\n";
        let a = analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let items = completion(&a, text, Position::new(2, 5));
        let count = items.iter().find(|i| i.label == "count").expect("count should be offered");
        match &count.documentation {
            Some(Documentation::String(s)) => assert!(s.contains("The current count."), "{s}"),
            other => panic!("expected documentation, got {other:?}"),
        }
    }

    #[test]
    fn completion_has_no_documentation_for_an_undocumented_local_symbol() {
        let text = "val count = 0\nprintln(count)\n";
        let a = analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let items = completion(&a, text, Position::new(1, 5));
        let count = items.iter().find(|i| i.label == "count").expect("count should be offered");
        assert!(count.documentation.is_none());
    }

    #[test]
    fn dotted_prefix_before_cursor_returns_a_numeric_prefix_as_is() {
        // Not rejected/emptied out (see the function's doc comment) — a
        // numeric segment is returned like any other, so `completion()`
        // can recognize it and assign a real type directly.
        assert_eq!(dotted_prefix_before_cursor("1."), (vec!["1".to_string()], String::new()));
        assert_eq!(dotted_prefix_before_cursor("1.5"), (vec!["1".to_string()], "5".to_string()));
        assert_eq!(dotted_prefix_before_cursor("println(1."), (vec!["1".to_string()], String::new()));
    }

    #[test]
    fn completion_in_type_annotation_position_offers_primitive_type_names() {
        // The cursor sits mid-line, right after "val x: " — the rest of
        // the (fully valid) line is irrelevant to `completion()`, which
        // only ever looks at text up to `position`.
        let text = "val x: Int = 0\n";
        let names = labels(text, Position::new(0, 7));
        assert!(names.contains(&"Int".to_string()), "{names:?}");
        assert!(names.contains(&"String".to_string()), "{names:?}");
        assert!(!names.contains(&"println".to_string()), "{names:?}");
        assert!(!names.contains(&"fun".to_string()), "{names:?}");
    }

    #[test]
    fn completion_mid_typing_a_type_name_still_offers_primitive_type_names() {
        let text = "val x: Int = 0\n";
        let names = labels(text, Position::new(0, 8)); // right after "val x: I"
        assert!(names.contains(&"Int".to_string()), "{names:?}");
    }

    #[test]
    fn completion_in_a_function_parameter_type_position_offers_primitive_type_names() {
        let text = "fun f(p: Int): Int {\n    return p\n}\n";
        let names = labels(text, Position::new(0, 9)); // right after "fun f(p: "
        assert!(names.contains(&"Int".to_string()), "{names:?}");
    }

    #[test]
    fn type_position_before_cursor_rejects_non_type_positions() {
        assert!(!type_position_before_cursor("val x = "));
        assert!(!type_position_before_cursor("println("));
        assert!(!type_position_before_cursor(""));
    }

    #[test]
    fn type_position_before_cursor_inside_a_function_types_own_parens() {
        assert!(type_position_before_cursor("val f: ("));
        assert!(type_position_before_cursor("val f: (I"));
    }

    #[test]
    fn type_position_before_cursor_after_a_function_types_comma() {
        // A second parameter inside a function type's own list is still a
        // type (`(Int, String) -> ...`), unlike a `fun`-decl's own comma.
        assert!(type_position_before_cursor("val f: (Int, "));
    }

    #[test]
    fn type_position_before_cursor_after_the_arrow() {
        assert!(type_position_before_cursor("val f: (Int) -> "));
        assert!(type_position_before_cursor("val f: (Int) -> I"));
    }

    #[test]
    fn type_position_before_cursor_handles_nested_function_types() {
        assert!(type_position_before_cursor("val f: ((Int) -> Int) -> "));
        assert!(type_position_before_cursor("val f: ((I"));
    }

    #[test]
    fn type_position_before_cursor_rejects_a_subsequent_params_name() {
        // The bug a naive "any `:` on this line" check would have: typing
        // `b`'s *name* right after `a`'s type must not offer type
        // completions just because an earlier `:` appeared on the line.
        assert!(!type_position_before_cursor("fun f(a: Int, b"));
        // But `b`'s own type position, right after, is correctly detected.
        assert!(type_position_before_cursor("fun f(a: Int, b: I"));
    }

    #[test]
    fn type_position_before_cursor_rejects_inside_a_default_values_call() {
        assert!(!type_position_before_cursor("fun f(a: Int = compute("));
    }

    #[test]
    fn completion_inside_a_function_types_parens_offers_primitive_type_names() {
        let text = "val f: (Int) -> Int = { x -> x + 1 }\n";
        let names = labels(text, Position::new(0, 8)); // right after "val f: ("
        assert!(names.contains(&"Int".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_the_arrow_offers_primitive_type_names() {
        let text = "val f: (Int) -> Int = { x -> x + 1 }\n";
        let names = labels(text, Position::new(0, 16)); // right after "val f: (Int) -> "
        assert!(names.contains(&"Int".to_string()), "{names:?}");
        assert!(!names.contains(&"fun".to_string()), "{names:?}");
    }

    #[test]
    fn dotted_prefix_before_cursor_splits_package_and_partial() {
        assert_eq!(
            dotted_prefix_before_cursor("util.math.sq"),
            (vec!["util".to_string(), "math".to_string()], "sq".to_string())
        );
        assert_eq!(
            dotted_prefix_before_cursor("util.math."),
            (vec!["util".to_string(), "math".to_string()], String::new())
        );
        assert_eq!(dotted_prefix_before_cursor("sq"), (vec![], "sq".to_string()));
        assert_eq!(dotted_prefix_before_cursor(""), (vec![], String::new()));
        // Only the trailing identifier-chain matters, not the whole line.
        assert_eq!(
            dotted_prefix_before_cursor("println(util.math.sq"),
            (vec!["util".to_string(), "math".to_string()], "sq".to_string())
        );
    }
}
