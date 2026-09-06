use ksl_core::error::{Expected, IdentRole};
use ksl_core::resolver::SymbolKind;
use ksl_core::stdlib::{format_signature, BUILTIN_METHODS};
use ksl_core::token::TokenKind;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Documentation, Position};

use crate::analysis::Analysis;


/// Completion candidates at `position` (0-based, LSP convention) within
/// `text` (needed, alongside `analysis`, to detect a package-qualified
/// prefix already typed before the cursor — see `dotted_prefix_before_cursor`).
///
/// Inside an in-progress bare `import a.b.c`/`import a.b.*` (see
/// `import_use_context`): delegates to `package_path_completions` with the
/// builtin-method fallback disabled — an import path is never a value
/// expression, so real package members and next-segment names (see that
/// function's own doc comment) are the only things that can legally follow.
///
/// Otherwise, if the cursor sits right after `a.b.`: also
/// `package_path_completions`, this time with the builtin-method fallback
/// enabled — when `a.b` is a single bare identifier resolving to a local
/// with a statically known type (e.g. `s` in `s.`, where `s: String`), only
/// that type's builtin methods are offered on top of any package match (the
/// concrete fix for the bug this filtering exists for: offering
/// `.length()`/`.uppercase()` after a receiver that obviously isn't a
/// `String`, e.g. a number); otherwise builtin methods are only shown when
/// `a.b` doesn't match or prefix any known package at all (see
/// `package_path_completions`'s own doc comment for why — this is what
/// stops an unrelated `length`/`uppercase`/... from polluting completion
/// after a real package-qualified prefix like `kotlin.test.` or a real
/// user package).
///
/// Otherwise (no `.` prefix at all): locals and outer-scope symbols visible
/// there, every stdlib function, every known package's full name (real and
/// built-in alike — see `package_index`'s module doc comment), every name
/// this file's own `import`/`@file:Import` brought into bare scope (real
/// or built-in package, wildcard or named, package-less flat namespace —
/// see `resolver::ResolveResult::imported_symbols`), and language
/// keywords — merged in that order (most relevant first).
pub fn completion(analysis: &Analysis, text: &str, position: Position) -> Vec<CompletionItem> {
    let line_text = text.lines().nth(position.line as usize).unwrap_or("");
    let up_to_cursor: String = line_text.chars().take(position.character as usize).collect();
    let full_up_to_cursor = text_up_to_position(text, position);

    // Checked first, ahead of every other (line-local) context below, and
    // ahead of `kdoc_tag_context` too: an `@param` tag's own parameter-name
    // position (`@param <cursor>`) would otherwise still match nothing
    // below and fall through to ordinary expression-position completion —
    // offering the documented function's own name and every other
    // global-scope symbol instead of its parameter names.
    if let Some(comment_so_far) = kdoc_param_name_context(&full_up_to_cursor) {
        let already_documented = kdoc_already_documented_param_names(comment_so_far);
        return kdoc_param_name_completions(analysis, position, &already_documented);
    }

    // A KDoc tag (`@param`, `@return`, ...) can only ever appear inside a
    // `/** ... */` doc comment, which is usually multi-line — unlike the
    // other detectors here, this one needs the text from the *start of the
    // document*, not just the current line.
    if kdoc_tag_context(&full_up_to_cursor).is_some() {
        return kdoc_tag_completions();
    }

    // What the real parser would need next, found by re-parsing the text
    // truncated exactly at the cursor and reading off what the resulting
    // error (if any) was expecting — see `ksl_core::completion_probe`'s
    // module doc comment. This is the "mid-typing an incomplete construct"
    // half of context detection; facts about an *already-complete*
    // construct (inside a loop, before the file's header ends, right
    // after a dangling `if`) are read straight off `analysis.resolved`
    // instead — see `keyword_allowed`.
    let expected = ksl_core::completion_probe::expected_at_cursor(&full_up_to_cursor);

    if let Some(ctx) = file_import_context(&up_to_cursor) {
        match ctx {
            // `@file:Import(...)` is governed by the same header-only
            // ordering rule as `import a.b.c`/`package` — the real parser
            // only ever recognizes `@`/`file`/`Import` as this construct
            // while still in the file's header (see
            // `parser::Parser::parse_imports`); past that point a stray
            // `@` fails with a free-text "unexpected token" error instead,
            // which never populates `expected` at all, so this check
            // naturally comes back empty there with no separate position
            // check needed. Once a path string is already being typed
            // (`InsidePath`), though, still finishing it out is harmless
            // (and still useful) regardless of position.
            FileImportContext::AtHead if expected.contains(&Expected::Identifier(IdentRole::FileImportHead)) => {
                return file_head_completion();
            }
            FileImportContext::AfterFileColon
                if expected.contains(&Expected::Identifier(IdentRole::FileImportAnnotationName)) =>
            {
                return import_keyword_completion();
            }
            FileImportContext::InsidePath(partial) => {
                return file_path_completions(&analysis.own_file, &partial);
            }
            _ => {}
        }
    }

    if import_use_context(&up_to_cursor) {
        let (prefix_path, _partial) = dotted_prefix_before_cursor(&up_to_cursor);
        return package_path_completions(analysis, &prefix_path, None, false);
    }

    if expected.contains(&Expected::Identifier(IdentRole::TypeName)) {
        return ksl_core::types::PRIMITIVE_TYPES
            .iter()
            .map(|(name, _)| CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::CLASS),
                ..Default::default()
            })
            .collect();
    }

    let (prefix_path, _partial) = dotted_prefix_before_cursor(&up_to_cursor);

    if !prefix_path.is_empty() {
        // A known concrete receiver type narrows the builtin-method
        // suggestions to it; anything else (multi-segment, unresolved, or
        // `Unknown`) falls back to the over-suggesting "show everything"
        // behavior. A numeric-literal prefix (`1`, or the two-segment
        // `1.5` shape) is recognized directly, via the raw-text
        // `prefix_path` itself — the *one* case that genuinely can't be
        // answered from the AST: a token like `1.5` is a single, already
        // fully-typed `DoubleLit`, and the query position can land
        // *inside* it (not past its own end) when the cursor sits right
        // after the `.` in `1.<something>`, so there's no "expression
        // ending here" to look up at all. Every other shape — a bare
        // local, a constructor call, a function call's result, a chained
        // `a.b().c()` — is answered generically by `type_ending_before`,
        // which reads the real, already-resolved static type of whatever
        // expression sits right before the cursor straight off the AST
        // (`resolver.rs`'s `expr_types`, populated by `walk_expr_expected`
        // for every expression in the file) — no raw-text re-parsing.
        let receiver_type = match prefix_path.as_slice() {
            [int_part] if is_digits(int_part) => Some(ksl_core::types::Type::Int),
            [int_part, frac_part] if is_digits(int_part) && is_digits(frac_part) => {
                Some(ksl_core::types::Type::Double)
            }
            _ => {
                let line = position.line + 1;
                let col = position.character + 1;
                analysis.resolved.table.type_ending_before(line, col).cloned().filter(|ty| *ty != ksl_core::types::Type::Unknown)
            }
        };

        return package_path_completions(analysis, &prefix_path, receiver_type.as_ref(), true);
    }

    let line = position.line + 1;
    let col = position.character + 1;
    let mut items = Vec::new();

    // Inside a call's own argument list (`test(longParam<cursor>`) — offer
    // `paramName =` for each of the callee's declared parameters not
    // already claimed by an earlier named argument, on top of the usual
    // locals/stdlib/etc. below (a bare identifier here could just as well
    // be a positional argument's value, so this only adds to the list,
    // never replaces it). `expected`'s own `CallArgument` (see
    // `parser::Parser::parse_call_args`'s doc comment) is the primary
    // source now — real parser state, not a guess — falling back to the
    // raw-text `call_arg_context` only when that comes back empty (no
    // bare-name callee to work with, e.g. `f()(`, or the tail-position
    // guard in `expected_at_cursor` tripped on a genuine unrelated error
    // elsewhere on the line) rather than offering nothing at all.
    let call_ctx = expected
        .iter()
        .find_map(|e| match e {
            Expected::CallArgument { callee: Some(callee), already_named } => {
                Some((callee.clone(), already_named.clone()))
            }
            _ => None,
        })
        .or_else(|| call_arg_context(&up_to_cursor));
    if let Some((callee, used_names)) = call_ctx {
        items.extend(named_arg_completions(analysis, &callee, &used_names, line, col));
    }

    let mut in_class: Option<String> = None;
    for symbol in analysis.resolved.table.symbols_in_scope(line, col) {
        // `this` is a real, scope-registered symbol inside a class
        // member's own body (`resolver.rs`'s `walk_class_decl`/
        // `walk_class_method`) — its type names the enclosing class,
        // which is exactly what implicit `this` (bare field/method
        // access, no `this.` needed) means completion should *also*
        // offer here, below, alongside ordinary locals.
        if symbol.name == "this" {
            if let ksl_core::types::Type::Class(name) = &symbol.ty {
                in_class = Some(name.to_string());
            }
        }
        items.push(CompletionItem {
            label: symbol.name.clone(),
            kind: Some(symbol_completion_kind(symbol.kind)),
            detail: Some(symbol.signature.clone()),
            documentation: symbol.doc.clone().map(Documentation::String),
            ..Default::default()
        });
    }
    if let Some(class_name) = &in_class {
        items.extend(class_member_completions(analysis, class_name));
    }

    for sym in ksl_core::stdlib::default_imported_symbols() {
        items.push(CompletionItem {
            label: sym.name.to_string(),
            kind: Some(symbol_completion_kind(sym.kind)),
            detail: Some(ksl_core::stdlib::format_signature(sym)),
            documentation: Some(Documentation::String(sym.doc.to_string())),
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

    // Every name this file's own real `import a.b.c`/`import a.b.*`
    // brought into bare scope, plus a package-less `@file:Import`ed file's
    // top-level names (flattened into this file's own global scope at
    // runtime — see `package_index::PackageIndex::flat_member`'s doc
    // comment) — both are real, verified resolution now (see
    // `resolver::ResolveResult::imported_symbols`), so offered right
    // alongside this file's own locals, not under any package-qualified
    // prefix.
    for member in &analysis.resolved.imported_symbols {
        items.push(package_member_completion(member));
    }

    // Derived straight from `ksl_core::token::KEYWORDS` — the lexer's own
    // canonical keyword table — rather than a hand-maintained copy, so a
    // keyword added there (as `package`/`import`/`null` once were, without
    // a matching update here) always shows up in completion too. Filtered
    // by `keyword_allowed` — most keywords are valid anywhere a statement
    // can start, but a handful have real grammar restrictions of their own
    // (see its doc comment).
    for (kw, _) in ksl_core::token::KEYWORDS {
        if keyword_allowed(kw, analysis, &expected, line, col) {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
    }

    items
}

/// Whether `kw` is grammatically legal to type at `position` at all — most
/// keywords (`fun`/`val`/`var`/`if`/`when`/`while`/`for`/`return`/`null`/
/// `true`/`false`) can start a statement or appear in an expression
/// anywhere, so they're always offered; a handful have a real, narrower
/// grammar rule of their own:
/// - `package`: only the very first thing in the file (`parser::Parser::
///   parse_package_decl`) — before every `import`/`@file:Import` too, and
///   only if the file doesn't already declare one.
/// - `import`: only before this file's first non-header statement
///   (`parser::Parser::parse_imports` — interleaved freely with `package`
///   and `@file:Import`, but never after "real" code starts).
/// - `break`/`continue`: only inside an enclosing loop (`resolver::
///   SymbolTable::inside_a_loop_at`, matching `parser::Parser::
///   loop_depth`'s own rule, including its reset at a fun/lambda boundary).
/// - `else`: only right after a real `if` that has no `else` of its own
///   yet (`resolver::SymbolTable::else_allowed_at`, a real fact read off
///   the resolved tree — not a guess about whatever `}` happens to precede
///   the cursor).
/// - `in`: only where the real parser would need it next — right after a
///   `for (name` header's own loop variable (`Expected::Token(In)`, from
///   `parser::Parser::parse_for`'s own `self.expect(TokenKind::In, ...)`
///   failing there) — see `ksl_core::completion_probe`. Never at a fresh
///   statement-start position, where `in` can't legally appear at all.
fn keyword_allowed(kw: &str, analysis: &Analysis, expected: &[Expected], line: u32, col: u32) -> bool {
    match kw {
        "package" => {
            analysis.package.is_none()
                && at_or_before(line, col, analysis.first_import_span)
                && at_or_before(line, col, analysis.first_stmt_span)
        }
        "import" => at_or_before(line, col, analysis.first_stmt_span),
        "break" | "continue" => analysis.resolved.table.inside_a_loop_at(line, col),
        "else" => analysis.resolved.table.else_allowed_at(line, col),
        "in" => expected.contains(&Expected::Token(TokenKind::In)),
        _ => true,
    }
}

/// `true` when `(line, col)` (1-based, `resolver`/`Span` convention) is at
/// or before `bound`'s own start — or unconditionally `true` when there's
/// no bound at all (nothing to be "before" yet).
fn at_or_before(line: u32, col: u32, bound: Option<ksl_core::error::Span>) -> bool {
    match bound {
        None => true,
        Some(b) => (line, col) <= (b.line, b.col),
    }
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

/// Whether the cursor sits inside an in-progress bare `import a.b.c` /
/// `import a.b.*` path — never `@file:Import(...)`, which is
/// `file_import_context`'s job. `import` only ever starts a whole
/// statement/line (`parser.rs`'s grammar: `parse_import_use` is the entire
/// statement, nothing else can share its line), so checking the line's own
/// start is enough — unlike `file_import_context`, no longest-suffix-first
/// care is needed since there's only one shape to recognize.
fn import_use_context(up_to_cursor: &str) -> bool {
    up_to_cursor.trim_start().starts_with("import ")
}

/// Completions for a dotted-prefix position (`prefix_path` from
/// `dotted_prefix_before_cursor`) — shared by both `import_use_context` and
/// the general expression-position dotted-prefix branch in `completion()`.
/// Entirely generic over *which* package `prefix_path` might name: real
/// user-declared ones and any built-in one (`kotlin.test`, ...) are both
/// just entries in `analysis.packages` (see `package_index`'s module doc
/// comment) — nothing here matches a package name by string.
///
/// Always includes:
/// - every member of `prefix_path`'s package, if it's a fully known one;
/// - the *next* dotted segment of any known package `prefix_path` is a
///   strict prefix of (`next_segment_completions` — e.g. prefix `kotlin`
///   finds package `kotlin.test` and offers `test`; an empty prefix offers
///   every package's *first* segment, which is exactly what "nothing typed
///   yet" needs, so no separate empty-prefix case exists here).
///
/// `receiver_type`/`allow_builtin_fallback` govern the *only* other
/// possible source, `BUILTIN_METHOD_SIGNATURES` — real Kotlin's `.`-method
/// completion, relevant only because `ExprKind::FieldAccess`/`MethodCall`'s
/// grammar can't tell a package-qualified name from a real value receiver
/// apart (see that node's own doc comment) — added *only* when
/// `allow_builtin_fallback` and either `receiver_type` is concretely known
/// (a resolved local, e.g. `s.` for `s: String` — always single-segment,
/// so this never conflicts with the package checks above) or nothing
/// package-shaped was found at all. A known/prefix-matched package name is
/// a strong enough signal on its own to stop hedging "this might actually
/// be a value receiver" — this is the general fix for a real, previously
/// unnoticed bug (confirmed independently by the user across unrelated
/// package names, not just `kotlin.test`): the old code appended
/// `BUILTIN_METHOD_SIGNATURES` unconditionally after *any* multi-segment
/// prefix, even an exact, fully-known package name. `import_use_context`
/// passes `allow_builtin_fallback: false` unconditionally — an import path
/// is never a value expression, so that hedge never applies there at all,
/// regardless of whether the prefix is package-shaped.
fn package_path_completions(
    analysis: &Analysis,
    prefix_path: &[String],
    receiver_type: Option<&ksl_core::types::Type>,
    allow_builtin_fallback: bool,
) -> Vec<CompletionItem> {
    let package_name = prefix_path.join(".");
    let mut items: Vec<CompletionItem> =
        analysis.packages.members_of(&package_name).iter().map(package_member_completion).collect();
    items.extend(next_segment_completions(analysis, prefix_path));

    let package_shaped = !items.is_empty() || analysis.packages.package_names().any(|n| n == &package_name);

    if allow_builtin_fallback && (receiver_type.is_some() || !package_shaped) {
        // A `Type::Class(name)` receiver (a real user class instance, or
        // the pseudo-class `Number`) — its own fields/methods plus `Any`'s
        // inherited ones, not `BUILTIN_METHODS`' flat, name-keyed table
        // (which has no entry for a dynamic class name at all).
        match receiver_type {
            Some(ksl_core::types::Type::Class(name)) => items.extend(class_member_completions(analysis, name)),
            _ => items.extend(builtin_method_completions(receiver_type)),
        }
    }
    items
}

/// Every completable member of a real user class instance (or the
/// pseudo-class `Number`) — its own fields/methods
/// (`SymbolTable::class_members`, populated as `resolver.rs`'s
/// `walk_class_decl`/`walk_class_method` define each one) plus `Any`'s
/// three native methods (every class implicitly extends `Any`, see
/// `stdlib::BUILTIN_METHODS`'s own doc comment) — the `Type::Class`
/// counterpart of `builtin_method_completions`.
fn class_member_completions(analysis: &Analysis, class_name: &str) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = analysis
        .resolved
        .table
        .class_members(class_name)
        .map(|s| CompletionItem {
            label: s.name.clone(),
            kind: Some(symbol_completion_kind(s.kind)),
            detail: Some(s.signature.clone()),
            documentation: s.doc.clone().map(Documentation::String),
            ..Default::default()
        })
        .collect();
    items.extend(BUILTIN_METHODS.iter().filter(|m| m.receiver == "Any").map(|m| CompletionItem {
        label: m.symbol.name.to_string(),
        kind: Some(CompletionItemKind::METHOD),
        detail: Some(format_signature(&m.symbol)),
        documentation: Some(Documentation::String(m.symbol.doc.to_string())),
        ..Default::default()
    }));
    items
}

/// The next dotted segment of every known package (`analysis.packages`,
/// real or built-in — see `package_path_completions`) whose own segments
/// start with `prefix_path` — e.g. `["kotlin"]` finds `kotlin.test` and
/// yields `test`; `[]` (nothing typed yet) yields every package's first
/// segment. Lets a multi-segment package name be discovered one segment at
/// a time, the same way for a real user package and a built-in one.
fn next_segment_completions(analysis: &Analysis, prefix_path: &[String]) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for full in analysis.packages.package_names() {
        let segments: Vec<&str> = full.split('.').collect();
        if segments.len() > prefix_path.len()
            && segments.iter().zip(prefix_path.iter()).all(|(s, p)| s == p)
        {
            let next = segments[prefix_path.len()];
            if seen.insert(next) {
                items.push(CompletionItem {
                    label: next.to_string(),
                    kind: Some(CompletionItemKind::MODULE),
                    ..Default::default()
                });
            }
        }
    }
    items
}

/// The callee name and already-used named-argument names (if any) for the
/// call whose own argument list the cursor sits directly inside — e.g.
/// `"test(longParam"` -> `Some(("test", vec![]))`, `"test(a = 1, "` ->
/// `Some(("test", vec!["a".to_string()]))`. `None` when the cursor isn't
/// directly inside any parens at all, or the innermost enclosing one isn't
/// immediately preceded by an identifier (only whitespace allowed in
/// between) — a bare grouping paren (`(1 + 2)`), or one right after an
/// operator/comma. A *keyword* immediately followed by `(` (`if (`, `while
/// (`) is textually indistinguishable from a call here and so still comes
/// back as a "callee" (`"if"`, ...) rather than `None` — harmless, since
/// nothing named `if` ever resolves to a real symbol downstream (see
/// `named_arg_completions`), so no bogus completions result either way.
///
/// Like `dotted_prefix_before_cursor`, this is
/// a raw-text recognizer (a call's own argument list doesn't exist as a
/// complete AST node while mid-typed) tracking paren nesting the same way —
/// an already-*closed* nested call's own named arguments (`outer(inner(x =
/// 1), y<cursor>`) never leak into the still-open outer one's `used_names`,
/// since each paren gets its own fresh tracking frame. A `==`/`!=`/`<=`/`>=`
/// comparison is never mistaken for a named-argument assignment (checked by
/// peeking the following character for `==`; the other three already leave
/// no pending identifier by the time `=` is reached, since `!`/`<`/`>`
/// aren't identifier characters). Single-line only, matching every sibling
/// recognizer here (see `completion`'s own doc comment on `up_to_cursor`).
fn call_arg_context(text_before_cursor: &str) -> Option<(String, Vec<String>)> {
    struct Frame {
        callee: Option<String>,
        used_names: Vec<String>,
    }

    let mut stack: Vec<Frame> = Vec::new();
    let mut cur = String::new();
    // The most recently completed identifier, still "live" as a candidate
    // callee/argument-name as long as only whitespace has followed it —
    // any other character (punctuation, an operator, a second identifier
    // after a separator) clears it, since it no longer directly precedes
    // whatever comes next.
    let mut pending: Option<String> = None;
    let mut chars = text_before_cursor.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
            continue;
        }
        if !cur.is_empty() {
            pending = Some(std::mem::take(&mut cur));
        }
        match c {
            ' ' | '\t' | '\r' | '\n' => {}
            '(' => stack.push(Frame { callee: pending.take(), used_names: Vec::new() }),
            ')' => {
                stack.pop();
                pending = None;
            }
            '=' if chars.peek() == Some(&'=') => {
                chars.next();
                pending = None;
            }
            '=' => {
                if let (Some(frame), Some(name)) = (stack.last_mut(), pending.take()) {
                    frame.used_names.push(name);
                }
            }
            _ => pending = None,
        }
    }

    let frame = stack.last()?;
    Some((frame.callee.clone()?, frame.used_names.clone()))
}

/// `paramName =` completions for `callee`'s declared parameters, gathered
/// from every source a callee name can resolve against — a local (possibly
/// overloaded, see the `Fun` filter below) `fun`, a stdlib function, or a
/// cross-file/`@file:Import`ed one (`resolver::ResolveResult::
/// imported_symbols`) — same precedence-agnostic "just union every source"
/// approach `completion`'s own bottom section already takes for locals vs.
/// stdlib vs. imports. `used` (already-claimed names) and a placeholder
/// empty-string param name (stdlib/cross-file params with no real name to
/// match a named argument against at all — see `resolver::CallSignature`'s
/// doc comment) are both skipped; deduplicated by name across every source
/// and every overload.
fn named_arg_completions(analysis: &Analysis, callee: &str, used: &[String], line: u32, col: u32) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    let mut push = |name: &str, ty: ksl_core::types::Type, doc: Option<String>| {
        if name.is_empty() || used.iter().any(|u| u == name) || !seen.insert(name.to_string()) {
            return;
        }
        items.push(CompletionItem {
            label: format!("{name} ="),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(format!("param {name}: {}", ty.name())),
            documentation: doc.map(Documentation::String),
            ..Default::default()
        });
    };

    for sym in analysis.resolved.table.symbols_in_scope(line, col) {
        // A `class`'s own `Symbol` carries its primary constructor's real
        // `param_types`/`param_names` too (patched in by
        // `resolver::Resolver::walk_class_decl`, mirroring an ordinary
        // `fun`'s), so `Point(x = 1)` offers `x =` the same way a plain
        // function call already does.
        if (sym.kind == SymbolKind::Fun || sym.kind == SymbolKind::Class) && sym.name == callee {
            for (i, pname) in sym.param_names.iter().enumerate() {
                let ty = sym.param_types.get(i).cloned().unwrap_or(ksl_core::types::Type::Unknown);
                let doc = sym.param_docs.get(i).cloned().flatten();
                push(pname, ty, doc);
            }
        }
    }
    if let Some(sym) = ksl_core::stdlib::default_imported_symbols().find(|s| s.name == callee) {
        for p in sym.params {
            push(p.name, p.ty.clone(), None);
        }
    }
    for member in &analysis.resolved.imported_symbols {
        if member.symbol.name == callee {
            for (i, pname) in member.symbol.param_names.iter().enumerate() {
                let ty = member.symbol.param_types.get(i).cloned().unwrap_or(ksl_core::types::Type::Unknown);
                let doc = member.symbol.param_docs.get(i).cloned().flatten();
                push(pname, ty, doc);
            }
        }
    }
    items
}

fn package_member_completion(m: &crate::package_index::PackageMember) -> CompletionItem {
    CompletionItem {
        label: m.symbol.name.clone(),
        kind: Some(symbol_completion_kind(m.symbol.kind)),
        detail: Some(m.symbol.signature.clone()),
        documentation: m.symbol.doc.clone().map(Documentation::String),
        ..Default::default()
    }
}

/// `BUILTIN_METHODS`, filtered to `receiver_type`'s own builtin-class chain
/// (`Int`/`Double` -> `Number` -> `Any`, others -> `Any` directly — see
/// `builtin_type_chain`) when it's concretely known (same
/// `Nullable`-unwrapping rule as `hover.rs`'s identical filter), so e.g.
/// `5.` also offers `Any`'s inherited `toString`/`equals`/`hashCode`, not
/// just methods declared directly on `Int` — or offered unfiltered when
/// it's `None`, the "over-suggest, never under-suggest" hedge described on
/// `package_path_completions`.
fn builtin_method_completions(receiver_type: Option<&ksl_core::types::Type>) -> Vec<CompletionItem> {
    let chain: Option<Vec<&str>> = receiver_type.map(|ty| {
        let name = match ty {
            ksl_core::types::Type::Nullable(inner) => inner.name(),
            other => other.name(),
        };
        ksl_core::stdlib::builtin_type_chain(&name)
    });
    BUILTIN_METHODS
        .iter()
        .filter(|m| match &chain {
            Some(chain) => chain.contains(&m.receiver),
            None => true,
        })
        .map(|m| CompletionItem {
            label: m.symbol.name.to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some(format_signature(&m.symbol)),
            documentation: Some(Documentation::String(m.symbol.doc.to_string())),
            ..Default::default()
        })
        .collect()
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
        SymbolKind::Val | SymbolKind::Var | SymbolKind::Param | SymbolKind::Field => CompletionItemKind::VARIABLE,
        SymbolKind::Fun | SymbolKind::Method => CompletionItemKind::FUNCTION,
        SymbolKind::Class => CompletionItemKind::CLASS,
    }
}

/// Where the cursor sits within an in-progress `@file:Import("...")` —
/// doesn't exist as an AST node while mid-typed (same reason
/// `dotted_prefix_before_cursor` operates on raw text: an incomplete
/// `@file:Import("` fails to parse at all), so this is a raw-text
/// recognizer. Its `AtHead`/`AfterFileColon` results still get validated
/// against `ksl_core::completion_probe::expected_at_cursor` before
/// `completion()` trusts them (see there) — this only narrows down
/// *which* of the three positions the cursor might be in, textually.
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

/// Whether the cursor sits inside a still-open `/** ... */` doc comment, at
/// the position to type an `@param` tag's own parameter name: right after
/// the tag word `param` and at least one separating space, before any
/// further whitespace — once a second run of whitespace appears, the rest
/// of the line is the tag's free-text description, not the name anymore
/// (`@param name the description` — only `name` is a completable
/// parameter name). Returns the comment's own text *before* this `@param`
/// tag on success, so the caller can find which parameter names this same
/// comment already documents (`kdoc_already_documented_param_names`) and
/// exclude them.
fn kdoc_param_name_context(text_up_to_cursor: &str) -> Option<&str> {
    let last_open = text_up_to_cursor.rfind("/**")?;
    let after_open = &text_up_to_cursor[last_open + 3..];
    if after_open.contains("*/") {
        return None;
    }
    let last_at = after_open.rfind('@')?;
    let after_at = &after_open[last_at + 1..];
    let rest = after_at.strip_prefix("param")?;
    if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None; // e.g. "@parameter" -- not really the "param" tag
    }
    let trimmed = rest.trim_start_matches([' ', '\t']);
    if trimmed.len() == rest.len() {
        return None; // no whitespace typed yet after the tag word itself
    }
    if trimmed.chars().any(|c| c.is_whitespace()) {
        return None; // past the name, into the tag's free-text description
    }
    Some(&after_open[..last_at])
}

/// Every parameter name a `@param` tag already documents somewhere in
/// `comment_prefix` (the doc comment's own text up to, but not including,
/// the `@param` tag currently being typed) — so `kdoc_param_name_completions`
/// doesn't suggest a name a second time.
fn kdoc_already_documented_param_names(comment_prefix: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = comment_prefix;
    while let Some(idx) = rest.find("@param") {
        let after = &rest[idx + "@param".len()..];
        if !after.starts_with(|c: char| c.is_ascii_alphabetic()) {
            let trimmed = after.trim_start_matches([' ', '\t']);
            let name: String = trimmed.chars().take_while(|c| !c.is_whitespace()).collect();
            if !name.is_empty() {
                names.push(name);
            }
        }
        rest = &rest[idx + "@param".len()..];
    }
    names
}

/// The still-undocumented parameter names of whichever function this
/// `@param` tag belongs to — the nearest top-level `fun` declared at or
/// after the cursor's own line (a KDoc comment always immediately precedes
/// its declaration, with no blank line between — see `doc_comment.rs`'s
/// module doc comment — so its target is simply the closest *following*
/// declaration, without needing to locate the comment's own closing `*/`).
fn kdoc_param_name_completions(
    analysis: &Analysis,
    position: Position,
    already_documented: &[String],
) -> Vec<CompletionItem> {
    let cursor_line = position.line + 1;
    let target = analysis
        .resolved
        .table
        .top_level_symbols()
        .filter(|s| s.kind == SymbolKind::Fun && s.def_span.line >= cursor_line)
        .min_by_key(|s| s.def_span.line);
    let Some(target) = target else { return Vec::new() };
    target
        .param_names
        .iter()
        .filter(|name| !already_documented.iter().any(|d| d == *name))
        .map(|name| CompletionItem { label: name.clone(), kind: Some(CompletionItemKind::VARIABLE), ..Default::default() })
        .collect()
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

    /// Regression test: `completion.rs`'s own keyword list used to be a
    /// hand-maintained copy of `token::KEYWORDS` that had drifted out of
    /// sync — `package`/`import`/`null` were real keywords (`token.rs`)
    /// the lexer recognized, but never offered as completions, because
    /// nobody remembered to add them to the separate copy here by hand.
    /// Now that the list is derived directly from `token::KEYWORDS`, this
    /// can't happen again — this test would fail the moment it did.
    ///
    /// Every keyword still needs its own valid position, though (see
    /// `keyword_allowed`) — a handful (`package`/`import`/`break`/
    /// `continue`/`else`/`in`) aren't legal at the generic "inside a
    /// function body" position every other keyword is checked at, so each
    /// gets its own tailored scenario here instead.
    #[test]
    fn every_real_keyword_is_offered_as_a_completion_somewhere_valid() {
        let general_text = "val outer = 1\nfun f(p: Int): Int {\n    val inner = 2\n    return inner\n}\n";
        let general_names = labels(general_text, Position::new(3, 4));
        let general_kws = [
            "fun", "val", "var", "if", "when", "while", "do", "for", "return", "null", "true", "false",
            // `class` is genuinely legal anywhere a statement can start,
            // same as `fun`/`val`. `this` is not — real Kotlin (and this
            // parser, `Parser::class_member_depth`) restricts it to inside
            // a class member — but classes have no resolver-side scope
            // integration yet (that's this phase's own later step), so
            // there's no `inside_a_class_member_at`-style fact to gate on
            // yet; offered unconditionally for now, same as every other
            // keyword here, until that lands.
            "class", "this",
        ];
        for kw in general_kws {
            assert!(general_names.contains(&kw.to_string()), "'{kw}' wasn't offered inside a function body");
        }

        let package_names = labels("", Position::new(0, 0));
        assert!(package_names.contains(&"package".to_string()), "{package_names:?}");
        assert!(package_names.contains(&"import".to_string()), "{package_names:?}");

        let after_package = "package foo\n";
        let import_names = labels(after_package, Position::new(1, 0));
        assert!(import_names.contains(&"import".to_string()), "{import_names:?}");
        assert!(!import_names.contains(&"package".to_string()), "{import_names:?}");

        let loop_body = "while (true) {\n    \n}\n";
        // Line 2 (0-based 1), inside the loop's own body.
        let loop_names = labels(loop_body, Position::new(1, 4));
        assert!(loop_names.contains(&"break".to_string()), "{loop_names:?}");
        assert!(loop_names.contains(&"continue".to_string()), "{loop_names:?}");

        let after_brace = "if (true) {\n}\n";
        // Right after the "}" that closes the `if`, same line.
        let else_names = labels(after_brace, Position::new(1, 1));
        assert!(else_names.contains(&"else".to_string()), "{else_names:?}");

        // Fully-typed and parseable (closing paren/body included) -- the
        // cursor, placed right after "for (i " (note the trailing space)
        // rather than at the line's end, exercises the same mid-typing
        // detection an unclosed paren would (same reasoning as
        // `named_arg_completion_offers_the_calls_declared_parameter`).
        // The space matters: `parser::Parser::expected_at_cursor` can't
        // tell a *complete* one-letter variable name apart from a still-
        // growing prefix of a longer one from text alone, so `in` is only
        // reported once something (even just whitespace) has clearly
        // ended the name -- see `keyword_allowed`'s doc comment. This also
        // matches the grammar for real: `for(iin1..3)` would lex as one
        // identifier `iin1`, so *some* separator is unavoidable here
        // anyway.
        let for_header = "for (i in 1..3) {\n}\n";
        let in_names = labels(for_header, Position::new(0, 7));
        assert!(in_names.contains(&"in".to_string()), "{in_names:?}");

        for (kw, _) in ksl_core::token::KEYWORDS {
            assert!(
                general_kws.contains(&kw)
                    || ["package", "import", "break", "continue", "else", "in"].contains(&kw),
                "'{kw}' is a real keyword this test doesn't check a valid position for"
            );
        }
    }

    #[test]
    fn keyword_completion_does_not_leak_into_the_wrong_context() {
        // The specific complaint this fixes: these used to be offered
        // literally everywhere, regardless of whether they were legal.
        let mid_function = "fun f() {\n    \n}\n";
        let names = labels(mid_function, Position::new(1, 4));
        assert!(!names.contains(&"package".to_string()), "{names:?}");
        assert!(!names.contains(&"import".to_string()), "{names:?}");
        assert!(!names.contains(&"break".to_string()), "{names:?}");
        assert!(!names.contains(&"continue".to_string()), "{names:?}");
        assert!(!names.contains(&"else".to_string()), "{names:?}");
        assert!(!names.contains(&"in".to_string()), "{names:?}");

        // `package`/`import` after real code has already started.
        let after_code = "println(1)\n";
        let names2 = labels(after_code, Position::new(1, 0));
        assert!(!names2.contains(&"package".to_string()), "{names2:?}");
        assert!(!names2.contains(&"import".to_string()), "{names2:?}");

        // `break`/`continue` inside a function nested in a loop.
        let nested_fun_in_loop = "while (true) {\n    fun f() {\n        \n    }\n}\n";
        let names3 = labels(nested_fun_in_loop, Position::new(2, 8));
        assert!(!names3.contains(&"break".to_string()), "{names3:?}");
        assert!(!names3.contains(&"continue".to_string()), "{names3:?}");

        // `else` with no preceding `}` at all.
        let no_brace = "val x = 1\n";
        let names4 = labels(no_brace, Position::new(1, 0));
        assert!(!names4.contains(&"else".to_string()), "{names4:?}");
    }

    #[test]
    fn in_is_not_offered_inside_a_for_header_before_the_variable_name_exists() {
        // The exact reported bug: `in` must not appear the moment `for (`
        // is typed, before any variable name has been written at all.
        let for_header = "for (i in 1..3) {\n}\n";
        // "for (" is 5 chars -- cursor right after the opening paren.
        let names = labels(for_header, Position::new(0, 5));
        assert!(!names.contains(&"in".to_string()), "{names:?}");
    }

    #[test]
    fn file_import_completion_does_not_offer_to_start_past_the_header() {
        // `@file:Import(...)` is governed by the same header-only ordering
        // rule as `import`/`package` -- typing "@" after real code has
        // already started must not offer to start one here. "@" past the
        // header doesn't parse on its own (see `parse_program`'s module
        // doc comment), so this goes through the same `last_good` fallback
        // `falls_back_to_last_good_tree_when_mid_typing` exercises.
        let good = std::sync::Arc::new(ksl_core::parse_only("println(1)\n").unwrap());
        let path = Path::new("/tmp/nonexistent_test.ksl");
        let text = "println(1)\n@f\n";
        let a = analysis::analyze(text, Some(good), path).expect("should fall back");
        let names: Vec<String> = completion(&a, text, Position::new(1, 2)).into_iter().map(|i| i.label).collect();
        assert!(!names.contains(&"file".to_string()), "{names:?}");
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
    fn completion_after_a_dot_right_on_a_constructor_calls_own_result_offers_its_members() {
        // The reported gap: `Point().<cursor>` -- `dotted_prefix_before_cursor`'s
        // raw-text scan stops at the call's own closing `)`, so this used
        // to fall through to an unfiltered builtin-method dump instead of
        // Point's own members.
        let text = "class Point(val x: Int) {\n    fun bark(): String = \"woof\"\n}\nPoint().bark()\n";
        // "Point()." is 8 chars, cursor right after the dot on line 4 (0-based 3).
        let names = labels(text, Position::new(3, 8));
        assert!(names.contains(&"x".to_string()), "{names:?}");
        assert!(names.contains(&"bark".to_string()), "{names:?}");
        assert!(names.contains(&"toString".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_a_val_decls_constructor_call_offers_its_members() {
        // Exact shape from the reported screenshot: `val origin = Point(0,
        // 0).` -- a real binding, multiple constructor arguments, cursor
        // at the end of the line right after the dot. A trailing `.` with
        // nothing after it doesn't parse on its own (no field/method name
        // yet) -- exactly like `falls_back_to_last_good_tree_when_mid_typing`,
        // this goes through the `last_good` fallback: the *previous*
        // keystroke's tree (without the trailing dot) is what a real
        // editor's LSP session would actually have cached at this point.
        let good = "class Point(val x: Int, val y: Int) {\n    fun bark(): String = \"woof\"\n}\nval origin = Point(0, 0)\n";
        let path = Path::new("/tmp/nonexistent_test.ksl");
        let last_good = std::sync::Arc::new(ksl_core::parse_only(good).unwrap());
        let text = format!("{}.\n", good.trim_end_matches('\n'));
        let a = analysis::analyze(&text, Some(last_good), path).expect("should fall back");
        // "val origin = Point(0, 0)." is 26 chars.
        let names: Vec<String> = completion(&a, &text, Position::new(3, 26)).into_iter().map(|i| i.label).collect();
        assert!(names.contains(&"x".to_string()), "{names:?}");
        assert!(names.contains(&"y".to_string()), "{names:?}");
        assert!(names.contains(&"bark".to_string()), "{names:?}");
        assert!(names.contains(&"toString".to_string()), "{names:?}");
        assert!(!names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_a_plain_functions_call_result_offers_its_return_types_members() {
        let text = "class Point(val x: Int)\nfun makePoint(): Point = Point(1)\nmakePoint().x\n";
        // "makePoint()." is 12 chars, cursor right after the dot on line 3 (0-based 2).
        let names = labels(text, Position::new(2, 12));
        assert!(names.contains(&"x".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_a_chained_method_calls_result() {
        // `a.b().c().<cursor>` — previously an explicitly documented gap
        // (the old, deleted `call_result_type_before_cursor` only ever
        // handled a single bare `name(...)`, never a chain) — now works
        // for free: `identity()`'s own `MethodCall` span is widened to
        // its real closing `)` (`parser.rs`) and gets its own `expr_types`
        // entry (`resolver.rs`) exactly like any other expression.
        let text = "class Point(val x: Int) {\n    fun identity(): Point = this\n}\nPoint(1).identity().x\n";
        // "Point(1).identity()." is 20 characters, cursor right after
        // that second dot on line 4 (0-based 3).
        let names = labels(text, Position::new(3, 20));
        assert!(names.contains(&"x".to_string()), "{names:?}");
        assert!(names.contains(&"identity".to_string()), "{names:?}");
        assert!(!names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_a_user_class_local_offers_its_own_members_plus_any() {
        let text = "class Point(val x: Int) {\n    fun bark(): String = \"woof\"\n}\nval p = Point(1)\np.bark()\n";
        // "p." is 2 chars, cursor right after the dot on line 5 (0-based 4).
        let names = labels(text, Position::new(4, 2));
        assert!(names.contains(&"x".to_string()), "{names:?}");
        assert!(names.contains(&"bark".to_string()), "{names:?}");
        assert!(names.contains(&"toString".to_string()), "{names:?}");
        assert!(names.contains(&"equals".to_string()), "{names:?}");
        assert!(names.contains(&"hashCode".to_string()), "{names:?}");
        // No String-specific method leaks in.
        assert!(!names.contains(&"uppercase".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_a_nullable_string_local_still_offers_string_methods() {
        // Regression test: a `String?`-typed local's static type is
        // `Type::Nullable(String)`, whose `name()` is `"String?"` — the
        // filter must unwrap one `Nullable` layer before comparing against
        // `BUILTIN_METHOD_SIGNATURES`'s bare `"String"` entries, or this
        // wrongly falls through to "offer nothing" instead of "offer
        // everything" (the "show everything" fallback only triggers on
        // `Type::Unknown`, which `Nullable(String)` is not).
        let text = "fun f(s: String?) {\n    s.uppercase()\n}\n";
        let names = labels(text, Position::new(1, 6));
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
        // `String`-specific method. It correctly *does* still offer `Any`'s
        // inherited `toString`/`hashCode`/`equals` (Step 4's real builtin
        // hierarchy) — this only asserts no `String` method leaks in.
        let text = "1.length()\n";
        let names = labels(text, Position::new(0, 2)); // right after "1."
        for string_only in ["length", "uppercase", "lowercase", "trim"] {
            assert!(!names.contains(&string_only.to_string()), "{names:?}");
        }
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
        // against a separate, validly-parsed `Analysis` — an *empty*
        // document, so its `first_stmt_span` is `None` and doesn't
        // spuriously gate this (still header-only) position (see
        // `keyword_allowed`'s doc comment) — the `AtHead` branch itself
        // never touches `analysis.resolved` either way (see
        // `file_import_context`'s doc comment).
        let a = analysis::analyze("", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let names: Vec<String> = completion(&a, "@f\n", Position::new(0, 2)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["file".to_string()]);
    }

    #[test]
    fn completion_after_file_colon_offers_import_keyword() {
        let a = analysis::analyze("", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let names: Vec<String> =
            completion(&a, "@file:Im\n", Position::new(0, 8)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["Import".to_string()]);
    }

    #[test]
    fn completion_right_after_file_colon_with_nothing_typed_yet_offers_import_keyword() {
        // The exact reported bug: right after typing `@file:`, before any
        // letter of "Import" — must still offer it, not just once a prefix
        // of "Import" has already been typed.
        let a = analysis::analyze("", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
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

    /// Regression test: a real (package-declaring) file's own wildcard
    /// `import a.b.*` used to be invisible to general (no-dot-prefix)
    /// completion — only a *package-less* `@file:Import`'s flat namespace
    /// was ever consulted here. `import test.*` must offer `greet` too.
    #[test]
    fn completion_offers_names_brought_in_by_a_wildcard_import_of_a_real_package() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_completion_wildcard_pkg_{n}_{}", std::process::id()));
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
        let names = completion(&a, text, Position::new(2, 8)).into_iter().map(|i| i.label).collect::<Vec<_>>();
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
        let a = analysis::analyze("", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
        let text = "/** already closed */\n@file:Import(\"x.ksl\")\n";
        // Cursor right after the "@" of "@file:Import" on line 2 -- must be
        // recognized as the file-import head, not confused with kdoc tags.
        let names: Vec<String> =
            completion(&a, text, Position::new(1, 1)).into_iter().map(|i| i.label).collect();
        assert_eq!(names, vec!["file".to_string()]);
    }

    #[test]
    fn completion_after_at_outside_any_doc_comment_does_not_offer_kdoc_tags() {
        let a = analysis::analyze("", None, Path::new("/tmp/nonexistent_test.ksl")).unwrap();
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
    fn kdoc_param_name_context_recognizes_the_name_position() {
        // Right after "@param" and its one separating space -- the name
        // position, nothing typed yet.
        assert!(kdoc_param_name_context("/**\n * @param ").is_some());
        // Partway through typing the name.
        assert!(kdoc_param_name_context("/**\n * @param my").is_some());
        // No space yet after the tag word -- still typing the tag itself,
        // `kdoc_tag_context`'s job, not this one's.
        assert!(kdoc_param_name_context("/**\n * @param").is_none());
        // Past the name, into the tag's free-text description.
        assert!(kdoc_param_name_context("/**\n * @param name the description").is_none());
        // "@parameter" is not the "param" tag -- word-boundary check.
        assert!(kdoc_param_name_context("/**\n * @parameter ").is_none());
        // Comment already closed before the cursor.
        assert!(kdoc_param_name_context("/** @param x closed */\n@param ").is_none());
    }

    #[test]
    fn kdoc_already_documented_param_names_finds_every_complete_entry() {
        assert_eq!(kdoc_already_documented_param_names(""), Vec::<String>::new());
        assert_eq!(kdoc_already_documented_param_names(" * @param a the first\n * "), vec!["a".to_string()]);
        assert_eq!(
            kdoc_already_documented_param_names(" * @param a\n * @param b\n * "),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// The reported bug: typing inside an already-written `@param` tag
    /// offered the documented function's own name and other global-scope
    /// symbols instead of its parameter name.
    #[test]
    fn kdoc_param_completion_offers_the_documented_functions_parameter_name() {
        let text = "/**\n * @param \n */\nfun myFunc(myParam: String) {}\n";
        // Line 1 is " * @param " (10 chars) -- cursor right after the
        // space that follows "@param".
        let names = labels(text, Position::new(1, 10));
        assert_eq!(names, vec!["myParam".to_string()], "{names:?}");
    }

    #[test]
    fn kdoc_param_completion_excludes_a_name_already_documented_earlier_in_the_same_comment() {
        let text = "/**\n * @param a first\n * @param \n */\nfun f(a: Int, b: Int) {}\n";
        // Line 2 is " * @param " (10 chars) -- cursor right after its
        // trailing space, about to document the second parameter.
        let names = labels(text, Position::new(2, 10));
        assert_eq!(names, vec!["b".to_string()], "{names:?}");
    }

    #[test]
    fn kdoc_param_completion_stays_empty_with_no_following_function() {
        let text = "/**\n * @param \n */\nval x = 1\n";
        let names = labels(text, Position::new(1, 10));
        assert!(names.is_empty(), "{names:?}");
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

    /// Regression test: `Nothing` is a real, working type (Phase 4.5) but
    /// used to never be offered in a type-annotation position, because
    /// `ksl-core`'s old `PRIMITIVE_TYPE_NAMES` list (a hand-maintained
    /// copy of `resolve_type_hint`'s primitive arms) had silently drifted
    /// out of sync and was missing it — same shape as the `import`/
    /// `package`/`null` keyword-completion bug. Now that both are derived
    /// from one `PRIMITIVE_TYPES` table, this can't happen again.
    #[test]
    fn completion_in_type_position_offers_nothing() {
        let text = "val f: (Int) -> Int = { x -> x + 1 }\n";
        let names = labels(text, Position::new(0, 16));
        assert!(names.contains(&"Nothing".to_string()), "{names:?}");
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

    // --- package-qualified completion (built-in and real packages) ---
    // Regression coverage for the bug reported against `kotlin.test`
    // completion: every case here is driven purely by `analysis.packages`
    // (see `package_path_completions`), so a real user package exercises
    // the exact same code path as the built-in one — nothing here is
    // `kotlin.test`-specific.
    //
    // Each test writes a *complete, valid* line (an in-progress `a.b.`
    // fails to parse at all — there's no identifier after the last `.` —
    // same reason `dotted_prefix_before_cursor`'s own doc comment gives)
    // and places the cursor mid-line, exactly like `completion_after_a_dot_*`
    // above already does for ordinary `.`-method completion.

    #[test]
    fn import_kotlin_test_dot_offers_exactly_the_real_members() {
        let text = "import kotlin.test.assertEquals\n";
        let names = labels(text, Position::new(0, 19)); // right after "import kotlin.test."
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["assertEquals", "assertFalse", "assertNotNull", "assertNull", "assertTrue"]);
        assert!(!names.contains(&"length".to_string()), "{names:?}");
        assert!(!names.contains(&"uppercase".to_string()), "{names:?}");
    }

    #[test]
    fn import_kotlin_dot_offers_only_the_next_segment() {
        // `kotlin.*` now names several real built-in packages
        // (`kotlin.io`, `kotlin.collections`, `kotlin.text`, `kotlin.math`,
        // `kotlin.test`) — every one of them offers its own next segment,
        // never a full member name (`println`, `assertEquals`, ...) this
        // early.
        let text = "import kotlin.test.assertEquals\n";
        let mut names = labels(text, Position::new(0, 14)); // right after "import kotlin."
        names.sort();
        assert_eq!(names, vec!["collections", "io", "math", "test", "text"]);
    }

    #[test]
    fn import_kotlin_test_assert_partial_still_only_offers_real_members() {
        // The editor does its own client-side filtering by the partial
        // text (`"assert"` here) — the server always returns the full
        // candidate list for the resolved context, same as every other
        // completion source in this file (see the module doc comment).
        let text = "import kotlin.test.assertEquals\n";
        let names = labels(text, Position::new(0, 25)); // right after "import kotlin.test.assert"
        assert!(names.contains(&"assertEquals".to_string()), "{names:?}");
        assert!(!names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn bare_kotlin_prefix_with_no_dot_finds_kotlin_test_via_package_names() {
        // The `"kotl"` case: no dot typed yet at all, so this hits the
        // ordinary no-prefix branch, which already lists every known
        // package's full name (`analysis.packages.package_names()`) — the
        // editor's own client-side prefix filter finds `kotlin.test` in
        // that list. No dedicated code path needed for this case.
        let text = "kotl\n";
        let names = labels(text, Position::new(0, 4));
        assert!(names.contains(&"kotlin.test".to_string()), "{names:?}");
    }

    #[test]
    fn general_expression_position_kotlin_test_dot_is_also_fixed() {
        // Same fix, outside of an `import` statement entirely — a bare
        // `kotlin.test.` in ordinary expression position (e.g. building up
        // a fully-qualified call) must not show builtin methods either.
        let text = "fun f() {\n    kotlin.test.assertEquals\n}\n";
        let names = labels(text, Position::new(1, 16)); // right after "    kotlin.test."
        assert!(names.contains(&"assertEquals".to_string()), "{names:?}");
        assert!(!names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn a_real_multi_segment_user_package_gets_the_same_next_segment_and_filtering() {
        // Proves the fix is general, not `kotlin.test`-specific: a real
        // `package a.b` declaration gets identical next-segment discovery
        // and identical builtin-method suppression.
        let dir = std::env::temp_dir().join(format!(
            "ksl_completion_test_real_pkg_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pkg.ksl"), "package a.b\nfun helper(): Int = 1\n").unwrap();
        let main_path = dir.join("main.ksl");
        let main_text = "@file:Import(\"pkg.ksl\")\nimport a.b.helper\n";
        std::fs::write(&main_path, main_text).unwrap();

        let analysis = analysis::analyze(main_text, None, &main_path).expect("should parse");

        // `import a.` -> next segment `b`, no builtin-method noise.
        let names: Vec<String> = completion(&analysis, main_text, Position::new(1, 9))
            .into_iter()
            .map(|i| i.label)
            .collect(); // right after "import a."
        assert_eq!(names, vec!["b".to_string()], "{names:?}");

        // `import a.b.` -> the package's real member, still no builtin noise.
        let names2: Vec<String> = completion(&analysis, main_text, Position::new(1, 11))
            .into_iter()
            .map(|i| i.label)
            .collect(); // right after "import a.b."
        assert!(names2.contains(&"helper".to_string()), "{names2:?}");
        assert!(!names2.contains(&"length".to_string()), "{names2:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn call_arg_context_finds_the_callee_with_no_args_typed_yet() {
        assert_eq!(call_arg_context("test(longParam"), Some(("test".to_string(), vec![])));
    }

    #[test]
    fn call_arg_context_collects_already_used_named_arguments() {
        assert_eq!(call_arg_context("test(a = 1, b"), Some(("test".to_string(), vec!["a".to_string()])));
    }

    #[test]
    fn call_arg_context_a_closed_nested_calls_own_names_do_not_leak_into_the_outer_one() {
        assert_eq!(
            call_arg_context("outer(inner(x = 1), y"),
            Some(("outer".to_string(), vec![])),
            "inner's own 'x' must not appear in outer's used_names"
        );
    }

    #[test]
    fn call_arg_context_a_comparison_is_never_mistaken_for_a_named_argument() {
        // `flag == true` must not register `flag` as a claimed name -- only
        // `==`, never a lone `=`, appears here.
        assert_eq!(call_arg_context("test(flag == true, b"), Some(("test".to_string(), vec![])));
    }

    #[test]
    fn call_arg_context_stays_none_for_a_bare_grouping_paren() {
        assert_eq!(call_arg_context("(1 + 2"), None);
    }

    #[test]
    fn call_arg_context_stays_none_outside_any_open_parens() {
        assert_eq!(call_arg_context("val x = 1"), None);
    }

    #[test]
    fn named_arg_completion_inside_a_nested_still_open_call_offers_the_inner_calls_parameter() {
        // The exact reported bug: `println(greet(<cursor>` — both calls
        // still open (unlike a closed-inner-call shape) — must offer
        // `greet`'s own parameters (`name =`/`greeting =`), not
        // `println`'s (`value =`), which `attach_callee_to_call_argument_
        // error`'s old unconditional overwrite (`parser.rs`) used to
        // report instead. The line is fully parseable — only the cursor
        // sits mid-call — so `expected_at_cursor` re-parses just the
        // truncated-at-cursor text and hits the same still-open-parens
        // shape a genuinely incomplete document would.
        let text = "fun greet(name: String, greeting: String = \"Hello\"): String {\n    return \"$greeting, $name!\"\n}\nprintln(greet(name = \"x\", greeting = \"y\"))\n";
        // "println(greet(" is 14 characters, cursor right after that
        // inner "(" on line 4 (0-based 3).
        let names = labels(text, Position::new(3, 14));
        assert!(names.contains(&"name =".to_string()), "{names:?}");
        assert!(names.contains(&"greeting =".to_string()), "{names:?}");
        assert!(!names.contains(&"value =".to_string()), "{names:?}");
    }

    #[test]
    fn named_arg_completion_inside_a_nested_still_open_call_reverse_nesting_too() {
        // Same shape, nesting reversed — `greet(println(<cursor>` must
        // offer `println`'s own `value =`, not `greet`'s parameters,
        // proving the fix isn't just "always prefer the textually second
        // call" but genuinely "always prefer the innermost still-open
        // one."
        let text = "fun greet(name: String, greeting: String = \"Hello\"): String {\n    return \"$greeting, $name!\"\n}\ngreet(name = println(\"x\"), greeting = \"y\")\n";
        // "greet(name = println(" is 22 characters, cursor right after
        // that inner "(" on line 4 (0-based 3).
        let names = labels(text, Position::new(3, 22));
        assert!(names.contains(&"value =".to_string()), "{names:?}");
        assert!(!names.contains(&"name =".to_string()), "{names:?}");
        assert!(!names.contains(&"greeting =".to_string()), "{names:?}");
    }

    #[test]
    fn named_arg_completion_offers_the_calls_declared_parameter() {
        // The exact reported bug: `Ctrl+Space` right after typing a partial
        // prefix of a declared parameter's name inside a call must offer
        // `paramName =`, not nothing. The line is a fully-typed, parseable
        // call (closing paren included) -- same reasoning as
        // `completion_after_a_dot_offers_builtin_methods`: the cursor,
        // placed right after "longParam" rather than at the line's end,
        // exercises the exact same mid-typing detection a genuinely
        // unclosed paren would, without needing a `last_good` fallback.
        let text = "fun test(longParamName: String) {}\ntest(longParam)\n";
        let names = labels(text, Position::new(1, 14)); // right after "test(longParam"
        assert!(names.contains(&"longParamName =".to_string()), "{names:?}");
    }

    #[test]
    fn completion_inside_a_method_body_offers_implicit_field_and_method_access() {
        // Implicit `this` (docs/syntax.md's "Classes" section) — a
        // promoted field/another method must appear as a bare completion
        // inside a method body too, not just via `this.`/`self.`-style
        // qualification.
        let text = "class Point(val x: Int) {\n    fun bark(): String = \"woof\"\n    fun getX(): Int {\n        return x\n    }\n}\n";
        let names = labels(text, Position::new(3, 15));
        assert!(names.contains(&"x".to_string()), "{names:?}");
        assert!(names.contains(&"bark".to_string()), "{names:?}");
        assert!(names.contains(&"toString".to_string()), "{names:?}");
        assert!(names.contains(&"this".to_string()), "{names:?}");
    }

    #[test]
    fn named_arg_completion_offers_a_class_constructors_declared_parameter() {
        // The reported bug: constructing a class (`Greeting(prefix = ...`)
        // didn't offer parameter-name completions at all — `named_arg_
        // completions` only ever checked `SymbolKind::Fun`, never
        // `SymbolKind::Class`.
        let text = "class Greeting(prefix: String, val name: String) {}\nGreeting(prefix)\n";
        let names = labels(text, Position::new(1, 15)); // right after "Greeting(prefix"
        assert!(names.contains(&"prefix =".to_string()), "{names:?}");
        assert!(names.contains(&"name =".to_string()), "{names:?}");
    }

    #[test]
    fn named_arg_completion_excludes_a_name_already_used_earlier_in_the_same_call() {
        let text = "fun test(a: Int, b: Int) {}\ntest(a = 1, b)\n";
        let names = labels(text, Position::new(1, 13)); // right after "test(a = 1, b"
        assert!(names.contains(&"b =".to_string()), "{names:?}");
        assert!(!names.contains(&"a =".to_string()), "{names:?}");
    }

    #[test]
    fn named_arg_completion_via_expected_at_cursor_excludes_a_nested_calls_own_names() {
        // End-to-end coverage of the new primary path (`Expected::
        // CallArgument` from `expected_at_cursor`, not the raw-text
        // `call_arg_context` fallback): `inner`'s own `x = 1` must not
        // leak into `outer`'s `already_named`.
        let text = "fun inner(x: Int): Int = x\nfun outer(y: Int): Int = y\nouter(inner(x = 1), y)\n";
        // "outer(inner(x = 1), y" is 21 chars -- cursor right after "y".
        let names = labels(text, Position::new(2, 21));
        assert!(names.contains(&"y =".to_string()), "{names:?}");
        assert!(!names.contains(&"x =".to_string()), "{names:?}");
    }

    #[test]
    fn named_arg_completion_offers_every_overloads_parameters() {
        // Real user-defined overloading (see `resolver::ScopeData::names`'s
        // doc comment) -- both candidates' own parameter names are offered,
        // since which overload applies isn't decided until the call is
        // complete.
        let text = "fun test(a: Int): Int = a\nfun test(x: String): String = x\ntest()\n";
        let names = labels(text, Position::new(2, 5)); // right after "test("
        assert!(names.contains(&"a =".to_string()), "{names:?}");
        assert!(names.contains(&"x =".to_string()), "{names:?}");
    }

    #[test]
    fn named_arg_completion_shows_the_parameters_own_param_doc() {
        let text = "/**\n * @param longParamName the important one\n */\nfun test(longParamName: String) {}\ntest(longParam)\n";
        let analysis = analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).expect("should parse");
        let item = completion(&analysis, text, Position::new(4, 14))
            .into_iter()
            .find(|i| i.label == "longParamName =")
            .expect("longParamName = should be offered");
        match item.documentation {
            Some(Documentation::String(doc)) => assert!(doc.contains("the important one"), "{doc}"),
            other => panic!("expected doc, got {other:?}"),
        }
    }
}
