use ksl_core::resolver::SymbolKind;
use ksl_core::stdlib::BUILTIN_METHOD_SIGNATURES;
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

    if import_use_context(&up_to_cursor) {
        let (prefix_path, _partial) = dotted_prefix_before_cursor(&up_to_cursor);
        return package_path_completions(analysis, &prefix_path, None, false);
    }

    if type_position_before_cursor(&up_to_cursor) {
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
    // never replaces it). See `call_arg_context`.
    if let Some((callee, used_names)) = call_arg_context(&up_to_cursor) {
        items.extend(named_arg_completions(analysis, &callee, &used_names, line, col));
    }

    for symbol in analysis.resolved.table.symbols_in_scope(line, col) {
        items.push(CompletionItem {
            label: symbol.name.clone(),
            kind: Some(symbol_completion_kind(symbol.kind)),
            detail: Some(symbol.signature.clone()),
            documentation: symbol.doc.clone().map(Documentation::String),
            ..Default::default()
        });
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
    // a matching update here) always shows up in completion too.
    for (kw, _) in ksl_core::token::KEYWORDS {
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
        items.extend(builtin_method_completions(receiver_type));
    }
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
/// Like `dotted_prefix_before_cursor`/`type_position_before_cursor`, this is
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
        if sym.kind == SymbolKind::Fun && sym.name == callee {
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

/// `BUILTIN_METHOD_SIGNATURES`, filtered to `receiver_type` when it's
/// concretely known (same `Nullable`-unwrapping rule as `hover.rs`'s
/// identical filter) or offered unfiltered when it's `None` — the
/// "over-suggest, never under-suggest" hedge described on
/// `package_path_completions`.
fn builtin_method_completions(receiver_type: Option<&ksl_core::types::Type>) -> Vec<CompletionItem> {
    BUILTIN_METHOD_SIGNATURES
        .iter()
        .filter(|(recv_ty, ..)| match receiver_type {
            Some(ksl_core::types::Type::Nullable(inner)) => inner.name() == *recv_ty,
            Some(ty) => ty.name() == *recv_ty,
            None => true,
        })
        .map(|(_, name, signature, doc)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some(signature.to_string()),
            documentation: Some(Documentation::String(doc.to_string())),
            ..Default::default()
        })
        .collect()
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

    /// Regression test: `completion.rs`'s own keyword list used to be a
    /// hand-maintained copy of `token::KEYWORDS` that had drifted out of
    /// sync — `package`/`import`/`null` were real keywords (`token.rs`)
    /// the lexer recognized, but never offered as completions, because
    /// nobody remembered to add them to the separate copy here by hand.
    /// Now that the list is derived directly from `token::KEYWORDS`, this
    /// can't happen again — this test would fail the moment it did.
    #[test]
    fn every_real_keyword_is_offered_as_a_completion() {
        let text = "val outer = 1\nfun f(p: Int): Int {\n    val inner = 2\n    return inner\n}\n";
        let names = labels(text, Position::new(3, 4));
        for (kw, _) in ksl_core::token::KEYWORDS {
            assert!(names.contains(&kw.to_string()), "'{kw}' is a real keyword but wasn't offered as a completion");
        }
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
    fn named_arg_completion_excludes_a_name_already_used_earlier_in_the_same_call() {
        let text = "fun test(a: Int, b: Int) {}\ntest(a = 1, b)\n";
        let names = labels(text, Position::new(1, 13)); // right after "test(a = 1, b"
        assert!(names.contains(&"b =".to_string()), "{names:?}");
        assert!(!names.contains(&"a =".to_string()), "{names:?}");
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
