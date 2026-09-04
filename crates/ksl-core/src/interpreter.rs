use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::*;
use crate::environment::{AssignError, Env};
use crate::error::{Flow, RuntimeError, Span};
use crate::value::{FunctionValue, Value};

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
}

impl<'a> Interpreter<'a> {
    pub fn new(out: &'a mut dyn std::io::Write) -> Self {
        let globals = Env::new();
        crate::stdlib::populate_globals(&globals);
        Interpreter { globals, out, packages: HashMap::new() }
    }

    pub fn run(&mut self, stmts: &[Stmt]) -> Result<(), Flow> {
        let env = self.globals.clone();
        self.run_in_env(stmts, &env)
    }

    /// Runs `stmts` against `env` directly — the general primitive `run`
    /// and package-scoped execution (see `importer.rs`, which runs a
    /// `package`-declaring file's top-level statements against
    /// `package_env(name)` instead of `globals`) both build on.
    pub fn run_in_env(&mut self, stmts: &[Stmt], env: &Env) -> Result<(), Flow> {
        for stmt in stmts {
            self.exec_stmt(stmt, env)?;
        }
        Ok(())
    }

    /// Gets or creates the `Env` that a `package name` declaration's own
    /// top-level code runs in, and that a `name.member`/`import
    /// name.member` reference resolves `member` against.
    pub fn package_env(&mut self, name: &str) -> Env {
        self.packages
            .entry(name.to_string())
            .or_insert_with(|| self.globals.child())
            .clone()
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
                    apply_binary(bop, cur, rhs, stmt.span)?
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
            StmtKind::FunDecl { name, params, return_type, body, .. } => {
                let f = Value::Function(Rc::new(FunctionValue {
                    name: Some(name.clone()),
                    params: params.clone(),
                    return_type: return_type.clone(),
                    body: body.clone(),
                    closure: env.clone(),
                }));
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
                apply_binary(*op, l, r, expr.span)
            }
            ExprKind::Range { start, end } => {
                let s = self.eval_expr(start, env)?;
                let e = self.eval_expr(end, env)?;
                match (s, e) {
                    (Value::Int(a), Value::Int(b)) => {
                        let items: Vec<Value> = (a..=b).map(Value::Int).collect();
                        Ok(Value::List(Rc::new(RefCell::new(items))))
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
                self.call_value(callee_val, arg_values, expr.span)
            }
            ExprKind::Lambda { params, body } => Ok(Value::Function(Rc::new(FunctionValue {
                name: None,
                params: params.clone(),
                return_type: None,
                body: FunBody::Block(body.clone()),
                closure: env.clone(),
            }))),
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if self.eval_expr(cond, env)?.is_truthy() {
                    self.exec_block(then_branch, &env.child())
                } else if let Some(eb) = else_branch {
                    self.exec_block(eb, &env.child())
                } else {
                    Ok(Value::Unit)
                }
            }
            ExprKind::When { subject, arms } => self.eval_when(subject.as_deref(), arms, env),
            ExprKind::Block(stmts) => self.exec_block(stmts, &env.child()),
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
                return self.exec_block(&arm.body, &env.child());
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
                    return self.exec_block(&arm.body, &env.child());
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
                let positional: Vec<Value> = args.into_iter().map(|(_, v)| v).collect();
                (n.func)(self, positional, span)
            }
            Value::Function(f) => self.call_function(&f, args, span),
            other => Err(RuntimeError::new(format!("'{}' is not callable", other.type_name()), span).into()),
        }
    }

    fn call_function(
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

fn apply_binary(op: BinaryOp, l: Value, r: Value, span: Span) -> Result<Value, Flow> {
    use BinaryOp::*;
    match op {
        Add => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_add(*b))),
            (Value::Str(a), _) => Ok(Value::str(format!("{a}{r}"))),
            (Value::List(a), Value::List(b)) => {
                let mut v = a.borrow().clone();
                v.extend(b.borrow().iter().cloned());
                Ok(Value::List(Rc::new(RefCell::new(v))))
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
        // `run`/`run_err` call `run_source` directly, which — unlike
        // `ksl-cli` — doesn't run `static_check` first, so this exercises
        // the interpreter's own runtime message directly, independent of
        // the resolver's (separate) static rejection of the same script.
        let err = run_err("val x: String? = null\nprintln(x.length())");
        assert!(err.contains("cannot call method 'length' on a null value"), "{err}");
        assert!(!err.contains("'Null' has no method"), "{err}");
    }

    #[test]
    fn unsafe_field_access_on_a_null_value_has_a_clear_message() {
        let err = run_err("val x: String? = null\nprintln(x.y)");
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
        let src = r#"
            fun apply(f, x: Int): Int = f(x)
            println(apply({ it * 2 }, 10))
            val add = { a, b -> a + b }
            println(add(3, 4))
        "#;
        assert_eq!(run(src), "20\n7\n");
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
        let err = run_err("fun add(a: Int, b: Int): Int { return a + b }\nadd(1, \"x\")");
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

    #[test]
    fn declared_typed_var_reassignment_incompatibly_is_a_runtime_error() {
        let err = run_err("var x: Int = 5\nx = \"hi\"\nprintln(x)");
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
        let err = run_err("var x = 5\nx = \"hi\"\nprintln(x)");
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
}
