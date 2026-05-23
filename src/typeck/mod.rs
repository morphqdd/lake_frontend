//! Type checker for Lake.
//!
//! Runs on the **resolved** AST (after [`crate::resolver::resolve_program`]) and
//! emits [`LakeError`]s. For the full list of error codes see `ERROR_CODES.md`.
//!
//! Signature lookup is delegated to [`ProgramRegistry`]: rt fns, machines,
//! ffi declarations and import bindings all flow through one entry point so
//! the checker doesn't maintain its own parallel symbol tables.

use std::collections::HashMap;

use chumsky::span::{SimpleSpan, Spanned};

use crate::{
    api::{
        ast::{Branch, Item, MachineItem, Type},
        expr::Expr,
    },
    error_handle::{LakeError, LakeErrors},
    loader::ParsedProgram,
    registry::{ModuleId, ModulePath, ProgramRegistry, Resolution, Signature},
    resolver::is_unknown,
};

/// Single-module type checker.  Borrows the program registry — every
/// signature lookup goes through there.
pub struct TypeChecker<'src, 'r> {
    registry: &'r ProgramRegistry<'src>,
    current_module: ModuleId,
    errors: Vec<LakeError>,
}

impl<'src, 'r> TypeChecker<'src, 'r> {
    pub fn new(registry: &'r ProgramRegistry<'src>, current_module: ModuleId) -> Self {
        Self {
            registry,
            current_module,
            errors: Vec::new(),
        }
    }

    pub fn check_module(&mut self, items: &[Spanned<Item<'src>>]) {
        for item in items {
            if let Item::Machine(m) = &item.inner {
                let machine_name = m.inner.ident.inner.0;
                for machine_item in &m.inner.items {
                    if let MachineItem::Branch(b) = &machine_item.inner {
                        self.check_branch(machine_name, b);
                    }
                }
            }
        }
    }

    pub fn into_errors(self) -> Vec<LakeError> {
        self.errors
    }

    fn check_branch(&mut self, machine_name: &'src str, branch: &Branch<'src>) {
        let mut scope: HashMap<&'src str, Type<'src>> = HashMap::new();
        for pat in &branch.patterns {
            if !pat.inner.is_wildcard() {
                scope.insert(pat.inner.ident.inner.0, pat.inner.ty.inner.clone());
            }
        }
        // Pre-seed the scope with every `let` binding in the branch body.
        // Without this `p(msg)` after `let p pid = ...` is incorrectly
        // reported as an unknown callable on the second pass through the
        // body (we walk top-down and the binding hasn't been recorded yet
        // when the call is encountered downstream).  This is also what
        // matches the resolver's behaviour, which already filled the
        // post-let types on every Var reference.
        for expr in &branch.body {
            collect_let_bindings(&expr.inner, &mut scope);
        }
        for expr in &branch.body {
            self.check_expr(machine_name, &scope, &expr.inner, expr.span);
        }
    }

    fn check_expr(
        &mut self,
        machine_name: &'src str,
        scope: &HashMap<&'src str, Type<'src>>,
        expr: &Expr<'src>,
        span: SimpleSpan,
    ) {
        match expr {
            Expr::Var(name, ty) => {
                // `self` is a keyword (always in scope) and `_` is the
                // wildcard pattern (legal as a `when` arm and as a branch
                // parameter; never refers to an actual binding).
                if *name == "self" || *name == "_" {
                    return;
                }
                // First check local scope; if not bound and the resolver
                // left the type unknown, the name escapes scope.
                if scope.contains_key(name) {
                    return;
                }
                if is_unknown(ty) && self.registry.resolve_bare_in(self.current_module, name).is_none()
                {
                    self.errors.push(
                        LakeError::new(format!("undeclared variable `{name}`"), span)
                            .code("E001")
                            .help(format!(
                                "declare `{name}` in the branch parameters, \
                                 e.g. `{name} i64`"
                            )),
                    );
                }
            }

            Expr::Let {
                default: Some(default),
                ..
            } => {
                self.check_expr(machine_name, scope, &default.inner, default.span);
            }

            Expr::Jump { ident, args } => {
                for arg in args {
                    self.check_expr(machine_name, scope, &arg.inner, arg.span);
                }
                self.check_jump(machine_name, scope, ident, args, span);
            }

            Expr::Add(l, r) | Expr::Sub(l, r) | Expr::Mul(l, r) | Expr::Div(l, r) => {
                self.check_arith_operand(machine_name, scope, l);
                self.check_arith_operand(machine_name, scope, r);
            }

            Expr::BAnd(l, r)
            | Expr::BOr(l, r)
            | Expr::BXor(l, r)
            | Expr::Shl(l, r)
            | Expr::Shr(l, r) => {
                self.check_arith_operand(machine_name, scope, l);
                self.check_arith_operand(machine_name, scope, r);
            }

            Expr::Neg(inner) => {
                self.check_arith_operand(machine_name, scope, inner);
            }

            Expr::When { cond, branches } => {
                self.check_expr(machine_name, scope, &cond.inner, cond.span);
                for (pat, body) in branches {
                    self.check_expr(machine_name, scope, &pat.inner, pat.span);
                    for e in body {
                        self.check_expr(machine_name, scope, &e.inner, e.span);
                    }
                }
            }

            Expr::Wait { handlers, filter } => {
                // Filter pids must be in scope; check them first.
                for f in filter {
                    self.check_expr(machine_name, scope, &f.inner, f.span);
                }
                for handler in handlers {
                    let mut handler_scope = scope.clone();
                    for pat in &handler.inner.patterns {
                        if !pat.inner.is_wildcard() {
                            handler_scope
                                .insert(pat.inner.ident.inner.0, pat.inner.ty.inner.clone());
                        }
                    }
                    for e in &handler.inner.body {
                        self.check_expr(machine_name, &handler_scope, &e.inner, e.span);
                    }
                }
            }

            Expr::Tuple(elems) => {
                for e in elems {
                    self.check_expr(machine_name, scope, &e.inner, e.span);
                }
            }

            Expr::TupleIndex { receiver, .. } => {
                // Bounds-check happens at codegen via the resolved
                // Type::Struct fields; the surface check here just
                // recurses into the receiver to surface scope errors.
                self.check_expr(machine_name, scope, &receiver.inner, receiver.span);
            }

            Expr::Index { receiver, index } => {
                // Bounds-check is enforced at runtime by the codegen.
                // Walk sub-exprs so scope errors surface here.  Stricter
                // shape checks (receiver must be buf/str, index must be
                // i64) land with #76's ownership-aware typeck.
                self.check_expr(machine_name, scope, &receiver.inner, receiver.span);
                self.check_expr(machine_name, scope, &index.inner, index.span);
            }

            // Atoms are bare literal tags — nothing to validate at this layer.
            Expr::Atom(_) => {}

            _ => {}
        }
    }

    /// Validate a `Jump { ident, args }`.  Splits the work by callee shape:
    /// `self()`, bare-name calls (rt / machine / ffi / import), and inline
    /// module-qualified `core:io:writer(...)`.
    fn check_jump(
        &mut self,
        machine_name: &'src str,
        scope: &HashMap<&'src str, Type<'src>>,
        ident: &Spanned<Expr<'src>>,
        args: &[Spanned<Expr<'src>>],
        call_span: SimpleSpan,
    ) {
        let arg_types: Vec<String> = args.iter().map(|a| self.expr_type_str(&a.inner)).collect();

        match &ident.inner {
            Expr::Var("self", _) => {
                // `self(...)` validates against the *enclosing* machine's
                // branches.
                self.validate_machine_call(machine_name, &arg_types, call_span, true);
            }
            Expr::Var(callee_name, _) => {
                // A name bound in the local scope is either a regular
                // value or a `pid`.  pid-typed vars are *message sends*
                // (`po(msg)` → push to po's mailbox), not function calls;
                // arity / typing for those is dynamic at the runtime
                // level and out of this pass's reach, so we skip them.
                if scope.contains_key(callee_name) {
                    return;
                }
                let resolution = self.registry.resolve_bare_in(self.current_module, callee_name);
                self.validate_resolution(callee_name, resolution, &arg_types, call_span);
            }
            Expr::Path(segments) => {
                if segments.len() < 2 {
                    return;
                }
                let display_path = segments
                    .iter()
                    .map(|s| s.inner.0)
                    .collect::<Vec<_>>()
                    .join(":");
                let Some((target_id, item_name)) =
                    self.registry.resolve_path(self.current_module, segments)
                else {
                    self.errors.push(
                        LakeError::new(
                            format!("unknown module path `{display_path}`"),
                            call_span,
                        )
                        .code("M005"),
                    );
                    return;
                };
                let resolution = self.registry.resolve_in_module(target_id, item_name);
                self.validate_resolution(&display_path, resolution, &arg_types, call_span);
            }
            _ => {
                // Other callee shapes (DotAccess, MethodCall, …) — not
                // type-checked by this pass.
            }
        }
    }

    fn validate_resolution(
        &mut self,
        display_name: &str,
        resolution: Option<Resolution<'_>>,
        arg_types: &[String],
        call_span: SimpleSpan,
    ) {
        match resolution {
            Some(Resolution::Rt(sig)) | Some(Resolution::Ffi(sig)) => {
                if !sig_matches_args(sig, arg_types) {
                    self.errors.push(no_match_error(
                        display_name,
                        std::slice::from_ref(sig),
                        arg_types,
                        call_span,
                        false,
                    ));
                }
            }
            Some(Resolution::Machine(m)) => {
                let any_match = m.branches.iter().any(|b| sig_matches_args(b, arg_types));
                if !any_match {
                    self.errors.push(no_match_error(
                        display_name,
                        &m.branches,
                        arg_types,
                        call_span,
                        false,
                    ));
                }
            }
            Some(Resolution::Const(_)) => {
                // Calling a const is meaningless — it's a value, not a
                // callable.  Surface a tailored error so the user fixes
                // the call shape rather than wonders why the signature
                // didn't match.
                self.errors.push(
                    LakeError::new(
                        format!("`{display_name}` is a const value, not a callable"),
                        call_span,
                    )
                    .code("E004"),
                );
            }
            Some(Resolution::Record(_)) => {
                // Constructor call validation lands in T7/T8.
            }
            None => {
                // Unknown name — surface as an explicit "no such callable"
                // error.  Cheaper than waiting for codegen to fail.
                self.errors.push(
                    LakeError::new(
                        format!("no callable named `{display_name}` in scope"),
                        call_span,
                    )
                    .code("E004")
                    .help(
                        "declare it as `@rt(name)`, `@ffi(name { params } { ret })`, \
                         a machine, or import it from another module",
                    ),
                );
            }
        }
    }

    /// Validate a self-call against the current machine's branches.  Falls
    /// through silently if the machine isn't registered (only happens when
    /// the program failed to populate the registry — earlier errors will
    /// have been reported).
    fn validate_machine_call(
        &mut self,
        machine_name: &str,
        arg_types: &[String],
        call_span: SimpleSpan,
        is_self: bool,
    ) {
        let Some(entry) = self.registry.module(self.current_module).machines.get(machine_name)
        else {
            return;
        };
        let any_match = entry
            .branches
            .iter()
            .any(|b| sig_matches_args(b, arg_types));
        if !any_match {
            let display = if is_self { "self" } else { machine_name };
            self.errors.push(no_match_error(
                machine_name,
                &entry.branches,
                arg_types,
                call_span,
                is_self,
            ).label_callee(display));
        }
    }

    /// Check that an arithmetic operand has a known type.
    fn check_arith_operand(
        &mut self,
        machine_name: &'src str,
        scope: &HashMap<&'src str, Type<'src>>,
        operand: &Spanned<Expr<'src>>,
    ) {
        if operand_type_unknown(&operand.inner) {
            self.errors.push(
                LakeError::new("arithmetic on a value of unknown type", operand.span)
                    .code("E002")
                    .note("arithmetic requires operands of type `i64`")
                    .help("declare the variable in the branch parameters with a concrete type"),
            );
        }
        self.check_expr(machine_name, scope, &operand.inner, operand.span);
    }

    /// Type-display for an expression used as a call argument.  `"?"` means
    /// the resolver / inferer hasn't decided.
    fn expr_type_str(&self, expr: &Expr<'_>) -> String {
        match expr {
            Expr::Var(_, ty) => ty.to_string(),
            Expr::Num(_, ty) => ty.to_string(),
            Expr::String(_, ty) => ty.to_string(),
            Expr::Bool(_) => "bool".to_string(),
            Expr::Add(l, _) | Expr::Sub(l, _) | Expr::Mul(l, _) | Expr::Div(l, _) => {
                self.expr_type_str(&l.inner)
            }
            Expr::Neg(inner) => self.expr_type_str(&inner.inner),
            Expr::Eq(_, _)
            | Expr::Le(_, _)
            | Expr::Ge(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _)
            | Expr::BAnd(_, _)
            | Expr::BOr(_, _)
            | Expr::BXor(_, _)
            | Expr::Shl(_, _)
            | Expr::Shr(_, _) => "i64".to_string(),
            Expr::Atom(_) => "atom".to_string(),
            // Tuples surface here when passed as call arguments; the
            // signature side cannot yet describe a structured type, so
            // surface a coarse `tuple` token.  Refine when call-arg sigs
            // gain shape awareness.
            Expr::Tuple(_) => "tuple".to_string(),
            Expr::TupleIndex { receiver, index } => {
                // Resolve the field type from the receiver's `Struct(fields)`
                // annotation (the resolver fills `Var.ty` from let / wait-
                // handler scope).  Without this, `tuple.idx` always reports
                // `?` to call-arg matching, breaking Go-style error-handling
                // pipelines that thread `{atom value}` tuples through chains
                // of `let r = call(...)` + `when r.0 { ... call(r.1) ... }`.
                if let Expr::Var(_, Type::Struct(fields)) = &receiver.inner {
                    if let Some(field) = fields.get(*index) {
                        return field.inner.to_string();
                    }
                }
                "?".to_string()
            }
            Expr::Index { .. } => "i64".to_string(),
            Expr::Jump { ident, .. } => match &ident.inner {
                Expr::Var(name, _) => self
                    .registry
                    .resolve_bare_in(self.current_module, name)
                    .map(|r| r.return_type().to_string())
                    .unwrap_or_else(|| "?".to_string()),
                Expr::Path(segments) if segments.len() >= 2 => self
                    .registry
                    .resolve_path(self.current_module, segments)
                    .and_then(|(id, item)| self.registry.resolve_in_module(id, item))
                    .map(|r| r.return_type().to_string())
                    .unwrap_or_else(|| "?".to_string()),
                _ => "?".to_string(),
            },
            _ => "?".to_string(),
        }
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

fn sig_matches_args(sig: &Signature, args: &[String]) -> bool {
    if sig.params.len() != args.len() {
        return false;
    }
    sig.params
        .iter()
        .zip(args.iter())
        .all(|(p, a)| types_compatible(p, a))
}

/// Coarse type compatibility check used by call-arity validation.
///
/// At runtime Lake's `str` is a fat-pointer represented as an `i64`;
/// any function that takes a fat-pointer (rt_write, rt_recv_async,
/// rt_allocate-derived buffers, …) accepts both source-level kinds.
/// We accept the obvious overlap here so `rt_write(1 my_buf len)`
/// (where `my_buf` is `i64` from `rt_allocate`) and `rt_write(1
/// "hello" 5)` both pass.
fn types_compatible(param: &str, arg: &str) -> bool {
    if param == arg {
        return true;
    }
    // `str` and `buf` (#45) share the same fat-pointer runtime layout —
    // `str` for immutable literals, `buf` for heap-allocated mutable buffers.
    // Accept both directions for source-level ergonomics; the mutability
    // distinction is enforced statically by #76 (move/copy ownership).
    //
    // `i64` is a pure number and is NOT interchangeable with buffer types.
    // Code that needs to traffic raw addresses must explicitly type them as
    // `buf` (or in escape-hatch cases, declare a `buf` param).
    matches!((param, arg), ("buf", "str") | ("str", "buf"))
}

fn display_sig(sig: &Signature) -> String {
    if sig.params.is_empty() {
        "_".to_string()
    } else {
        sig.params.join(" ")
    }
}

fn no_match_error(
    machine_name: &str,
    sigs: &[Signature],
    arg_types: &[String],
    call_span: SimpleSpan,
    _is_self: bool,
) -> LakeError {
    let branch_list = sigs
        .iter()
        .map(|s| format!("`{}`", display_sig(s)))
        .collect::<Vec<_>>()
        .join(", ");
    let call_sig = if arg_types.is_empty() {
        "_".to_string()
    } else {
        arg_types.join(" ")
    };

    LakeError::with_label_msg(
        format!("no branch of `{machine_name}` matches the call `{machine_name}({call_sig})`"),
        call_span,
        format!("no branch accepts ({call_sig})"),
    )
    .code("E003")
    .note(format!(
        "available branch signatures for `{machine_name}`: {branch_list}"
    ))
    .help(
        "add a branch whose parameter types match the argument types, \
         or fix the argument types",
    )
}

trait LakeErrorExt {
    fn label_callee(self, callee: &str) -> LakeError;
}

impl LakeErrorExt for LakeError {
    /// Replace the headline's machine name with `callee` (e.g. swap to
    /// `self` for self-calls).  Cheaper than threading a flag through
    /// `no_match_error` parameters.
    fn label_callee(mut self, callee: &str) -> Self {
        if callee != "self" {
            return self;
        }
        // Replace "matches the call `name(...)`" → "matches the call `self(...)`"
        let original = std::mem::take(&mut self.message);
        if let Some(start) = original.find("matches the call `") {
            let prefix_end = start + "matches the call `".len();
            if let Some(paren) = original[prefix_end..].find('(') {
                let absolute_paren = prefix_end + paren;
                let mut new_msg = String::with_capacity(original.len());
                new_msg.push_str(&original[..prefix_end]);
                new_msg.push_str(callee);
                new_msg.push_str(&original[absolute_paren..]);
                self.message = new_msg;
                return self;
            }
        }
        self.message = original;
        self
    }
}

/// Walk an expression tree and add every `let`-introduced binding to
/// `scope`.  Recurses into bodies of when arms / wait handlers so a
/// `let` nested inside a control-flow construct is still visible to a
/// later sibling expression.
fn collect_let_bindings<'src>(
    expr: &Expr<'src>,
    scope: &mut HashMap<&'src str, Type<'src>>,
) {
    match expr {
        Expr::Let { ident, ty, default } => {
            scope.insert(ident.inner.0, ty.inner.clone());
            if let Some(d) = default {
                collect_let_bindings(&d.inner, scope);
            }
        }
        Expr::Jump { ident, args } => {
            collect_let_bindings(&ident.inner, scope);
            for a in args {
                collect_let_bindings(&a.inner, scope);
            }
        }
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Eq(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r) => {
            collect_let_bindings(&l.inner, scope);
            collect_let_bindings(&r.inner, scope);
        }
        Expr::Neg(inner) => collect_let_bindings(&inner.inner, scope),
        Expr::When { cond, branches } => {
            collect_let_bindings(&cond.inner, scope);
            for (_, body) in branches {
                for e in body {
                    collect_let_bindings(&e.inner, scope);
                }
            }
        }
        Expr::Wait { handlers, .. } => {
            for h in handlers {
                for e in &h.inner.body {
                    collect_let_bindings(&e.inner, scope);
                }
            }
        }
        _ => {}
    }
}

/// True when the operand's effective type cannot be determined, i.e. it is
/// `Type::Unknown` or recurses into an unknown leaf.
fn operand_type_unknown(expr: &Expr<'_>) -> bool {
    match expr {
        Expr::Var(_, ty) | Expr::Num(_, ty) | Expr::String(_, ty) => is_unknown(ty),
        Expr::Add(l, _) | Expr::Sub(l, _) | Expr::Mul(l, _) | Expr::Div(l, _) => {
            operand_type_unknown(&l.inner)
        }
        Expr::Neg(inner) => operand_type_unknown(&inner.inner),
        _ => false,
    }
}

// ── public entry points ────────────────────────────────────────────────────

/// Run the type checker over a single-file Lake program.  Used by callers
/// that didn't construct a [`ProgramRegistry`] (mostly tests / older
/// integrations); a fresh registry is built and discarded.
pub fn typecheck<'src>(program: Vec<Spanned<Item<'src>>>) -> LakeErrors {
    let parsed = ParsedProgram {
        modules: vec![crate::loader::ParsedModule {
            module_path: ModulePath::root(),
            source_path: std::path::PathBuf::from("<inline>"),
            ast: program,
        }],
    };
    let mut reg: ProgramRegistry<'src> = ProgramRegistry::with_rt();
    if let Err(errs) = reg.populate_from(&parsed) {
        return errs;
    }
    let mut all = Vec::new();
    for m in &parsed.modules {
        let id = reg.module_id_for_path(&m.module_path).expect("populated");
        let mut checker = TypeChecker::new(&reg, id);
        checker.check_module(&m.ast);
        all.extend(checker.into_errors());
    }
    LakeErrors::new(all)
}

/// Multi-module entry point: typecheck every module of a [`ParsedProgram`]
/// against a populated [`ProgramRegistry`].  All diagnostics are
/// accumulated into a single [`LakeErrors`].
pub fn typecheck_program<'src>(
    program: &ParsedProgram<'src>,
    registry: &ProgramRegistry<'src>,
) -> LakeErrors {
    let mut all = Vec::new();
    for module in &program.modules {
        let id = registry
            .module_id_for_path(&module.module_path)
            .expect("registry must be populated before typecheck");
        let mut checker = TypeChecker::new(registry, id);
        checker.check_module(&module.ast);
        // Bug #126: tag each error with its module's source file so
        // the multi-file renderer doesn't slice a foreign source.
        for mut e in checker.into_errors() {
            if e.source_path.is_none() {
                e.source_path = Some(module.source_path.clone());
            }
            all.push(e);
        }
    }
    LakeErrors::new(all)
}

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use super::{typecheck_program, ParsedProgram};
    use crate::{
        lexer::lexer,
        parser::program,
        registry::{ModulePath, ProgramRegistry},
        resolver::resolve_program,
    };

    /// Run the full pipeline (parse → populate → resolve_program → typecheck)
    /// on a single-file source, returning every diagnostic message.
    fn run_pipeline(src: &str) -> Vec<crate::error_handle::LakeError> {
        let tokens = lexer().parse(src).into_result().expect("lex error");
        let ast = program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse error");

        let parsed = ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("<inline>"),
                ast,
            }],
        };

        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&parsed).expect("populate");
        let resolved = resolve_program(parsed, &mut reg);
        typecheck_program(&resolved, &reg).0
    }

    /// Mirror the production pipeline in `prelude::build_program`:
    /// lower BEFORE resolve, then run typeck.  Needed for tests that
    /// stress let-RHS / `.N` typing on ret-machine call results — those
    /// only surface after lowering rewrites `let r = M(...)` into a
    /// pid_let + wait handler that binds `r` to the declared return
    /// shape.
    fn run_full_pipeline(src: &str) -> Vec<crate::error_handle::LakeError> {
        let tokens = lexer().parse(src).into_result().expect("lex error");
        let ast = program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse error");

        let parsed = ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("<inline>"),
                ast,
            }],
        };

        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&parsed).expect("populate");
        let parsed = crate::lowering::lower_program(parsed, &reg);
        reg.populate_from(&parsed).expect("populate post-lower");
        let resolved = resolve_program(parsed, &mut reg);
        typecheck_program(&resolved, &reg).0
    }

    fn check(src: &str) -> Vec<String> {
        run_pipeline(src)
            .into_iter()
            .map(|e| e.message)
            .collect()
    }

    fn check_codes(src: &str) -> Vec<String> {
        run_pipeline(src)
            .into_iter()
            .map(|e| e.code.unwrap_or_default())
            .collect()
    }

    /// Regression for the tuple-atom resolver bug: when a ret-machine
    /// returns a declared tuple like `{i64 buf}`, the `.N` field
    /// accesses on the call result must surface as the declared element
    /// types (`i64`, `buf`) — not get steamrolled to `{atom T}`.  Before
    /// the fix this raised E003 `no branch of \`route\` matches the
    /// call \`route(... atom ...)\``.  The inner-when shape matches the
    /// marine handler that motivated the bug (rt_allocate's atom guard
    /// nested around a ret-machine call whose `.N`s are then forwarded).
    #[test]
    fn tuple_index_on_ret_machine_struct_keeps_declared_types() {
        let src = r#"
@rt(rt_allocate)
parse_request is {
  req buf flen i64 -> ret { i64 buf } { ret { 1 req } }
}
route is { m i64 path buf -> ret i64 { ret 0 } }
caller is {
  conn i64 -> {
    let r = rt_allocate(4096)
    when r.0 {
      :ok -> {
        let b = r.1
        let req = parse_request(b 0)
        let resp = route(req.0 req.1)
      }
      _ -> { }
    }
  }
}
"#;
        let errs = run_full_pipeline(src);
        let bad: Vec<_> = errs
            .iter()
            .filter(|e| e.message.contains("no branch") && e.message.contains("route"))
            .collect();
        assert!(
            bad.is_empty(),
            "tuple-atom resolver bug: route(req.0 req.1) should typecheck against (i64 buf), got {bad:?}"
        );
    }

    /// Same shape as the marine bug, but with `req` shadowing an
    /// earlier `r {atom buf}` from `rt_allocate` — guards against the
    /// scope-leak path where the implicit-tuple shape contaminates the
    /// later declared-tuple binding.
    #[test]
    fn tuple_index_shadowed_by_declared_tuple_keeps_correct_types() {
        let src = r#"
@rt(rt_allocate)
parse_request is {
  req buf -> ret { i64 buf } { ret { 1 req } }
}
route is { m i64 path buf -> ret i64 { ret 0 } }
caller is {
  conn i64 -> {
    let r = rt_allocate(4096)
    when r.0 {
      :ok -> {
        let buf2 = r.1
        let r = parse_request(buf2)
        let resp = route(r.0 r.1)
      }
      _ -> { }
    }
  }
}
"#;
        let errs = run_full_pipeline(src);
        let bad: Vec<_> = errs
            .iter()
            .filter(|e| e.message.contains("no branch") && e.message.contains("route"))
            .collect();
        assert!(
            bad.is_empty(),
            "shadowed `r` should re-bind to parse_request's `{{i64 buf}}`, got {bad:?}"
        );
    }

    // ── E001: undeclared variable ─────────────────────────────────────────────

    #[test]
    fn e001_undeclared_variable() {
        let errs = check("counter is { _ -> { x } }");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("undeclared variable `x`"));
    }

    #[test]
    fn no_error_for_declared_variable() {
        let errs = check("counter is { n i64 -> { n } }");
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn no_error_for_self_keyword() {
        // `self` is a keyword; bare `self` should not trigger E001.
        let errs = check("counter is { _ -> { self } }");
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn e001_in_call_argument() {
        // `x` used as argument to a call without being declared.
        let errs = check("counter is { _ -> { foo(x) } }");
        assert!(!errs.is_empty());
        assert!(errs.iter().any(|e| e.contains("undeclared variable `x`")));
    }

    // ── E002: unknown type in arithmetic ─────────────────────────────────────

    #[test]
    fn e002_arithmetic_on_undeclared() {
        let codes = check_codes("counter is { _ -> { x + x } }");
        assert!(codes.iter().any(|c| c == "E002"), "codes: {codes:?}");
    }

    #[test]
    fn no_e002_for_declared_arithmetic() {
        let errs = check("counter is { n i64 -> { n + n } }");
        let e002: Vec<_> = errs.iter().filter(|e| e.contains("arithmetic")).collect();
        assert!(e002.is_empty(), "unexpected E002: {e002:?}");
    }

    // ── E003: self call with no matching branch ───────────────────────────────

    #[test]
    fn e003_self_call_no_matching_branch() {
        // Machine only has a branch taking `i64`; calling with no args has no match.
        let errs = check("counter is { n i64 -> { self() } }");
        assert!(!errs.is_empty());
        assert!(errs.iter().any(|e| e.contains("no branch")));
    }

    #[test]
    fn no_e003_self_call_matching_branch() {
        // `self(n)` where n: i64 matches the branch `n i64 -> ...`.
        let errs = check("counter is { n i64 -> { self(n) } }");
        let e003: Vec<_> = errs.iter().filter(|e| e.contains("no branch")).collect();
        assert!(e003.is_empty(), "unexpected E003: {e003:?}");
    }

    #[test]
    fn e003_code_is_set() {
        let codes = check_codes("counter is { n i64 -> { self() } }");
        assert!(codes.iter().any(|c| c == "E003"), "codes: {codes:?}");
    }

    // ── E004: unknown callable ────────────────────────────────────────────────

    #[test]
    fn e004_unknown_callable() {
        let codes = check_codes("main is { _ -> { not_a_thing(1) } }");
        assert!(
            codes.iter().any(|c| c == "E004"),
            "expected E004, got {codes:?}"
        );
    }

    // ── rt fn return-type lookup ──────────────────────────────────────────────

    #[test]
    fn rt_return_type_is_known() {
        // `rt_listen_tcp(80)` returns i64 per the registry.  `let s = ...`
        // should therefore type s as i64; calling `f(s)` against a
        // single-i64-branch machine should typecheck.
        let src = r#"
            @rt(rt_listen_tcp)
            f is { x i64 -> { } }
            main is { _ -> {
              let s = rt_listen_tcp(80)
              f(s)
            } }
        "#;
        let errs = check(src);
        assert!(errs.is_empty(), "unexpected: {errs:?}");
    }
}
