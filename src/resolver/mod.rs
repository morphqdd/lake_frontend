use std::collections::HashMap;

use chumsky::span::{SimpleSpan, SpanWrap, Spanned};

use crate::api::{
    ast::{Branch, Ident, Item, Machine, MachineItem, Type},
    expr::Expr,
};
use crate::loader::ParsedProgram;
use crate::registry::{ModuleId, ModulePath, ProgramRegistry};

/// True when the parser emitted a placeholder type that the resolver needs to
/// fill in.  `Type::Unit` (`{}` in source) is *not* unknown — it is a legitimate
/// concrete type.
pub(crate) fn is_unknown(ty: &Type<'_>) -> bool {
    matches!(ty, Type::Unknown)
}

/// Walks a parsed Lake AST and fills in `{}` placeholder types on variable
/// references with the actual declared type from the enclosing branch pattern
/// or `let` binding.
///
/// Each `Branch` gets its own scope that is discarded after the branch body is
/// processed, so variable bindings do not leak across branches.
///
/// The optional `registry` enables let-RHS inference: when a `let` lacks a
/// type annotation, the resolver looks at the right-hand side and consults
/// the registry for call return types (rt fns / machines / ffi).
pub struct Resolver<'src, 'r> {
    scope: HashMap<&'src str, Type<'src>>,
    registry: Option<&'r ProgramRegistry<'src>>,
    /// Module currently being resolved.  Used to disambiguate bare-name
    /// calls in let-RHS inference.  `ModuleId::ROOT` for single-file
    /// compilation.
    current_module: ModuleId,
}

impl Default for Resolver<'_, '_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'src, 'r> Resolver<'src, 'r> {
    pub fn new() -> Self {
        Self {
            scope: HashMap::new(),
            registry: None,
            current_module: ModuleId::ROOT,
        }
    }

    /// Build a registry-aware resolver pinned to a specific module.  Used
    /// by the multi-module pipeline so let-RHS inference can look up
    /// imported names in the right scope.
    pub fn with_registry(
        registry: &'r ProgramRegistry<'src>,
        current_module: ModuleId,
    ) -> Self {
        Self {
            scope: HashMap::new(),
            registry: Some(registry),
            current_module,
        }
    }

    fn bind(&mut self, name: &'src str, ty: Type<'src>) {
        self.scope.insert(name, ty);
    }

    fn resolve_var_ty(&self, name: &'src str, ty: Type<'src>) -> Type<'src> {
        if is_unknown(&ty) {
            self.scope.get(name).cloned().unwrap_or(ty)
        } else {
            ty
        }
    }

    /// Infer the static type of an already-resolved expression.  Returns
    /// [`Type::Unknown`] when the inference engine has nothing to say —
    /// callers should treat that as "user must annotate" rather than
    /// "compatible with anything".
    fn infer_expr_type(&self, expr: &Expr<'src>, span: SimpleSpan) -> Type<'src> {
        match expr {
            Expr::Num(_, _) => static_named("i64", span),
            Expr::String(_, _) => static_named("str", span),
            Expr::Bool(_) => static_named("bool", span),
            Expr::Unit => Type::Unit,
            Expr::Atom(_) => static_named("atom", span),
            Expr::Tuple(elems) => Type::Struct(
                elems
                    .iter()
                    .map(|e| self.infer_expr_type(&e.inner, e.span).with_span(e.span))
                    .collect(),
            ),
            Expr::TupleIndex { receiver, index } => {
                let recv_ty = self.infer_expr_type(&receiver.inner, receiver.span);
                if let Type::Struct(fields) = recv_ty {
                    fields.get(*index).map(|f| f.inner.clone()).unwrap_or(Type::Unknown)
                } else {
                    Type::Unknown
                }
            }
            Expr::Var(name, ty) => {
                if !is_unknown(ty) {
                    ty.clone()
                } else if let Some(t) = self.scope.get(name) {
                    t.clone()
                } else if let Some(reg) = self.registry {
                    use crate::registry::Resolution;
                    match reg.resolve_bare_in(self.current_module, name) {
                        Some(Resolution::Const(c)) => type_from_str(&c.ty, span),
                        _ => Type::Unknown,
                    }
                } else {
                    Type::Unknown
                }
            }
            // Arithmetic, negation, and comparisons all evaluate to i64.
            Expr::Add(_, _)
            | Expr::Sub(_, _)
            | Expr::Mul(_, _)
            | Expr::Div(_, _)
            | Expr::Neg(_)
            | Expr::Eq(_, _)
            | Expr::Le(_, _)
            | Expr::Ge(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _) => static_named("i64", span),
            Expr::Jump { ident, .. } => self.infer_jump_return(&ident.inner, span),
            // Higher-order shapes (let, when, wait, method-call, …) don't
            // have a defined "value" yet — leave them unresolved.
            _ => Type::Unknown,
        }
    }

    fn infer_jump_return(&self, callee: &Expr<'src>, span: SimpleSpan) -> Type<'src> {
        let Some(reg) = self.registry else {
            return Type::Unknown;
        };
        match callee {
            Expr::Var(name, _ty) => {
                // Bare-name lookup uses whatever module is currently being
                // resolved.  We can't borrow `reg` and rebuild current
                // mutably; resolve_bare consults `reg.current` so swap
                // current temporarily isn't an option here.  Instead use
                // `resolve_in_module` for the local module manually + fall
                // back to rt.
                if let Some(sig) = reg.rt_fns.get(*name) {
                    return type_from_str(&sig.ret, span);
                }
                if let Some(r) =
                    reg.resolve_in_module(self.current_module, name)
                {
                    return type_from_str(r.return_type(), span);
                }
                // Imported alias: walk the current module's import table.
                let scope = reg.module(self.current_module);
                if let Some(binding) = scope.imports.get(*name) {
                    if let Some(r) = reg.resolve_in_module(
                        binding.target_module,
                        &binding.target_item,
                    ) {
                        return type_from_str(r.return_type(), span);
                    }
                }
                Type::Unknown
            }
            Expr::Path(segments) => {
                if segments.len() < 2 {
                    return Type::Unknown;
                }
                let module_segs: Vec<String> = segments[..segments.len() - 1]
                    .iter()
                    .map(|s| s.inner.0.to_string())
                    .collect();
                let item = segments.last().expect("len >= 2").inner.0;
                let module_path = ModulePath(module_segs);
                let Some(target_id) = reg.module_id_for_path(&module_path) else {
                    return Type::Unknown;
                };
                reg.resolve_in_module(target_id, item)
                    .map(|r| type_from_str(r.return_type(), span))
                    .unwrap_or(Type::Unknown)
            }
            _ => Type::Unknown,
        }
    }

    fn resolve_expr(&mut self, expr: Spanned<Expr<'src>>) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Var(name, ty) => Expr::Var(name, self.resolve_var_ty(name, ty)),

            Expr::Let { ident, ty, default } => {
                // Pre-bind so the default can reference the binding (matches
                // existing semantics).
                self.bind(ident.inner.0, ty.inner.clone());
                let default = default.map(|d| Box::new(self.resolve_expr(*d)));

                // If the user did not write an annotation, infer the type
                // from the default's resolved expression.  Registry-aware
                // inference covers literals, arithmetic, and call returns.
                let final_ty_inner = if is_unknown(&ty.inner) {
                    if let Some(d) = &default {
                        self.infer_expr_type(&d.inner, d.span)
                    } else {
                        ty.inner.clone()
                    }
                } else {
                    ty.inner.clone()
                };

                // Re-bind under the inferred type so subsequent expressions
                // in this scope see the right type.
                self.bind(ident.inner.0, final_ty_inner.clone());

                let final_ty = final_ty_inner.with_span(ty.span);
                Expr::Let {
                    ident,
                    ty: final_ty,
                    default,
                }
            }

            Expr::Jump { ident, args } => {
                let ident = Box::new(self.resolve_expr(*ident));
                let args = args.into_iter().map(|a| self.resolve_expr(a)).collect();
                Expr::Jump { ident, args }
            }

            Expr::Add(l, r) => Expr::Add(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Neg(inner) => Expr::Neg(Box::new(self.resolve_expr(*inner))),
            Expr::Ret(inner) => Expr::Ret(Box::new(self.resolve_expr(*inner))),
            Expr::Pin(inner) => Expr::Pin(Box::new(self.resolve_expr(*inner))),

            Expr::When { cond, branches } => {
                let cond = Box::new(self.resolve_expr(*cond));
                let branches = branches
                    .into_iter()
                    .map(|(pat, body)| {
                        let pat = self.resolve_expr(pat);
                        let body = body.into_iter().map(|e| self.resolve_expr(e)).collect();
                        (pat, body)
                    })
                    .collect();
                Expr::When { cond, branches }
            }

            Expr::Wait { handlers, filter } => {
                let filter = filter
                    .into_iter()
                    .map(|f| self.resolve_expr(f))
                    .collect();
                let handlers = handlers
                    .into_iter()
                    .map(|h| {
                        let span = h.span;
                        self.resolve_branch(h.inner).with_span(span)
                    })
                    .collect();
                Expr::Wait { handlers, filter }
            }

            Expr::Tuple(elems) => Expr::Tuple(
                elems.into_iter().map(|e| self.resolve_expr(e)).collect(),
            ),
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(self.resolve_expr(*receiver)),
                index,
            },

            other => other,
        };
        inner.with_span(span)
    }

    fn resolve_branch(&mut self, branch: Branch<'src>) -> Branch<'src> {
        let saved = self.scope.clone();

        // Bind all non-wildcard, non-literal-guard pattern variables into scope.
        for pat in &branch.patterns {
            if !pat.inner.is_wildcard() && !pat.inner.is_literal_guard() {
                self.bind(pat.inner.ident.inner.0, pat.inner.ty.inner.clone());
            }
        }

        let body = branch
            .body
            .into_iter()
            .map(|e| self.resolve_expr(e))
            .collect();

        self.scope = saved;
        Branch::new(branch.label, branch.patterns, branch.ret_ty, body)
    }

    fn resolve_machine(&mut self, machine: Machine<'src>) -> Machine<'src> {
        let items = machine
            .items
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    MachineItem::Branch(b) => {
                        MachineItem::Branch(self.resolve_branch(b)).with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect();
        Machine::new(
            machine.attrs,
            machine.vis,
            machine.ident,
            machine.generics,
            items,
        )
    }

    /// Resolve types in a complete parsed program.
    pub fn resolve(&mut self, program: Vec<Spanned<Item<'src>>>) -> Vec<Spanned<Item<'src>>> {
        program
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    Item::Machine(m) => {
                        let machine_span = m.span;
                        Item::Machine(self.resolve_machine(m.inner).with_span(machine_span))
                            .with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect()
    }
}

/// Convenience wrapper: resolve types in a single-file Lake program.  Used
/// by callers that don't construct a [`ProgramRegistry`] (mostly tests).
pub fn resolve<'src>(program: Vec<Spanned<Item<'src>>>) -> Vec<Spanned<Item<'src>>> {
    Resolver::new().resolve(program)
}

/// Resolve every module of a multi-file program in registry-aware mode.
///
/// Each module is processed under its own [`Resolver`], which consults the
/// registry for let-RHS inference (call return types, alias bindings).
/// The registry is mutated only insofar as `current_module` is bumped per
/// iteration; machine / ffi / import entries remain unchanged.
pub fn resolve_program<'src>(
    mut program: ParsedProgram<'src>,
    registry: &mut ProgramRegistry<'src>,
) -> ParsedProgram<'src> {
    let modules = std::mem::take(&mut program.modules);
    let mut resolved_modules = Vec::with_capacity(modules.len());

    for module in modules {
        let id = registry
            .module_id_for_path(&module.module_path)
            .expect("populate_from must run before resolve_program");
        registry.set_current(id);

        let mut resolver = Resolver::with_registry(registry, id);
        let resolved_ast = resolver.resolve(module.ast);

        resolved_modules.push(crate::loader::ParsedModule {
            module_path: module.module_path,
            source_path: module.source_path,
            ast: resolved_ast,
        });
    }

    program.modules = resolved_modules;
    program
}

// ── inference helpers ──────────────────────────────────────────────────────

/// Build a `Type::Named` from a `&'static` name plus a span.  Used by the
/// inference engine where the type's textual identifier is fixed at compile
/// time (e.g. `"i64"` from arithmetic).
fn static_named<'src>(name: &'static str, span: SimpleSpan) -> Type<'src> {
    Type::Named(Ident::new(name).with_span(span))
}

/// Render a registry-supplied return-type string into a [`Type`].  Only the
/// small set of types Lake currently exposes through call signatures
/// (i64 / str / bool / pid / unit) round-trip; richer return types stay
/// unresolved so the caller can fall back to "user must annotate".
fn type_from_str<'src>(s: &str, span: SimpleSpan) -> Type<'src> {
    match s {
        "i64" => static_named("i64", span),
        "str" => static_named("str", span),
        "bool" => static_named("bool", span),
        "pid" => static_named("pid", span),
        "{}" => Type::Unit,
        _ => Type::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use super::resolve;
    use crate::{
        api::{
            ast::{Ident, Item, MachineItem, Type},
            expr::Expr,
        },
        lexer::lexer,
        parser::program,
    };

    fn parse_resolve(src: &str) -> Vec<chumsky::span::Spanned<Item<'_>>> {
        let tokens = lexer().parse(src).into_result().expect("lex error");
        let ast = program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse error");
        resolve(ast)
    }

    fn first_branch_body(src: &str) -> Vec<Expr<'_>> {
        let ast = parse_resolve(src);
        let Item::Machine(m) = &ast[0].inner else {
            panic!("expected Machine")
        };
        let MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!("expected Branch")
        };
        b.body.iter().map(|e| e.inner.clone()).collect()
    }

    #[test]
    fn resolves_var_type_from_pattern() {
        // `n` is declared as `i64` in the pattern; the body just returns `n`.
        let body = first_branch_body("counter is { n i64 -> { n } }");
        let Expr::Var(name, ty) = &body[0] else {
            panic!("expected Var")
        };
        assert_eq!(*name, "n");
        assert!(
            matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected i64, got {ty:?}"
        );
    }

    #[test]
    fn resolves_var_type_in_self_call_args() {
        // `self(n)` where `n: i64` — the argument should be resolved to `i64`.
        let body = first_branch_body("counter is { n i64 -> { self(n) } }");
        let Expr::Jump { ident, args } = &body[0] else {
            panic!("expected Jump")
        };
        // callee is `self`
        assert!(matches!(ident.inner, Expr::Var("self", _)));
        // argument type is resolved
        let Expr::Var(_, ty) = &args[0].inner else {
            panic!("expected Var arg")
        };
        assert!(
            matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected i64, got {ty:?}"
        );
    }

    #[test]
    fn resolves_var_type_in_regular_call_args() {
        let body = first_branch_body("counter is { n i64 -> { foo(n) } }");
        let Expr::Jump { args, .. } = &body[0] else {
            panic!("expected Jump")
        };
        let Expr::Var(_, ty) = &args[0].inner else {
            panic!("expected Var arg")
        };
        assert!(matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")));
    }

    #[test]
    fn unknown_var_stays_unknown() {
        // `x` is not declared anywhere; its type stays as `{}`.
        let body = first_branch_body("counter is { _ -> { x } }");
        let Expr::Var(name, ty) = &body[0] else {
            panic!("expected Var")
        };
        assert_eq!(*name, "x");
        assert!(is_unknown_ty(ty), "expected {{}} placeholder, got {ty:?}");
    }

    #[test]
    fn scope_does_not_leak_between_branches() {
        // `n` is in scope for the first branch but not the second.
        let src = "counter is { n i64 -> { n } _ -> { n } }";
        let ast = parse_resolve(src);
        let Item::Machine(m) = &ast[0].inner else {
            panic!()
        };
        // first branch: n should be i64
        let MachineItem::Branch(b0) = &m.inner.items[0].inner else {
            panic!()
        };
        let Expr::Var(_, ty0) = &b0.body[0].inner else {
            panic!()
        };
        assert!(matches!(ty0, Type::Named(i) if i.inner == Ident::new("i64")));

        // second branch: n is not declared, stays {}
        let MachineItem::Branch(b1) = &m.inner.items[1].inner else {
            panic!()
        };
        let Expr::Var(_, ty1) = &b1.body[0].inner else {
            panic!()
        };
        assert!(
            is_unknown_ty(ty1),
            "expected {{}} in second branch, got {ty1:?}"
        );
    }

    fn is_unknown_ty(ty: &Type<'_>) -> bool {
        super::is_unknown(ty)
    }

    fn parsed_module(src: &str) -> crate::loader::ParsedModule<'_> {
        use crate::lexer::lexer;
        use crate::parser::program;
        let tokens = lexer().parse(src).into_result().expect("lex");
        let ast = program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse");
        crate::loader::ParsedModule {
            module_path: crate::registry::ModulePath::root(),
            source_path: std::path::PathBuf::from("main.lake"),
            ast,
        }
    }

    /// Resolve `src` and return the first branch body of the LAST machine
    /// declared (most tests put the assertion target last for readability).
    fn resolve_with_reg(src: &str) -> Vec<Expr<'_>> {
        let module = parsed_module(src);
        let program = crate::loader::ParsedProgram { modules: vec![module] };
        let mut reg: crate::registry::ProgramRegistry<'_> =
            crate::registry::ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populate");
        let resolved = super::resolve_program(program, &mut reg);
        let m = resolved.root();
        let machine = m
            .ast
            .iter()
            .filter_map(|it| match &it.inner {
                Item::Machine(m) => Some(m),
                _ => None,
            })
            .last()
            .expect("at least one machine");
        let MachineItem::Branch(b) = &machine.inner.items[0].inner else {
            panic!()
        };
        b.body.iter().map(|e| e.inner.clone()).collect()
    }

    #[test]
    fn infers_let_type_from_int_literal() {
        let body = resolve_with_reg("m is { _ -> { let n = 42 } }");
        let Expr::Let { ty, .. } = &body[0] else {
            panic!()
        };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "got {ty:?}"
        );
    }

    #[test]
    fn infers_let_type_from_string_literal() {
        let body = resolve_with_reg("m is { _ -> { let s = \"hi\" } }");
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("str")),
            "got {ty:?}"
        );
    }

    #[test]
    fn infers_let_type_from_bool_compare() {
        let body = resolve_with_reg("m is { n i64 -> { let b = 0 == n } }");
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        // Comparisons always evaluate to i64 (0 / 1).
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "got {ty:?}"
        );
    }

    #[test]
    fn infers_let_type_from_arith() {
        let body = resolve_with_reg("m is { n i64 -> { let r = n - n / 3 * 3 } }");
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "got {ty:?}"
        );
    }

    #[test]
    fn infers_let_type_from_rt_call() {
        let body = resolve_with_reg(
            "@rt(rt_listen_tcp)\nm is { _ -> { let s = rt_listen_tcp(8080) } }",
        );
        // `rt_listen_tcp` returns i64 per the static rt registry.
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "got {ty:?}"
        );
    }

    #[test]
    fn infers_let_type_from_machine_call_as_pid() {
        let body = resolve_with_reg(
            "worker is { n i64 -> { } }\nm is { _ -> { let p = worker(5) } }",
        );
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("pid")),
            "got {ty:?}"
        );
    }

    #[test]
    fn explicit_annotation_is_preserved() {
        let body = resolve_with_reg("m is { _ -> { let s str = \"hi\" } }");
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("str")),
            "got {ty:?}"
        );
    }

    #[test]
    fn resolves_var_in_wait_handler() {
        // `n` from the outer branch pattern should be accessible inside wait handler body
        let ast = parse_resolve("counter is { n i64 -> { wait { @inc amount i64 -> { n } } } }");
        let Item::Machine(m) = &ast[0].inner else {
            panic!()
        };
        let MachineItem::Branch(b) = &m.inner.items[0].inner else {
            panic!()
        };
        let Expr::Wait { handlers, .. } = &b.body[0].inner else {
            panic!("expected Wait")
        };
        // `n` inside handler body should be resolved to i64
        let Expr::Var(_, ty) = &handlers[0].inner.body[0].inner else {
            panic!("expected Var")
        };
        assert!(
            matches!(ty, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected i64, got {ty:?}"
        );
    }

    #[test]
    fn const_var_reference_resolves_to_const_type() {
        let body = resolve_with_reg(
            "const N = 42\nm is { _ -> { let r = N } }",
        );
        let Expr::Let { ty, .. } = &body[0] else {
            panic!("expected Let, got {:?}", body[0])
        };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "got {ty:?}"
        );
    }

    #[test]
    fn const_string_reference_resolves_to_str() {
        let body = resolve_with_reg(
            r#"const HELLO = "hi"
m is { _ -> { let r = HELLO } }"#,
        );
        let Expr::Let { ty, .. } = &body[0] else {
            panic!("expected Let, got {:?}", body[0])
        };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("str")),
            "got {ty:?}"
        );
    }

    #[test]
    fn infers_let_type_from_atom() {
        let body = resolve_with_reg("m is { _ -> { let r = :ok } }");
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("atom")),
            "got {ty:?}"
        );
    }

    #[test]
    fn infers_let_type_from_tuple() {
        let body = resolve_with_reg(r#"m is { _ -> { let p = { 1 "alice" } } }"#);
        let Expr::Let { ty, .. } = &body[0] else { panic!() };
        let Type::Struct(fields) = &ty.inner else {
            panic!("expected Struct, got {:?}", ty.inner)
        };
        assert_eq!(fields.len(), 2);
        assert!(matches!(&fields[0].inner, Type::Named(i) if i.inner == Ident::new("i64")));
        assert!(matches!(&fields[1].inner, Type::Named(i) if i.inner == Ident::new("str")));
    }

    #[test]
    fn infers_let_type_from_tuple_index() {
        let body = resolve_with_reg(
            r#"m is { _ -> { let p = { 1 "alice" } let n = p.0 } }"#,
        );
        let Expr::Let { ty, .. } = &body[1] else {
            panic!("expected second Let, got {:?}", body[1])
        };
        assert!(
            matches!(&ty.inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "got {ty:?}"
        );
    }
}
