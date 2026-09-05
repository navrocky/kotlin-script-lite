use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::*;
use crate::environment::{AssignError, Env};
use crate::error::{Flow, RuntimeError, Span, StackFrame};
use crate::gc::GcRegistry;
use crate::value::{function_signature, FunctionValue, Value};

pub struct Interpreter<'a> {
    pub globals: Env,
    pub out: &'a mut dyn std::io::Write,
    /// A package's own `Env` (a direct child of `globals`, so package code
    /// can see stdlib but not other packages' or the entry script's own
    /// locals), keyed by its dotted name. Created lazily via `package_env`
    /// — multiple files declaring the same `package` share one entry here,
    /// so they see each other's top-level names the same way multiple
    /// `@file:Import`ed files without a package already share one flat
    /// `globals`. Never removed; a script only ever adds packages.
    packages: HashMap<String, Env>,
    /// Which file is "currently executing" — stamped onto every
    /// `FunctionValue` created while it's active (`StmtKind::FunDecl`/
    /// `ExprKind::Lambda`). Defaults to a `<script>` sentinel for anything
    /// run without ever going through `importer.rs` (most unit tests);
    /// `importer.rs` sets this to each real file's own path before running
    /// that file's top-level statements.
    pub(crate) current_file: Arc<Path>,
    /// The current file's own resolver-computed overload decisions —
    /// call-site span -> the one local-overload candidate's own
    /// declaration span (`resolver::ResolveResult::resolved_calls`).
    /// Consulted by `eval_expr`'s `Call`/`MethodCall` handling *before*
    /// falling back to `call_value`'s own by-runtime-argument-type search
    /// (`Value::Overload`'s dispatch) — every call the resolver could
    /// already decide statically skips redoing that search at every
    /// execution. Defaults empty for anything run without ever going
    /// through `importer.rs`; swapped — saved and restored, mirroring
    /// `current_file` — around a call so a function's own file's
    /// decisions apply inside its body even when called from a different
    /// file's code.
    pub(crate) current_resolved: Rc<HashMap<Span, Span>>,
    /// The active call chain, innermost (most recent) last — pushed by
    /// `call_function`/`call_value` on entry, popped only on a *successful*
    /// return, so an error in progress leaves every enclosing frame in
    /// place until `run_in_env` reads them off into the error itself (see
    /// that function) and clears this back to empty.
    call_stack: Vec<StackFrame>,
    /// This interpreter's own cycle-collection registry — every `Env`/
    /// `List` (and, eventually, every other GC-tracked container) created
    /// anywhere in its lifetime registers into this one automatically at
    /// construction time (see `gc::GcRef::new`), so nothing here needs to
    /// remember to opt in. `collect_cycles` reads it to find candidates
    /// for its trial-deletion pass. Shared (`Rc`, not owned outright)
    /// because `Env`/`List`/etc. each carry a handle to it too — see
    /// `gc.rs`'s module doc comment.
    pub(crate) gc: Rc<GcRegistry>,
    /// Test-only instrumentation: counts calls that fell through to
    /// `call_overload`'s by-runtime-value search, as opposed to the
    /// `ExprKind::Call` fast path that uses the resolver's own
    /// pre-computed decision — see `overload_search_count`'s doc comment.
    #[cfg(test)]
    overload_search_count: u32,
}

/// Every named `fun` creates a real `Rc` cycle (`StmtKind::FunDecl`'s
/// `closure: env.clone()`, stored back into that same `env`) — plain `Rc`
/// drop can never reclaim that on its own. `globals`/each package `Env`
/// legitimately need to keep such cycles alive for the interpreter's whole
/// life (breaking them earlier would break recursion/re-invocation), so the
/// only correct place to break them is right here, when the `Interpreter`
/// itself is going away for good and nothing needs `globals` to keep
/// working anymore.
impl<'a> Drop for Interpreter<'a> {
    fn drop(&mut self) {
        self.globals.clear();
        for env in self.packages.values() {
            env.clear();
        }
    }
}

impl<'a> Interpreter<'a> {
    pub fn new(out: &'a mut dyn std::io::Write) -> Self {
        let globals = Env::new();
        let gc = globals.registry();
        let mut packages = HashMap::new();
        // One synthetic package env per `stdlib::STDLIB_PACKAGES` entry —
        // real Kotlin packages (`kotlin.io`, `kotlin.test`, ...), not
        // backed by any loaded file. A `default_imported` package's
        // members are additionally mirrored onto `globals` (same `Value`s,
        // not copies — `Value::Native`'s `Rc<NativeValue>` is cheap to
        // clone) so they're visible with no `import` at all, exactly like
        // real Kotlin auto-importing `kotlin.io`/`kotlin.text`/
        // `kotlin.collections`/etc. into every file; a non-default-imported
        // one (`kotlin.math`, `kotlin.test`) stays reachable only via
        // `import`/full qualification — see `STDLIB_PACKAGES`'s own doc
        // comment for exactly which is which and why.
        for pkg in crate::stdlib::STDLIB_PACKAGES {
            let env = globals.child();
            (pkg.populate)(&env);
            if pkg.default_imported {
                for (name, value) in env.own_bindings() {
                    globals.define(&name, value, false, crate::types::Type::Unknown);
                }
            }
            packages.insert(pkg.name.to_string(), env);
        }
        Interpreter {
            globals,
            out,
            packages,
            current_file: Arc::from(Path::new("<script>")),
            current_resolved: Rc::new(HashMap::new()),
            call_stack: Vec::new(),
            gc,
            #[cfg(test)]
            overload_search_count: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn overload_search_count(&self) -> u32 {
        self.overload_search_count
    }

    /// Test-only: how many still-live GC-tracked nodes the registry
    /// currently holds — used to prove `collect_cycles` keeps this bounded
    /// across repeated calls rather than growing with the number of calls
    /// made.
    #[cfg(test)]
    pub(crate) fn registry_len(&self) -> usize {
        self.gc.len()
    }

    /// The call stack captured at the point of the most recent uncaught
    /// error (see `run_in_env`) — empty otherwise, including after a
    /// successful run.
    pub fn call_stack(&self) -> &[StackFrame] {
        &self.call_stack
    }

    pub fn run(&mut self, stmts: &[Stmt]) -> Result<(), Flow> {
        let env = self.globals.clone();
        self.run_in_env(stmts, &env)
    }

    /// Runs `stmts` against `env` directly — the general primitive `run`
    /// and package-scoped execution (see `importer.rs`, which runs a
    /// `package`-declaring file's top-level statements against
    /// `package_env(name)` instead of `globals`) both build on. A
    /// `RuntimeError` that reaches here uncaught gets the interpreter's
    /// current call stack attached exactly once (innermost frame first —
    /// see `StackFrame`), then the stack is cleared either way so a
    /// reused `Interpreter` (multiple files via `importer.rs`, or a test
    /// harness) starts the next run with a clean slate.
    pub fn run_in_env(&mut self, stmts: &[Stmt], env: &Env) -> Result<(), Flow> {
        let result = self.run_in_env_inner(stmts, env);
        match result {
            Err(Flow::Error(mut e)) => {
                e.stack = std::mem::take(&mut self.call_stack);
                Err(Flow::Error(e))
            }
            other => {
                self.call_stack.clear();
                other
            }
        }
    }

    /// Reclaims `Env`s that became unreachable-but-cyclic since the last
    /// collection (or since construction) — e.g. a locally-scoped closure
    /// that captured its own defining scope, called once and no longer
    /// needed. Plain `Rc` drop can't reclaim such a cycle on its own (see
    /// `Interpreter`'s `Drop` impl's doc comment for the general problem);
    /// this runs the trial-deletion algorithm real Kotlin's GC would do
    /// implicitly, restricted to exactly the cyclic case that reference
    /// counting alone can't handle. Returns how many `Env`s were reclaimed
    /// this way (0 is the common, expected result — most code creates no
    /// such cycles).
    ///
    /// # Only call this between top-level calls into a loaded program
    /// Calling this while a KSL call is still executing on the Rust stack
    /// (e.g. from inside a `NativeFn` callback, or any other re-entrant
    /// path) is unsound: the currently-executing call's own local `Env`
    /// chain typically lives only as a Rust-local variable somewhere on
    /// the call stack (see `call_function_inner`'s `call_env`) — it is not
    /// reachable from `globals`/`packages`, and `call_stack:
    /// Vec<StackFrame>` holds no `Env` at all. A collection pass run at
    /// that moment would see zero external anchor for that env chain and
    /// could clear out the still-running call's own variables mid-
    /// execution, corrupting it rather than merely freeing memory. Safe
    /// call sites: right after `run`/`run_in_env` returns (`Ok` or `Err`),
    /// before making the next top-level call into the loaded program.
    pub fn collect_cycles(&mut self) -> usize {
        let live = self.gc.live_nodes();
        if live.is_empty() {
            return 0;
        }

        let index_of = |h: &crate::gc::GcHandle| {
            let target = h.upgrade()?;
            live.iter().position(|l| Rc::ptr_eq(l, &target))
        };

        // Real strong count minus our own transient hold via `live` — the
        // one and only place this algorithm needs that `-1` compensation.
        let mut counts: Vec<usize> = live.iter().map(|n| Rc::strong_count(n).saturating_sub(1)).collect();

        // Subtract every reference held by *another* candidate — what's
        // left is each candidate's count of holders *outside* the set.
        for node in &live {
            let mut refs = Vec::new();
            node.borrow().trace_refs(&mut refs);
            for neighbor in &refs {
                if let Some(i) = index_of(neighbor) {
                    counts[i] = counts[i].saturating_sub(1);
                }
            }
        }

        // Seed real roots (globals/packages) plus every candidate with a
        // positive external count, then transitively mark everything
        // reachable from them.
        let mut reachable = vec![false; live.len()];
        let mut worklist: Vec<usize> = counts.iter().enumerate().filter(|(_, &c)| c > 0).map(|(i, _)| i).collect();
        for root in std::iter::once(self.globals.handle()).chain(self.packages.values().map(Env::handle)) {
            if let Some(i) = index_of(&root) {
                worklist.push(i);
            }
        }
        while let Some(i) = worklist.pop() {
            if reachable[i] {
                continue;
            }
            reachable[i] = true;
            let mut refs = Vec::new();
            live[i].borrow().trace_refs(&mut refs);
            for neighbor in &refs {
                if let Some(j) = index_of(neighbor) {
                    if !reachable[j] {
                        worklist.push(j);
                    }
                }
            }
        }

        // Whatever's left unreached has no anchor outside this set at
        // all — true unreachable cyclic garbage. Clearing it breaks the
        // cycle; ordinary `Rc` drop reclaims the rest.
        let mut cleared = 0;
        for (i, node) in live.iter().enumerate() {
            if !reachable[i] {
                node.borrow_mut().clear();
                cleared += 1;
            }
        }
        cleared
    }

    fn run_in_env_inner(&mut self, stmts: &[Stmt], env: &Env) -> Result<(), Flow> {
        for stmt in stmts {
            self.exec_stmt(stmt, env)?;
        }
        Ok(())
    }

    /// Gets or creates the `Env` that a `package name` declaration's own
    /// top-level code runs in, and that a `name.member`/`import
    /// name.member` reference resolves `member` against.
    pub fn package_env(&mut self, name: &str) -> Env {
        self.packages.entry(name.to_string()).or_insert_with(|| self.globals.child()).clone()
    }

    /// Resolves a package-qualified path like `["a","b","greet"]` — package
    /// `a.b`, member `greet` (the last segment is always the member, every
    /// segment before it the package name) — to the value it names. Shared
    /// by `ExprKind::FieldAccess`/`MethodCall` evaluation (see
    /// `dotted_path_if_unresolved`) and `import a.b.greet` processing in
    /// `importer.rs`.
    pub fn resolve_qualified(&self, path: &[String], span: Span) -> Result<Value, RuntimeError> {
        let (package, member) = path.split_at(path.len() - 1);
        let package_name = package.join(".");
        let package_env = self.packages.get(&package_name).ok_or_else(|| {
            RuntimeError::new(format!("unknown package '{package_name}'"), span)
        })?;
        package_env
            .get(&member[0])
            .ok_or_else(|| RuntimeError::new(format!("undefined name '{}'", path.join(".")), span))
    }

    /// Resolves every top-level member of package `path` (e.g. `["kotlin",
    /// "test"]` for `kotlin.test`) — the wildcard-`import` counterpart of
    /// `resolve_qualified`, which resolves exactly one named member.
    pub fn resolve_package_members(&self, path: &[String], span: Span) -> Result<Vec<(String, Value)>, RuntimeError> {
        let package_name = path.join(".");
        let package_env = self.packages.get(&package_name).ok_or_else(|| {
            RuntimeError::new(format!("unknown package '{package_name}'"), span)
        })?;
        Ok(package_env.own_bindings())
    }

    /// If `receiver` is a chain of bare-identifier `.` access (`a`, `a.b`,
    /// `a.b.c`, ...) whose base name does *not* resolve to a real local
    /// binding in `env`, returns the flattened dotted segments — the
    /// parser can't tell a local from a package name apart (see
    /// `ExprKind::FieldAccess`'s doc comment), so this is how `eval_expr`
    /// defers that decision to evaluation time: try the receiver as a
    /// local value first, and only treat the whole chain as a possible
    /// package-qualified reference (`Interpreter::resolve_qualified`) once
    /// its base is confirmed to not be one. Returns `None` for anything
    /// else, including a chain whose base name *is* a real local (the
    /// receiver is then evaluated normally instead).
    fn dotted_path_if_unresolved(&self, receiver: &Expr, env: &Env) -> Option<Vec<String>> {
        let path = flatten_dotted_chain(receiver)?;
        if env.get(&path[0]).is_some() {
            None
        } else {
            Some(path)
        }
    }

    /// Evaluates a call's argument list in order, pairing each with its
    /// (optional) name — shared by `ExprKind::Call` and the "receiver
    /// wasn't a package reference" branch of `MethodCall`.
    fn eval_args(&mut self, args: &[Arg], env: &Env) -> Result<Vec<(Option<String>, Value)>, Flow> {
        let mut arg_values = Vec::with_capacity(args.len());
        for a in args {
            let v = self.eval_expr(&a.value, env)?;
            arg_values.push((a.name.clone(), v));
        }
        Ok(arg_values)
    }

    /// A block's value is the value of its last statement if that statement
    /// is a bare expression, else `Unit` — mirrors Rust block semantics and
    /// is what makes `if`/`when`/`{}` usable as expressions.
    fn exec_block(&mut self, stmts: &[Stmt], env: &Env) -> Result<Value, Flow> {
        let mut value = Value::Unit;
        for (i, stmt) in stmts.iter().enumerate() {
            let is_last = i + 1 == stmts.len();
            if is_last {
                if let StmtKind::ExprStmt(e) = &stmt.kind {
                    return self.eval_expr(e, env);
                }
            }
            self.exec_stmt(stmt, env)?;
            value = Value::Unit;
        }
        Ok(value)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, env: &Env) -> Result<(), Flow> {
        match &stmt.kind {
            StmtKind::ExprStmt(e) => {
                self.eval_expr(e, env)?;
                Ok(())
            }
            StmtKind::ValDecl {
                name,
                type_annotation,
                mutable,
                init,
                ..
            } => {
                let v = self.eval_expr(init, env)?;
                let declared_type = match type_annotation {
                    Some(ann) => check_type(ann, &v, ann.span())?,
                    // No annotation — infer from the value itself, matching
                    // Kotlin: `var x = 5` infers `Int` just as surely as
                    // `var x: Int = 5` does, so `Env::assign` can enforce
                    // it on every later reassignment (see
                    // `resolver.rs`'s identical `ValDecl` handling).
                    None => crate::types::Type::from_value(&v),
                };
                env.define(name, v, *mutable, declared_type);
                Ok(())
            }
            StmtKind::Assign { name, op, value } => {
                let new_val = if *op == AssignOp::Set {
                    self.eval_expr(value, env)?
                } else {
                    let cur = env
                        .get(name)
                        .ok_or_else(|| RuntimeError::new(format!("undefined name '{name}'"), stmt.span))?;
                    let rhs = self.eval_expr(value, env)?;
                    let bop = match op {
                        AssignOp::Add => BinaryOp::Add,
                        AssignOp::Sub => BinaryOp::Sub,
                        AssignOp::Mul => BinaryOp::Mul,
                        AssignOp::Div => BinaryOp::Div,
                        AssignOp::Rem => BinaryOp::Rem,
                        AssignOp::Set => unreachable!(),
                    };
                    apply_binary(&self.gc, bop, cur, rhs, stmt.span)?
                };
                env.assign(name, new_val).map_err(|e| match e {
                    AssignError::Undefined => {
                        RuntimeError::new(format!("undefined name '{name}'"), stmt.span).into()
                    }
                    AssignError::NotMutable => {
                        RuntimeError::new(format!("cannot reassign val '{name}'"), stmt.span).into()
                    }
                    AssignError::TypeMismatch(actual, expected) => RuntimeError::new(
                        format!("type mismatch: expected {}, got {}", expected.name(), actual.name()),
                        stmt.span,
                    )
                    .into(),
                })
            }
            StmtKind::FunDecl { name, name_span, params, return_type, body, .. } => {
                let new_fn = Rc::new(FunctionValue {
                    name: Some(name.clone()),
                    params: params.clone(),
                    return_type: return_type.clone(),
                    body: body.clone(),
                    closure: env.clone(),
                    file: self.current_file.clone(),
                    // Must match `resolver.rs`'s own `Symbol.def_span`
                    // exactly (`*name_span`, not the whole statement's
                    // span) — that's the span a resolved overload decision
                    // in `resolved_calls` actually names, see
                    // `FunctionValue::decl_span`'s doc comment.
                    decl_span: *name_span,
                    resolved_calls: self.current_resolved.clone(),
                });
                // Real overloading: a sibling `fun` already sharing this
                // exact name *in this same scope* (never a parent's — see
                // `Env::get_own`) groups with it into `Value::Overload`
                // instead of silently overwriting it, matching the
                // resolver's own `ScopeData::names` (which already allowed
                // this statically — see its doc comment).
                let f = match env.get_own(name) {
                    Some(Value::Function(old)) => Value::Overload(Rc::new(vec![old, new_fn])),
                    Some(Value::Overload(old_group)) => {
                        let mut group = (*old_group).clone();
                        group.push(new_fn);
                        Value::Overload(Rc::new(group))
                    }
                    _ => Value::Function(new_fn),
                };
                env.define(name, f, false, crate::types::Type::Unknown);
                Ok(())
            }
            StmtKind::While { cond, body } => {
                while self.eval_expr(cond, env)?.is_truthy() {
                    let loop_env = env.child();
                    match self.exec_block(body, &loop_env) {
                        Ok(_) => {}
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            StmtKind::For {
                var_name,
                iterable,
                body,
                ..
            } => {
                let it = self.eval_expr(iterable, env)?;
                let items = iterate(it, stmt.span)?;
                for item in items {
                    let loop_env = env.child();
                    loop_env.define(var_name, item, false, crate::types::Type::Unknown);
                    match self.exec_block(body, &loop_env) {
                        Ok(_) => {}
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            StmtKind::Return(e) => {
                let v = match e {
                    Some(e) => self.eval_expr(e, env)?,
                    None => Value::Unit,
                };
                Err(Flow::Return(v))
            }
            StmtKind::Break => Err(Flow::Break),
            StmtKind::Continue => Err(Flow::Continue),
        }
    }

    fn eval_expr(&mut self, expr: &Expr, env: &Env) -> Result<Value, Flow> {
        match &expr.kind {
            ExprKind::IntLit(v) => Ok(Value::Int(*v)),
            ExprKind::DoubleLit(v) => Ok(Value::Double(*v)),
            ExprKind::BoolLit(v) => Ok(Value::Bool(*v)),
            ExprKind::StringTemplate(parts) => {
                let mut s = String::new();
                for part in parts {
                    match part {
                        StringPart::Text(t) => s.push_str(t),
                        StringPart::Expr(e) => {
                            let v = self.eval_expr(e, env)?;
                            s.push_str(&v.to_string());
                        }
                    }
                }
                Ok(Value::str(s))
            }
            ExprKind::Name(name) => env
                .get(name)
                .ok_or_else(|| RuntimeError::new(format!("undefined name '{name}'"), expr.span).into()),
            ExprKind::FieldAccess { receiver, field, safe, .. } => {
                if let Some(path) = self.dotted_path_if_unresolved(receiver, env) {
                    let mut full_path = path;
                    full_path.push(field.clone());
                    return self.resolve_qualified(&full_path, expr.span).map_err(Into::into);
                }
                let recv = self.eval_expr(receiver, env)?;
                if *safe && matches!(recv, Value::Null) {
                    return Ok(Value::Null);
                }
                // A clearer message than the generic "has no field" below
                // for the specific, common case of a null receiver — this
                // is the residual, purely-dynamic path a well-typed script
                // never reaches (the resolver now statically rejects an
                // unsafe access whenever the receiver's declared type is
                // nullable); it only fires when that type was genuinely
                // `Unknown` to the static checker (e.g. a package-qualified
                // call's result) and turned out to be `Value::Null` at
                // runtime.
                if matches!(recv, Value::Null) {
                    return Err(RuntimeError::new(
                        format!("cannot access field '{field}' on a null value — check for null first or use ?./!!"),
                        expr.span,
                    )
                    .into());
                }
                Err(RuntimeError::new(
                    format!("'{}' has no field '{field}'", recv.type_name()),
                    expr.span,
                )
                .into())
            }
            ExprKind::MethodCall { receiver, method, args, safe, .. } => {
                if let Some(path) = self.dotted_path_if_unresolved(receiver, env) {
                    let mut full_path = path;
                    full_path.push(method.clone());
                    let callee = self.resolve_qualified(&full_path, expr.span)?;
                    let arg_values = self.eval_args(args, env)?;
                    return self.call_value(callee, arg_values, expr.span);
                }
                let recv = self.eval_expr(receiver, env)?;
                if *safe && matches!(recv, Value::Null) {
                    return Ok(Value::Null);
                }
                // See the identical check in `FieldAccess` above — a
                // clearer message than `call_builtin_method`'s generic
                // "has no method" for the specific, common case of calling
                // on a null receiver.
                if matches!(recv, Value::Null) {
                    return Err(RuntimeError::new(
                        format!("cannot call method '{method}' on a null value — check for null first or use ?./!!"),
                        expr.span,
                    )
                    .into());
                }
                let arg_values: Vec<Value> =
                    self.eval_args(args, env)?.into_iter().map(|(_, v)| v).collect();
                crate::stdlib::call_builtin_method(recv, method, arg_values, expr.span)
            }
            ExprKind::Unary { op, expr: inner } => {
                let v = self.eval_expr(inner, env)?;
                match (op, &v) {
                    (UnaryOp::Neg, Value::Int(i)) => Ok(Value::Int(i.wrapping_neg())),
                    (UnaryOp::Neg, Value::Double(d)) => Ok(Value::Double(-d)),
                    (UnaryOp::Pos, Value::Int(_) | Value::Double(_)) => Ok(v),
                    (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    (UnaryOp::NotNullAssert, Value::Null) => Err(RuntimeError::new(
                        "null cannot be cast to non-null type",
                        expr.span,
                    )
                    .into()),
                    (UnaryOp::NotNullAssert, _) => Ok(v),
                    _ => Err(RuntimeError::new(
                        format!("bad operand type for unary operator: {}", v.type_name()),
                        expr.span,
                    )
                    .into()),
                }
            }
            ExprKind::IncDec { op, name, prefix, .. } => {
                // Sugar over `name = name + 1`/`name = name - 1` — reuses
                // the exact `apply_binary`/`Env::assign` path
                // `StmtKind::Assign`'s `+=`/`-=` already goes through, so
                // `Int`/`Double` widening and mutability/type-mismatch
                // errors behave identically to the equivalent compound
                // assignment.
                let cur = env
                    .get(name)
                    .ok_or_else(|| RuntimeError::new(format!("undefined name '{name}'"), expr.span))?;
                let bop = match op {
                    IncDecOp::Inc => BinaryOp::Add,
                    IncDecOp::Dec => BinaryOp::Sub,
                };
                let new_val = apply_binary(&self.gc, bop, cur.clone(), Value::Int(1), expr.span)?;
                env.assign(name, new_val.clone()).map_err(|e| match e {
                    AssignError::Undefined => RuntimeError::new(format!("undefined name '{name}'"), expr.span),
                    AssignError::NotMutable => RuntimeError::new(format!("cannot reassign val '{name}'"), expr.span),
                    AssignError::TypeMismatch(actual, expected) => RuntimeError::new(
                        format!("type mismatch: expected {}, got {}", expected.name(), actual.name()),
                        expr.span,
                    ),
                })?;
                Ok(if *prefix { new_val } else { cur })
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if *op == BinaryOp::And {
                    let l = self.eval_expr(lhs, env)?;
                    if !l.is_truthy() {
                        return Ok(Value::Bool(false));
                    }
                    let r = self.eval_expr(rhs, env)?;
                    return Ok(Value::Bool(r.is_truthy()));
                }
                if *op == BinaryOp::Or {
                    let l = self.eval_expr(lhs, env)?;
                    if l.is_truthy() {
                        return Ok(Value::Bool(true));
                    }
                    let r = self.eval_expr(rhs, env)?;
                    return Ok(Value::Bool(r.is_truthy()));
                }
                let l = self.eval_expr(lhs, env)?;
                let r = self.eval_expr(rhs, env)?;
                apply_binary(&self.gc, *op, l, r, expr.span)
            }
            ExprKind::Range { start, end } => {
                let s = self.eval_expr(start, env)?;
                let e = self.eval_expr(end, env)?;
                match (s, e) {
                    (Value::Int(a), Value::Int(b)) => {
                        let items: Vec<Value> = (a..=b).map(Value::Int).collect();
                        Ok(Value::List(crate::gc::GcRef::new(self.gc.clone(), items)))
                    }
                    (s, e) => Err(RuntimeError::new(
                        format!("range bounds must be Int, got {} and {}", s.type_name(), e.type_name()),
                        expr.span,
                    )
                    .into()),
                }
            }
            ExprKind::Call { callee, args } => {
                let callee_val = self.eval_expr(callee, env)?;
                let arg_values = self.eval_args(args, env)?;
                // Fast path: the resolver already picked exactly one
                // overload for *this* call site, by static argument type
                // (`resolver::ResolveResult::resolved_calls`) — skip
                // `call_value`'s own by-runtime-value search
                // (`Value::Overload`'s dispatch in `call_overload`)
                // entirely and invoke that one directly. Only reachable
                // when `callee_val` really is an `Overload` group (a
                // single `Function`/`Native` never needed resolving in the
                // first place, so was never recorded here) — falls
                // through to the normal path when this call's span isn't
                // in the map (the resolver left it ambiguous, e.g. an
                // `Unknown`-typed argument) or the recorded declaration
                // isn't found in this group (shouldn't happen for a
                // same-file call, but never fatal either way).
                if let Value::Overload(group) = &callee_val {
                    if let Some(&target) = self.current_resolved.get(&expr.span) {
                        if let Some(f) = group.iter().find(|f| f.decl_span == target) {
                            return self.call_function(f, arg_values, expr.span);
                        }
                    }
                }
                self.call_value(callee_val, arg_values, expr.span)
            }
            ExprKind::Lambda { params, body } => Ok(Value::Function(Rc::new(FunctionValue {
                name: None,
                params: params.clone(),
                return_type: None,
                body: FunBody::Block(body.clone()),
                closure: env.clone(),
                file: self.current_file.clone(),
                decl_span: expr.span,
                resolved_calls: self.current_resolved.clone(),
            }))),
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if self.eval_expr(cond, env)?.is_truthy() {
                    let child = env.child();
                    self.exec_block(then_branch, &child)
                } else if let Some(eb) = else_branch {
                    let child = env.child();
                    self.exec_block(eb, &child)
                } else {
                    Ok(Value::Unit)
                }
            }
            ExprKind::When { subject, arms } => self.eval_when(subject.as_deref(), arms, env),
            ExprKind::Block(stmts) => {
                let child = env.child();
                self.exec_block(stmts, &child)
            }
            ExprKind::NullLit => Ok(Value::Null),
            ExprKind::Elvis { lhs, rhs } => {
                let l = self.eval_expr(lhs, env)?;
                if matches!(l, Value::Null) {
                    self.eval_expr(rhs, env)
                } else {
                    Ok(l)
                }
            }
        }
    }

    fn eval_when(&mut self, subject: Option<&Expr>, arms: &[WhenArm], env: &Env) -> Result<Value, Flow> {
        let subject_val = match subject {
            Some(e) => Some(self.eval_expr(e, env)?),
            None => None,
        };
        for arm in arms {
            if arm.is_else {
                let child = env.child();
                return self.exec_block(&arm.body, &child);
            }
            for cond in &arm.conditions {
                let matched = match cond {
                    WhenCondition::Equals(e) => {
                        let cv = self.eval_expr(e, env)?;
                        Value::values_equal(subject_val.as_ref().unwrap(), &cv)
                    }
                    WhenCondition::Bool(e) => self.eval_expr(e, env)?.is_truthy(),
                };
                if matched {
                    let child = env.child();
                    return self.exec_block(&arm.body, &child);
                }
            }
        }
        Ok(Value::Unit)
    }

    pub fn call_value(
        &mut self,
        callee: Value,
        args: Vec<(Option<String>, Value)>,
        span: Span,
    ) -> Result<Value, Flow> {
        match callee {
            Value::Native(n) => {
                self.push_frame(n.name.clone(), Arc::from(Path::new("<builtin>")), span);
                let positional: Vec<Value> = args.into_iter().map(|(_, v)| v).collect();
                let result = (n.func)(self, positional, span);
                if result.is_ok() {
                    self.call_stack.pop();
                }
                result
            }
            Value::Function(f) => self.call_function(&f, args, span),
            Value::Overload(group) => self.call_overload(&group, args, span),
            other => Err(RuntimeError::new(format!("'{}' is not callable", other.type_name()), span).into()),
        }
    }

    /// Real, Kotlin-style overload resolution at the point a call to a
    /// `Value::Overload` group actually runs — the runtime fallback for
    /// any call the resolver couldn't already resolve to one specific
    /// overload statically (an argument whose static type was `Unknown`;
    /// every *statically* fully-typed call instead takes the fast path in
    /// `eval_expr`'s `Call`/`MethodCall` handling, which looks up the
    /// resolver's own pre-computed decision instead of ever reaching
    /// here). Uses the exact same `types::resolve_overload` core the
    /// static checker uses, fed real runtime argument types
    /// (`Type::from_value`) against each candidate's real parameter types
    /// (`types::function_signature_types`) — so a call that somehow still
    /// reaches here with fully concrete argument values resolves
    /// identically to how the static checker would have, had it known
    /// them.
    fn call_overload(
        &mut self,
        group: &[Rc<FunctionValue>],
        args: Vec<(Option<String>, Value)>,
        span: Span,
    ) -> Result<Value, Flow> {
        #[cfg(test)]
        {
            self.overload_search_count += 1;
        }
        let name = group[0].name.as_deref().unwrap_or("<lambda>");
        let arg_types: Vec<crate::types::Type> = args.iter().map(|(_, v)| crate::types::Type::from_value(v)).collect();
        let candidate_param_types: Vec<Vec<crate::types::Type>> =
            group.iter().map(|f| crate::types::function_signature_types(f).0).collect();
        let candidate_params: Vec<&[crate::types::Type]> = candidate_param_types.iter().map(Vec::as_slice).collect();
        match crate::types::resolve_overload(&candidate_params, &arg_types) {
            crate::types::OverloadResolution::Unique(i) => self.call_function(&group[i], args, span),
            crate::types::OverloadResolution::ArityMismatch => Err(RuntimeError::new(
                format!("no overload of '{name}' accepts {} argument(s)", arg_types.len()),
                span,
            )
            .into()),
            crate::types::OverloadResolution::NoMatch => {
                let arg_list = arg_types.iter().map(crate::types::Type::name).collect::<Vec<_>>().join(", ");
                Err(RuntimeError::new(format!("no overload of '{name}' matches argument types ({arg_list})"), span).into())
            }
            // Every runtime `Value` has a fully concrete `Type::from_value`
            // (unlike the static checker, which can genuinely not know an
            // argument's type yet) — real ambiguity here would mean two
            // overloads both structurally accept the exact same concrete
            // argument types, which `resolve_user_overload`'s own
            // "conflicting overload" diagnostic already rejects at
            // resolve time. Reachable only for a script that ran despite
            // that diagnostic (e.g. a caller of `Interpreter` that skips
            // `resolver::resolve` entirely) — a real error either way,
            // not a panic.
            crate::types::OverloadResolution::Ambiguous(_) => {
                Err(RuntimeError::new(format!("ambiguous call to overloaded function '{name}'"), span).into())
            }
        }
    }

    /// Records `span` (in the *current* top-of-stack frame's own file's
    /// coordinates — it's a span from whatever that frame's body is
    /// currently evaluating) as that frame's current `position`, then
    /// pushes a fresh frame for the callee being entered — see
    /// `StackFrame`'s own doc comment for why the two are kept separate
    /// instead of one field mixing a callee's file with a caller's span.
    fn push_frame(&mut self, signature: String, file: Arc<Path>, span: Span) {
        if let Some(top) = self.call_stack.last_mut() {
            top.position = span;
        }
        self.call_stack.push(StackFrame { signature, file, position: span });
    }

    fn call_function(
        &mut self,
        f: &FunctionValue,
        args: Vec<(Option<String>, Value)>,
        span: Span,
    ) -> Result<Value, Flow> {
        self.push_frame(function_signature(f), f.file.clone(), span);
        // Swapped for the duration of this call — a nested `fun`/lambda
        // declared *inside* `f`'s own body must be stamped with `f`'s own
        // file (see `FunctionValue::file`'s doc comment), and any call
        // site inside `f`'s body must resolve against `f`'s own file's
        // overload decisions, not whichever file happened to be
        // "current" in the *caller* — always restored afterward,
        // regardless of success or error, since this is definitional
        // context, not part of the call-stack/error-reporting mechanism.
        let caller_file = std::mem::replace(&mut self.current_file, f.file.clone());
        let caller_resolved = std::mem::replace(&mut self.current_resolved, f.resolved_calls.clone());
        let result = self.call_function_inner(f, args, span);
        self.current_file = caller_file;
        self.current_resolved = caller_resolved;
        if result.is_ok() {
            self.call_stack.pop();
        }
        result
    }

    fn call_function_inner(
        &mut self,
        f: &FunctionValue,
        args: Vec<(Option<String>, Value)>,
        span: Span,
    ) -> Result<Value, Flow> {
        let call_env = f.closure.child();

        // Implicit `it` parameter: a lambda declared with no explicit
        // parameter list, called with exactly one argument.
        if f.params.is_empty() && f.name.is_none() && args.len() == 1 {
            call_env.define("it", args.into_iter().next().unwrap().1, false, crate::types::Type::Unknown);
        } else {
            if args.len() > f.params.len() {
                return Err(RuntimeError::new(
                    format!(
                        "too many arguments: expected at most {}, got {}",
                        f.params.len(),
                        args.len()
                    ),
                    span,
                )
                .into());
            }
            let mut named: HashMap<String, Value> = HashMap::new();
            let mut positional: Vec<Value> = Vec::new();
            for (name, v) in args {
                match name {
                    Some(n) => {
                        if !f.params.iter().any(|p| p.name == n) {
                            return Err(RuntimeError::new(format!("unknown parameter '{n}'"), span).into());
                        }
                        named.insert(n, v);
                    }
                    None => positional.push(v),
                }
            }
            let mut positional_iter = positional.into_iter();
            for p in &f.params {
                let v = if let Some(v) = named.remove(&p.name) {
                    v
                } else if let Some(v) = positional_iter.next() {
                    v
                } else if let Some(default) = &p.default {
                    self.eval_expr(default, &call_env)?
                } else {
                    return Err(RuntimeError::new(format!("missing argument '{}'", p.name), span).into());
                };
                if let Some(ann) = &p.type_annotation {
                    check_type(ann, &v, span)?;
                }
                call_env.define(&p.name, v, false, crate::types::Type::Unknown);
            }
        }

        let result = match &f.body {
            FunBody::Expr(e) => self.eval_expr(e, &call_env),
            FunBody::Block(stmts) => match self.exec_block(stmts, &call_env) {
                Ok(v) => Ok(v),
                Err(Flow::Return(v)) => Ok(v),
                Err(e) => Err(e),
            },
        }?;
        if let Some(ann) = &f.return_type {
            check_type(ann, &result, span)?;
        }
        Ok(result)
    }
}

/// Flattens a chain of bare-identifier `.` access (`a`, `a.b`, `a.b.c`,
/// ...) into its dotted segments, or `None` if it contains anything else
/// partway through (a call, a literal, ...) — see
/// `Interpreter::dotted_path_if_unresolved`, the only caller.
fn flatten_dotted_chain(expr: &Expr) -> Option<Vec<String>> {
    match &expr.kind {
        ExprKind::Name(name) => Some(vec![name.clone()]),
        ExprKind::FieldAccess { receiver, field, .. } => {
            let mut path = flatten_dotted_chain(receiver)?;
            path.push(field.clone());
            Some(path)
        }
        _ => None,
    }
}

fn iterate(v: Value, span: Span) -> Result<Vec<Value>, Flow> {
    match v {
        Value::List(items) => Ok(items.borrow().clone()),
        other => Err(RuntimeError::new(format!("cannot iterate over {}", other.type_name()), span).into()),
    }
}

/// Checks a value against a source-level type annotation — a thin wrapper
/// over `types::resolve_type_annotation`/`Type::from_value`/`is_assignable`
/// shared with the resolver's static checks (see `docs/roadmap.md`'s
/// type-system phase). No generic-parameter/known-class context is
/// threaded through here (both always empty today, same as the resolver's
/// — see `types::resolve_type_hint`'s doc comment; once classes exist, the
/// interpreter will need its own live class-name lookup to pass through
/// here instead of an always-empty set, so a real class name doesn't
/// start being rejected as "unknown" at runtime). Returns the resolved
/// `Type` on success, so a caller (e.g. `ValDecl`) that needs it for the
/// binding's own declared type doesn't have to resolve the annotation a
/// second time.
///
/// A bare name that doesn't resolve to anything known is itself an error
/// here — a typo or a reference to a type this interpreter doesn't have
/// yet has no legitimate meaning, mirroring `resolver.rs`'s
/// `resolve_type_hint_here` (this is no longer the old "unenforced, always
/// accepted" whitelist). A bad name *inside* a function-type annotation
/// (`(Foo) -> Int`, `Foo` unresolvable) isn't separately flagged here —
/// `is_assignable` already treats that slot as a wildcard `Unknown`, same
/// leniency as everywhere else an inner type can't be pinned down; the
/// resolver's own static check is the one that flags every bad name
/// precisely (see `resolve_type_annotation_here`).
fn check_type(annotation: &TypeAnnotation, v: &Value, span: Span) -> Result<crate::types::Type, Flow> {
    let expected = crate::types::resolve_type_annotation(annotation, &[], &std::collections::HashSet::new());
    if let Some(name) = top_level_named_name(annotation) {
        let unresolved = match &expected {
            crate::types::Type::Unknown => true,
            crate::types::Type::Nullable(inner) => **inner == crate::types::Type::Unknown,
            _ => false,
        };
        if unresolved {
            return Err(RuntimeError::new(format!("unknown type '{name}'"), span).into());
        }
    }
    let actual = crate::types::Type::from_value(v);
    if crate::types::is_assignable(&actual, &expected) {
        Ok(expected)
    } else {
        Err(RuntimeError::new(
            format!("type mismatch: expected {}, got {}", expected.name(), actual.name()),
            span,
        )
        .into())
    }
}

/// Unwraps at most one level of `Nullable` to find a bare `Named`'s own
/// name, for `check_type`'s "unknown type" diagnostic — mirrors this
/// function's existing deliberate non-recursion into `Function`'s nested
/// parameter/return annotations: a bad name buried inside `(Sting) -> Int`
/// still silently resolves that slot to `Unknown` rather than erroring here
/// (the resolver's own `diagnose_unknown_names` is the one place that's
/// checked fully).
fn top_level_named_name(ann: &TypeAnnotation) -> Option<&str> {
    match ann {
        TypeAnnotation::Named(name, _) => Some(name),
        TypeAnnotation::Nullable(inner, _) => top_level_named_name(inner),
        TypeAnnotation::Function(..) => None,
    }
}

fn numeric_pair(l: &Value, r: &Value) -> Option<(f64, f64)> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Some((*a as f64, *b as f64)),
        (Value::Double(a), Value::Double(b)) => Some((*a, *b)),
        (Value::Int(a), Value::Double(b)) => Some((*a as f64, *b)),
        (Value::Double(a), Value::Int(b)) => Some((*a, *b as f64)),
        _ => None,
    }
}

fn compare_values(l: &Value, r: &Value, span: Span) -> Result<Ordering, Flow> {
    match (l, r) {
        (Value::Str(a), Value::Str(b)) => Ok(a.as_ref().cmp(b.as_ref())),
        _ => match numeric_pair(l, r) {
            Some((a, b)) => a
                .partial_cmp(&b)
                .ok_or_else(|| RuntimeError::new("cannot compare NaN", span).into()),
            None => Err(RuntimeError::new(
                format!("cannot compare {} and {}", l.type_name(), r.type_name()),
                span,
            )
            .into()),
        },
    }
}

fn apply_binary(gc: &Rc<GcRegistry>, op: BinaryOp, l: Value, r: Value, span: Span) -> Result<Value, Flow> {
    use BinaryOp::*;
    match op {
        Add => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_add(*b))),
            (Value::Str(a), _) => Ok(Value::str(format!("{a}{r}"))),
            (Value::List(a), Value::List(b)) => {
                let mut v = a.borrow().clone();
                v.extend(b.borrow().iter().cloned());
                Ok(Value::List(crate::gc::GcRef::new(gc.clone(), v)))
            }
            _ => match numeric_pair(&l, &r) {
                Some((a, b)) => Ok(Value::Double(a + b)),
                None => type_err("+", &l, &r, span),
            },
        },
        Sub => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_sub(*b))),
            _ => match numeric_pair(&l, &r) {
                Some((a, b)) => Ok(Value::Double(a - b)),
                None => type_err("-", &l, &r, span),
            },
        },
        Mul => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_mul(*b))),
            _ => match numeric_pair(&l, &r) {
                Some((a, b)) => Ok(Value::Double(a * b)),
                None => type_err("*", &l, &r, span),
            },
        },
        Div => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_div(*b)
                .map(Value::Int)
                .ok_or_else(|| RuntimeError::new("division by zero", span).into()),
            _ => match numeric_pair(&l, &r) {
                Some((a, b)) => Ok(Value::Double(a / b)),
                None => type_err("/", &l, &r, span),
            },
        },
        Rem => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_rem(*b)
                .map(Value::Int)
                .ok_or_else(|| RuntimeError::new("division by zero", span).into()),
            _ => match numeric_pair(&l, &r) {
                Some((a, b)) => Ok(Value::Double(a % b)),
                None => type_err("%", &l, &r, span),
            },
        },
        Eq => Ok(Value::Bool(Value::values_equal(&l, &r))),
        NotEq => Ok(Value::Bool(!Value::values_equal(&l, &r))),
        Lt => Ok(Value::Bool(compare_values(&l, &r, span)? == Ordering::Less)),
        Gt => Ok(Value::Bool(compare_values(&l, &r, span)? == Ordering::Greater)),
        LtEq => Ok(Value::Bool(compare_values(&l, &r, span)? != Ordering::Greater)),
        GtEq => Ok(Value::Bool(compare_values(&l, &r, span)? != Ordering::Less)),
        And | Or => unreachable!("handled with short-circuit evaluation before apply_binary"),
    }
}

fn type_err(op: &str, l: &Value, r: &Value, span: Span) -> Result<Value, Flow> {
    Err(RuntimeError::new(
        format!("unsupported operand types for '{op}': {} and {}", l.type_name(), r.type_name()),
        span,
    )
    .into())
}

#[cfg(test)]
mod tests {
    fn run(src: &str) -> String {
        let mut out = Vec::new();
        crate::run_source(src, &mut out, std::path::Path::new("."), None)
            .unwrap_or_else(|e| panic!("script failed: {e}\nsource:\n{src}"));
        String::from_utf8(out).unwrap()
    }

    fn run_err(src: &str) -> String {
        let mut out = Vec::new();
        match crate::run_source(src, &mut out, std::path::Path::new("."), None) {
            Ok(()) => panic!("expected script to fail:\n{src}"),
            Err(e) => e.to_string(),
        }
    }

    /// Runs `src` directly against a bare `Interpreter` — parsing only,
    /// never through `resolver::resolve` at all (unlike `run`/`run_err`,
    /// which go through `run_source`/`importer.rs`'s unified parse-
    /// resolve-then-run flow and so refuse to execute anything statically
    /// invalid). Exists specifically to exercise the *runtime* layer's own
    /// type-checking (`call_function`'s argument/return checks,
    /// `Env::assign`'s reassignment check, the null-safety runtime
    /// messages) in isolation, for a script that's statically invalid on
    /// purpose — real `ksl-cli` usage would never reach these checks for
    /// such a script (the resolver rejects it first), but the runtime
    /// checks still exist as defense in depth (e.g. for a value whose
    /// static type was genuinely `Unknown`) and deserve their own direct
    /// coverage.
    fn run_bypassing_static_check(src: &str) -> Result<String, String> {
        let program = crate::parse_only(src).expect("valid syntax");
        let mut out = Vec::new();
        let mut interp = super::Interpreter::new(&mut out);
        let result = interp.run(&program.stmts);
        drop(interp);
        match result {
            Ok(()) => Ok(String::from_utf8(out).unwrap()),
            Err(crate::error::Flow::Error(e)) => Err(e.message),
            Err(_) => panic!("expected a Flow::Error"),
        }
    }

    fn run_err_bypassing_static_check(src: &str) -> String {
        match run_bypassing_static_check(src) {
            Ok(_) => panic!("expected script to fail:\n{src}"),
            Err(e) => e,
        }
    }

    /// Isolates the cycle mechanics from the interpreter entirely: builds a
    /// bare `Env` holding a `Value::Function` whose own `closure` points
    /// back at it (the exact shape `StmtKind::FunDecl` produces for every
    /// named function), proves plain `drop` cannot reduce its strong count
    /// on its own (the cycle keeps it alive), and that `clear()` does.
    #[test]
    fn clear_breaks_a_self_referential_env_function_cycle() {
        let env = crate::environment::Env::new();
        let self_ref = std::rc::Rc::new(crate::value::FunctionValue {
            name: Some("foo".to_string()),
            params: Vec::new(),
            return_type: None,
            body: crate::ast::FunBody::Block(Vec::new()),
            closure: env.clone(),
            file: std::sync::Arc::from(std::path::Path::new("<test>")),
            decl_span: crate::error::Span::default(),
            resolved_calls: std::rc::Rc::new(std::collections::HashMap::new()),
        });
        env.define(
            "foo",
            crate::value::Value::Function(self_ref),
            false,
            crate::types::Type::Unknown,
        );

        // Held externally by our own `env` binding, plus internally by
        // `self_ref.closure` — 2, not 1, proving the cycle exists.
        assert_eq!(env.strong_count(), 2);
        env.clear();
        // `clear()` drops the `Value::Function` (and thus its `closure`
        // handle) out of `vars` — only our own external handle remains.
        assert_eq!(env.strong_count(), 1);
    }

    /// The real, end-to-end regression: every named `fun` (here, a
    /// self-recursive one) leaves its own defining scope reachable only
    /// through itself once nothing outside the `Interpreter` needs it
    /// anymore. Proves `Interpreter`'s `Drop` impl (not just the isolated
    /// `Env`-level mechanics above) actually breaks that cycle in the real
    /// `globals` scope produced by running a real script.
    #[test]
    fn dropping_the_interpreter_reclaims_a_self_recursive_functions_cycle() {
        let program = crate::parse_only("fun foo() { foo() }\n").expect("valid syntax");
        let mut out = Vec::new();
        let mut interp = super::Interpreter::new(&mut out);
        assert!(interp.run(&program.stmts).is_ok());

        let globals = interp.globals.clone();
        // Held by `interp.globals`, our own clone, and internally by
        // `foo`'s own `closure` — at least 3, proving the cycle exists
        // before teardown.
        assert!(globals.strong_count() >= 3, "{}", globals.strong_count());

        drop(interp);
        // Only our own external clone remains once `Interpreter::drop`
        // has cleared `globals` (and thus `foo`'s cyclic `closure`).
        assert_eq!(globals.strong_count(), 1);
    }

    /// The target embedding pattern: load a program once, then call into
    /// it repeatedly with the interpreter reused across calls (`globals`
    /// persists, exactly as `Interpreter::run`/`run_in_env`'s own doc
    /// comments already describe). Each call to `make` here defines its
    /// own locally-scoped, self-referential closure that never escapes
    /// into `globals` — without `collect_cycles`, each such call would
    /// leave one more permanently-live `Env` behind. Proves the registry
    /// stays bounded across many calls instead of growing with the call
    /// count.
    #[test]
    fn collect_cycles_reclaims_a_local_self_referential_closure_across_repeated_calls() {
        let src = "fun make(): Int {\n    fun inner() { inner() }\n    return 1\n}\n";
        let program = crate::parse_only(src).expect("valid syntax");
        let mut out = Vec::new();
        let mut interp = super::Interpreter::new(&mut out);
        assert!(interp.run(&program.stmts).is_ok());
        let make = interp.globals.get("make").expect("make is defined");
        // `globals` plus one `Env` per stdlib package (both register too,
        // like everything else — see `gc::GcRef::new`) are permanent,
        // always-reachable baseline nodes, not a leak; what must stay
        // bounded is everything *beyond* that fixed baseline.
        let baseline = interp.registry_len();

        for _ in 0..50 {
            assert!(interp.call_value(make.clone(), Vec::new(), crate::error::Span::default()).is_ok());
            interp.collect_cycles();
        }
        let len = interp.registry_len();
        assert_eq!(len, baseline, "registry grew beyond its fixed baseline across repeated calls");
    }

    /// Guards the trial-deletion algorithm's correctness in the other
    /// direction: something genuinely reachable from `globals` must
    /// survive a collection pass and keep working afterward, not just
    /// "not crash".
    #[test]
    fn collect_cycles_does_not_clear_a_function_still_reachable_from_globals() {
        let src = "fun counter(): Int { return 42 }\n";
        let program = crate::parse_only(src).expect("valid syntax");
        let mut out = Vec::new();
        let mut interp = super::Interpreter::new(&mut out);
        assert!(interp.run(&program.stmts).is_ok());

        interp.collect_cycles();

        let counter = interp.globals.get("counter").expect("counter is defined");
        let result = interp.call_value(counter, Vec::new(), crate::error::Span::default());
        assert!(
            matches!(result, Ok(crate::value::Value::Int(42))),
            "counter() should still work after collect_cycles"
        );
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(run("println(1 + 2 * 3)"), "7\n");
        assert_eq!(run("println((1 + 2) * 3)"), "9\n");
        assert_eq!(run("println(7 / 2)"), "3\n");
        assert_eq!(run("println(7.0 / 2)"), "3.5\n");
    }

    #[test]
    fn val_is_immutable_var_is_not() {
        assert_eq!(run("var x = 1\nx = 2\nprintln(x)"), "2\n");
        let err = run_err("val x = 1\nx = 2\nprintln(x)");
        assert!(err.contains("cannot reassign val"), "{err}");
    }

    #[test]
    fn closures_capture_definition_env() {
        let src = r#"
            fun makeCounter(): Int {
                var count = 0
                fun increment(): Int {
                    count = count + 1
                    return count
                }
                increment()
                increment()
                return increment()
            }
            println(makeCounter())
        "#;
        assert_eq!(run(src), "3\n");
    }

    #[test]
    fn string_templates() {
        assert_eq!(run(r#"val x = 5
println("x = $x, x*2 = ${x * 2}")"#), "x = 5, x*2 = 10\n");
    }

    #[test]
    fn short_circuit_logical_ops() {
        let src = r#"
            fun boom(): Boolean {
                println("evaluated")
                return true
            }
            println(false && boom())
            println(true || boom())
        "#;
        assert_eq!(run(src), "false\ntrue\n");
    }

    #[test]
    fn safe_call_short_circuits_to_null_on_a_null_receiver() {
        let src = r#"
            val s: String? = null
            println(s?.length() ?: -1)
        "#;
        assert_eq!(run(src), "-1\n");
    }

    #[test]
    fn safe_call_dispatches_normally_on_a_non_null_receiver() {
        let src = r#"
            val s: String? = "hi"
            println(s?.length() ?: -1)
        "#;
        assert_eq!(run(src), "2\n");
    }

    #[test]
    fn elvis_does_not_evaluate_rhs_when_lhs_is_non_null() {
        let src = r#"
            fun boom(): Int {
                println("evaluated")
                return -1
            }
            val s: String? = "hi"
            println(s?.length() ?: boom())
        "#;
        assert_eq!(run(src), "2\n");
    }

    #[test]
    fn not_null_assert_passes_through_a_non_null_value() {
        assert_eq!(run("val x: Int? = 5\nprintln(x!!)"), "5\n");
    }

    #[test]
    fn not_null_assert_errors_on_null() {
        let err = run_err("val x: Int? = null\nprintln(x!!)");
        assert!(err.contains("null cannot be cast to non-null type"), "{err}");
    }

    #[test]
    fn unsafe_method_call_on_a_null_value_has_a_clear_message() {
        // A `String?` unsafe (`.`) call is *also* rejected statically
        // (`run_source` refuses to run a script the resolver flags at
        // all — see `run_bypassing_static_check`'s own doc comment) — this
        // specifically exercises the interpreter's own runtime message,
        // independent of that static rejection.
        let err = run_err_bypassing_static_check("val x: String? = null\nprintln(x.length())");
        assert!(err.contains("cannot call method 'length' on a null value"), "{err}");
        assert!(!err.contains("'Null' has no method"), "{err}");
    }

    #[test]
    fn unsafe_field_access_on_a_null_value_has_a_clear_message() {
        let err = run_err_bypassing_static_check("val x: String? = null\nprintln(x.y)");
        assert!(err.contains("cannot access field 'y' on a null value"), "{err}");
        assert!(!err.contains("'Null' has no field"), "{err}");
    }

    #[test]
    fn null_equality_is_never_a_type_error() {
        assert_eq!(run("println(null == null)"), "true\n");
        assert_eq!(run("println(null == 1)"), "false\n");
        assert_eq!(run("println(1 == null)"), "false\n");
    }

    #[test]
    fn chained_safe_calls_short_circuit_through_a_null_receiver() {
        let src = r#"
            val a: String? = null
            println(a?.uppercase()?.length() ?: -1)
        "#;
        assert_eq!(run(src), "-1\n");
    }

    #[test]
    fn assigning_null_to_a_non_nullable_declared_type_is_a_runtime_error() {
        let err = run_err("val x: String = null\nprintln(x)");
        assert!(err.contains("type mismatch"), "{err}");
    }

    #[test]
    fn for_over_range_and_list() {
        assert_eq!(
            run("var s = 0\nfor (i in 1..5) { s = s + i }\nprintln(s)"),
            "15\n"
        );
        assert_eq!(
            run(r#"for (x in listOf("a", "b")) { println(x) }"#),
            "a\nb\n"
        );
    }

    #[test]
    fn when_subject_form() {
        let src = r#"
            fun describe(x: Int): String {
                return when (x) {
                    1 -> "one"
                    2, 3 -> "two-or-three"
                    else -> "other"
                }
            }
            println(describe(1))
            println(describe(3))
            println(describe(9))
        "#;
        assert_eq!(run(src), "one\ntwo-or-three\nother\n");
    }

    #[test]
    fn default_and_named_args() {
        let src = r#"
            fun greet(name: String, greeting: String = "Hello"): String {
                return "$greeting, $name!"
            }
            println(greet("World"))
            println(greet(greeting = "Hi", name = "Rust"))
        "#;
        assert_eq!(run(src), "Hello, World!\nHi, Rust!\n");
    }

    #[test]
    fn lambda_implicit_it_and_explicit_params() {
        // Untyped `f`/lambda params are statically invalid under the
        // strict-typing rules the resolver already enforces (unrelated to
        // this test) — bypassed here since the actual point of this test
        // is the interpreter's own lambda-calling mechanics (implicit
        // `it`, explicit multi-param destructuring-free binding), not
        // whether this particular untyped script is well-typed.
        let src = r#"
            fun apply(f, x: Int): Int = f(x)
            println(apply({ it * 2 }, 10))
            val add = { a, b -> a + b }
            println(add(3, 4))
        "#;
        assert_eq!(run_bypassing_static_check(src).unwrap(), "20\n7\n");
    }

    #[test]
    fn package_qualified_name_calls_directly_without_import() {
        // A single script declaring its own package can be a stand-in for
        // "two files, same package" here: greet() lands in package_env
        // ("a.b"), and a.b.greet(...) resolves it without any `import`.
        let src = r#"
            package a.b
            fun greet(name: String): String = "Hi, $name"
            println(a.b.greet("World"))
        "#;
        assert_eq!(run(src), "Hi, World\n");
    }

    // `import a.b.greet` (bringing a qualified name into bare scope) must
    // appear as a leading declaration, before any regular statement in the
    // importing file — same constraint as `@file:Import`. That means it
    // can't be exercised in one self-contained script the way
    // `package_qualified_name_calls_directly_without_import` above can:
    // realistically it needs a *different*, already-`@file:Import`ed file
    // to have populated the package first. See
    // `importer::tests::import_use_brings_a_qualified_name_into_bare_scope`.

    #[test]
    fn unknown_package_is_a_runtime_error() {
        let err = run_err("println(a.b.greet(\"x\"))");
        assert!(err.contains("unknown package 'a.b'"), "{err}");
    }

    #[test]
    fn unknown_member_of_a_known_package_is_a_runtime_error() {
        let src = r#"
            package a.b
            fun greet(name: String): String = "Hi, $name"
            println(a.b.nope())
        "#;
        let err = run_err(src);
        assert!(err.contains("undefined name 'a.b.nope'"), "{err}");
    }

    #[test]
    fn no_package_script_is_unaffected() {
        // Full backward-compatibility check: a script with no `package`
        // declaration behaves exactly as before this feature existed.
        assert_eq!(run("fun greet(name: String): String = \"Hi, $name\"\nprintln(greet(\"World\"))"), "Hi, World\n");
    }

    #[test]
    fn builtin_string_methods_via_dot_syntax() {
        assert_eq!(run(r#"println("hello".length())"#), "5\n");
        assert_eq!(run(r#"println("kotlin".uppercase())"#), "KOTLIN\n");
        assert_eq!(run(r#"println("KOTLIN".lowercase())"#), "kotlin\n");
        assert_eq!(run(r#"println("  hi  ".trim())"#), "hi\n");
    }

    #[test]
    fn builtin_method_call_on_a_local_variable_receiver() {
        // `s` is a real local, not a package name — must still evaluate `s`
        // and dispatch the method on its value, not treat "s.uppercase" as
        // a qualified-name lookup.
        assert_eq!(run("val s = \"hi\"\nprintln(s.uppercase())"), "HI\n");
    }

    #[test]
    fn method_call_chaining() {
        assert_eq!(run(r#"println("  Loud  ".trim().uppercase())"#), "LOUD\n");
    }

    #[test]
    fn unknown_builtin_method_is_a_runtime_error() {
        let err = run_err(r#"println("hi".shout())"#);
        assert!(err.contains("'String' has no method 'shout'"), "{err}");
    }

    #[test]
    fn field_access_on_a_value_with_no_fields_is_a_runtime_error() {
        // No object system yet (Phase 5) — any `FieldAccess` whose base
        // isn't a package name is a clear runtime error.
        let err = run_err("val s = \"hi\"\nprintln(s.bogus)");
        assert!(err.contains("'String' has no field 'bogus'"), "{err}");
    }

    #[test]
    fn builtin_method_arity_error_reports_call_site_span() {
        let err = run_err("\"hi\".length(1)");
        assert!(err.contains("length expects 0 argument(s), got 1"), "{err}");
        assert!(!err.contains("at 0:0"), "{err}");
    }

    // ---- runtime type enforcement ----

    #[test]
    fn call_function_rejects_a_wrongly_typed_argument() {
        let err = run_err_bypassing_static_check("fun add(a: Int, b: Int): Int { return a + b }\nadd(1, \"x\")");
        assert!(err.contains("type mismatch: expected Int, got String"), "{err}");
    }

    #[test]
    fn call_function_accepts_a_correctly_typed_argument() {
        assert_eq!(run("fun add(a: Int, b: Int): Int { return a + b }\nprintln(add(1, 2))"), "3\n");
    }

    #[test]
    fn call_function_rejects_a_wrongly_typed_return_value() {
        let err = run_err("fun f(): Int { return \"x\" }\nf()");
        assert!(err.contains("type mismatch: expected Int, got String"), "{err}");
    }

    #[test]
    fn call_function_accepts_a_correctly_typed_return_value() {
        assert_eq!(run("fun f(): Int = 5\nprintln(f())"), "5\n");
    }

    // ---- real user-defined function overloading (runtime dispatch) ----

    #[test]
    fn overloaded_fun_dispatches_to_the_right_body_by_argument_type() {
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: Boolean): String = \"bool\"\nprintln(show(5))\nprintln(show(true))\n";
        assert_eq!(run(src), "int\nbool\n");
    }

    #[test]
    fn overloaded_fun_call_with_no_matching_overload_is_a_runtime_error() {
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: Boolean): String = \"bool\"\nshow(\"nope\")\n";
        let err = run_err(src);
        assert!(err.contains("no overload of 'show' matches argument types (String)"), "{err}");
    }

    #[test]
    fn a_single_fun_per_name_still_behaves_exactly_as_before() {
        // Regression safety for the overwhelming common case: no
        // `Value::Overload` involved at all when a name is never
        // redeclared.
        assert_eq!(run("fun add(a: Int, b: Int): Int = a + b\nprintln(add(1, 2))"), "3\n");
    }

    // ---- resolver -> interpreter bridge: skip the runtime search when
    // the resolver already decided statically ----

    #[test]
    fn statically_resolved_overload_call_skips_the_runtime_search() {
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: Boolean): String = \"bool\"\nprintln(show(5))\nprintln(show(true))\n";
        let program = crate::parse_only(src).unwrap();
        let resolved = crate::resolver::resolve(&program);
        assert!(resolved.type_errors.is_empty(), "{:?}", resolved.type_errors);
        let mut out = Vec::new();
        let mut interp = super::Interpreter::new(&mut out);
        interp.current_resolved = std::rc::Rc::new(resolved.resolved_calls);
        assert!(interp.run(&program.stmts).is_ok());
        assert_eq!(interp.overload_search_count(), 0, "the fast path should have skipped the runtime search entirely");
        drop(interp);
        assert_eq!(String::from_utf8(out).unwrap(), "int\nbool\n");
    }

    #[test]
    fn bypassing_static_resolution_falls_back_to_the_runtime_overload_search() {
        // Same script as above, but run the way `run_bypassing_static_check`
        // does — `current_resolved` never populated — so every overloaded
        // call here has no choice but to fall back to `call_overload`'s own
        // by-runtime-value search. Confirms that fallback still exists and
        // still dispatches correctly, independent of the fast path.
        let src = "fun show(x: Int): String = \"int\"\nfun show(x: Boolean): String = \"bool\"\nprintln(show(5))\nprintln(show(true))\n";
        let program = crate::parse_only(src).unwrap();
        let mut out = Vec::new();
        let mut interp = super::Interpreter::new(&mut out);
        assert!(interp.run(&program.stmts).is_ok());
        assert_eq!(interp.overload_search_count(), 2, "both calls should have used the runtime search");
        drop(interp);
        assert_eq!(String::from_utf8(out).unwrap(), "int\nbool\n");
    }

    // ---- call stack / stack traces ----

    #[test]
    fn uncaught_error_prints_a_stack_trace_with_both_frames_in_order() {
        let src = "import kotlin.test.*\nfun b() {\n    assertTrue(1 > 2)\n}\nfun a() {\n    b()\n}\na()\n";
        let err = run_err(src);
        let lines: Vec<&str> = err.lines().collect();
        assert!(lines[0].starts_with("runtime error: assertTrue failed at"), "{err}");
        // Innermost (where the failure happened) first, then outward.
        assert!(lines[1].trim_start().starts_with("at assertTrue ("), "{err}");
        assert!(lines[2].trim_start().starts_with("at b() ("), "{err}");
        assert!(lines[3].trim_start().starts_with("at a() ("), "{err}");
    }

    #[test]
    fn a_successful_run_leaves_the_call_stack_empty() {
        let mut out = Vec::new();
        let mut interp = super::Interpreter::new(&mut out);
        let src = "fun add(a: Int, b: Int): Int = a + b\nprintln(add(1, 2))\n";
        let tokens = crate::lexer::tokenize(src).unwrap();
        let program = crate::parser::parse(&tokens).unwrap();
        assert!(interp.run(&program.stmts).is_ok());
        assert!(interp.call_stack().is_empty(), "{:?}", interp.call_stack());
    }

    #[test]
    fn declared_typed_var_reassignment_incompatibly_is_a_runtime_error() {
        let err = run_err_bypassing_static_check("var x: Int = 5\nx = \"hi\"\nprintln(x)");
        assert!(err.contains("type mismatch: expected Int, got String"), "{err}");
    }

    #[test]
    fn hintless_var_reassignment_across_types_is_a_runtime_error() {
        // Matching Kotlin: `var x = 5` infers `Int` from the initializer
        // just as surely as `var x: Int = 5` does, so `x = "hi"` is a type
        // error either way, not a special dynamic case for the hint-less
        // form. At runtime there's no "Unknown" escape hatch the way there
        // is statically — every value has a concrete `Type::from_value`,
        // so a hint-less local is type-locked to its initial value's type
        // from the moment it's first assigned.
        let err = run_err_bypassing_static_check("var x = 5\nx = \"hi\"\nprintln(x)");
        assert!(err.contains("type mismatch: expected Int, got String"), "{err}");
    }

    #[test]
    fn typo_d_type_hint_is_an_unknown_type_runtime_error() {
        let err = run_err("val x: INt = 5");
        assert!(err.contains("unknown type 'INt'"), "{err}");
    }

    #[test]
    fn typo_d_param_type_hint_is_an_unknown_type_runtime_error() {
        let err = run_err("fun f(x: Sting) { }\nf(1)");
        assert!(err.contains("unknown type 'Sting'"), "{err}");
    }

    #[test]
    fn check_type_error_wording_is_unchanged_by_the_refactor() {
        let err = run_err("val x: Double = 5");
        assert!(err.contains("type mismatch: expected Double, got Int"), "{err}");
    }

    #[test]
    fn function_typed_val_accepts_a_matching_function_value() {
        let src = "fun addOne(x: Int): Int { return x + 1 }\nval f: (Int) -> Int = addOne\nprintln(f(4))\n";
        assert_eq!(run(src), "5\n");
    }

    #[test]
    fn function_typed_val_rejects_a_wrongly_shaped_function_value() {
        let src = "fun greet(x: String): String { return x }\nval f: (Int) -> Int = greet\n";
        let err = run_err(src);
        assert!(err.contains("type mismatch"), "{err}");
    }

    #[test]
    fn function_typed_val_rejects_a_non_callable_value() {
        let err = run_err("val f: (Int) -> Int = 5\n");
        assert!(err.contains("type mismatch: expected (Int) -> Int, got Int"), "{err}");
    }

    #[test]
    fn postfix_increment_returns_old_value_and_mutates() {
        assert_eq!(run("var x = 5\nprintln(x++)\nprintln(x)"), "5\n6\n");
    }

    #[test]
    fn postfix_decrement_returns_old_value_and_mutates() {
        assert_eq!(run("var x = 5\nprintln(x--)\nprintln(x)"), "5\n4\n");
    }

    #[test]
    fn prefix_increment_and_decrement_return_the_new_value() {
        assert_eq!(run("var x = 5\nprintln(++x)\nprintln(x)"), "6\n6\n");
        assert_eq!(run("var x = 5\nprintln(--x)\nprintln(x)"), "4\n4\n");
    }

    #[test]
    fn increment_works_on_double_locals() {
        assert_eq!(run("var d = 1.5\nd++\nprintln(d)"), "2.5\n");
    }

    #[test]
    fn increment_in_a_while_condition_matches_kotlin_semantics() {
        // `x++ < 5` compares the *old* value, then increments — a do-while-
        // shaped loop body relies on exactly this ordering.
        let src = "var x = 0\nvar count = 0\nwhile (x++ < 5) {\n    count = count + 1\n}\nprintln(count)\nprintln(x)";
        assert_eq!(run(src), "5\n6\n");
    }

    #[test]
    fn decrement_a_val_is_a_runtime_error() {
        let err = run_err("val x = 5\nx++\n");
        assert!(err.contains("cannot reassign val"), "{err}");
    }

    #[test]
    fn braceless_if_branch_with_return_runs_correctly() {
        let src = "fun f(b: Boolean): String {\n    if (b) return \"yes\"\n    return \"no\"\n}\nprintln(f(true))\nprintln(f(false))";
        assert_eq!(run(src), "yes\nno\n");
    }

    #[test]
    fn braceless_if_branch_with_continue_runs_correctly() {
        let src = "var count = 0\nvar i = 0\nwhile (i < 5) {\n    i = i + 1\n    if (i <= 2) continue\n    count = count + 1\n}\nprintln(count)";
        assert_eq!(run(src), "3\n");
    }

    #[test]
    fn braceless_for_and_while_bodies_run_correctly() {
        let src = "var j = 0\nfor (i in listOf(1, 2, 3)) j = j + i\nprintln(j)\nvar k = 0\nwhile (k < 3) k = k + 1\nprintln(k)";
        assert_eq!(run(src), "6\n3\n");
    }

    #[test]
    fn increment_an_undefined_name_is_a_runtime_error() {
        let err = run_err("x++\n");
        assert!(err.contains("undefined name 'x'"), "{err}");
    }
}
