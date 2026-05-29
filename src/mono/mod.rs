//! Generics monomorphisation pass — feature #142, phases 2-3.
//!
//! See docs/state/features/142_generics.md.
//!
//! Runs between the first [`crate::registry::ProgramRegistry::populate_from`]
//! call and [`crate::lowering::lower_program`].  Walks every non-generic
//! item, infers type arguments at each call site to a generic record or
//! machine, queues unique (item, subst) instantiations, and emits a
//! concrete copy per instantiation with TypeVar references replaced by
//! the substituted concrete types.  After the pass the AST contains
//! zero generic templates and zero `Type::NamedGeneric` / `Type::TypeVar`
//! nodes — the rest of the pipeline can pretend generics never existed.
//!
//! Naming: `Box_i64`, `Pair_i64_str`, `unbox_i64` (positional, declaration
//! order).  Identical post-mono bodies dedupe through the `(name, args)`
//! key on the mono cache.
//!
//! Diagnostics: E040 surfaces when call-site inference cannot resolve a
//! type parameter (e.g. nullary generic ctor).
use std::collections::{HashMap, HashSet};

use chumsky::span::{SimpleSpan, SpanWrap, Spanned};

use crate::api::{
    ast::{
        Branch, EnumDecl, Ident, Item, Machine, MachineItem, Pattern, PatternKind, RecordDecl,
        RecordField, Type, Variant, VariantPayload,
    },
    expr::Expr,
};
use crate::error_handle::LakeError;
use crate::loader::{ParsedModule, ParsedProgram};
use crate::registry::{ModuleId, ProgramRegistry, Resolution};

/// Substitution map from type-parameter source name (e.g. `"T"`) to its
/// concrete type.  Built per call site by [`MonoCx::unify`].
type Subst<'src> = HashMap<&'src str, Type<'src>>;

/// Run the monomorphisation pass.  Emits errors via the returned vector;
/// the program is always returned (even on partial failure) so callers
/// can keep accumulating diagnostics across passes.
pub fn monomorphise_program<'src>(
    program: ParsedProgram<'src>,
    registry: &ProgramRegistry<'src>,
) -> (ParsedProgram<'src>, Vec<LakeError>) {
    let mut cx = MonoCx::new(registry);
    let program = cx.run(program);
    (program, cx.errors)
}

struct MonoCx<'src, 'a> {
    registry: &'a ProgramRegistry<'src>,
    /// Mono cache: (base_name, arg_type_strings) → mangled name.  Keyed
    /// by name rather than `ModuleId` because Lake's flat symbol space
    /// is already globally unique per source name (T028 enforces that
    /// at the registry layer).
    mono_records: HashMap<(String, Vec<String>), String>,
    mono_machines: HashMap<(String, Vec<String>), String>,
    /// agent: generics-142 phase 4 — enum mono cache.  Same key shape
    /// as records / machines so an `Option[i64]` site shares the
    /// `Option_i64` mangled name across modules.
    mono_enums: HashMap<(String, Vec<String>), String>,
    /// Pending mono jobs — drained until empty after the first walk
    /// pass.  Each job re-enters the walker so transitive mono'd calls
    /// surface fresh sub-instantiations.
    work: Vec<MonoJob<'src>>,
    /// Generic record templates indexed by source name.
    record_templates: HashMap<String, RecordDecl<'src>>,
    /// Generic machine templates indexed by source name.  A name can
    /// map to MORE THAN ONE template when distinct modules define a
    /// same-named generic (e.g. `std.option.unwrap_or[T]` vs
    /// `std.result.unwrap_or[T E]`) — Lake's symbol space is flat but
    /// module-aware dispatch resolves the right one per call site.
    /// Single-entry names (the common case) behave exactly as a plain
    /// map.  Each entry carries its owning module.
    machine_templates: HashMap<String, Vec<(ModuleId, Machine<'src>)>>,
    /// agent: generics-142 phase 4 — generic enum templates indexed by
    /// source name.  Populated in pass 1; consulted during walk_expr
    /// when rewriting `Enum.Variant(args)` constructors and Let-bound
    /// `NamedGeneric` type annotations.
    enum_templates: HashMap<String, EnumDecl<'src>>,
    /// Module owning each generic template — used so we emit the
    /// concrete copy back into the same module the template lived in.
    template_module: HashMap<String, ModuleId>,
    /// Mono'd item names whose source-borrowed `&'src str` we need to
    /// keep alive for the rest of compilation.  Leaked once per name.
    leaked: HashSet<String>,
    /// agent: generics-142 phase 4 — per-enum-name ordered type-arg
    /// stack.  Pushed by Let walks when the annotation is a known
    /// generic enum's `NamedGeneric`; consulted by the variant ctor
    /// rewrite for sites like `Option.None` where no payload args
    /// would otherwise pin down T.  Popped on exit.
    enum_hints: HashMap<&'src str, Vec<Vec<Type<'src>>>>,
    /// #88 — expected return type of the current zero-arg generic call,
    /// taken from the enclosing `let x T = f()` annotation.  Lets a
    /// no-argument generic constructor like `vec_new[T]` (`_ -> ret
    /// Vec[T]`) recover `T` by unifying its declared return against the
    /// binding's annotation.  Set only around a zero-arg default walk.
    ret_hint: Option<Type<'src>>,
    errors: Vec<LakeError>,
}

#[derive(Debug, Clone)]
enum MonoJob<'src> {
    Record {
        base: String,
        args: Vec<Type<'src>>,
        mangled: &'src str,
    },
    Machine {
        base: String,
        args: Vec<Type<'src>>,
        mangled: &'src str,
        /// Owning module of the chosen template — disambiguates
        /// same-named generics across modules (e.g. two `unwrap_or`).
        module: ModuleId,
    },
    /// agent: generics-142 phase 4 — concrete enum instance to emit.
    Enum {
        base: String,
        args: Vec<Type<'src>>,
        mangled: &'src str,
    },
}

impl<'src, 'a> MonoCx<'src, 'a> {
    fn new(registry: &'a ProgramRegistry<'src>) -> Self {
        Self {
            registry,
            mono_records: HashMap::new(),
            mono_machines: HashMap::new(),
            mono_enums: HashMap::new(),
            work: Vec::new(),
            record_templates: HashMap::new(),
            machine_templates: HashMap::new(),
            enum_templates: HashMap::new(),
            template_module: HashMap::new(),
            leaked: HashSet::new(),
            enum_hints: HashMap::new(),
            ret_hint: None,
            errors: Vec::new(),
        }
    }

    /// Leak a name string with `'src` lifetime.  All identifier slots in
    /// the AST are `&'src str`, so a synthesized name must outlive the
    /// borrow on source text.  Idempotent — re-leaking the same string
    /// returns the previously leaked instance.
    fn leak_name(&mut self, s: String) -> &'src str {
        // Idempotent leak: insert into the set first; we then leak a
        // fresh copy that lives for the program's lifetime.  Using
        // `Box::leak` is the only way to get a `&'static str` (which
        // up-casts to `&'src str`) from a runtime-built name.
        self.leaked.insert(s.clone());
        let boxed: Box<str> = s.into_boxed_str();
        Box::leak(boxed)
    }

    fn run(&mut self, mut program: ParsedProgram<'src>) -> ParsedProgram<'src> {
        // Pass 1: index templates by name + module.
        for module in &program.modules {
            let module_id = self
                .registry
                .module_id_for_path(&module.module_path)
                .expect("populate_from must run before mono");
            for item in &module.ast {
                match &item.inner {
                    Item::Record(d) if !d.inner.type_params.is_empty() => {
                        let name = d.inner.ident.inner.0.to_string();
                        self.record_templates.insert(name.clone(), d.inner.clone());
                        self.template_module.insert(name, module_id);
                    }
                    Item::Machine(m) if !m.inner.generics.is_empty() => {
                        let name = m.inner.ident.inner.0.to_string();
                        self.machine_templates
                            .entry(name.clone())
                            .or_default()
                            .push((module_id, m.inner.clone()));
                        // template_module keeps the LAST owner for the
                        // legacy single-entry path; the per-instance
                        // owner now travels in MonoJob::Machine.module.
                        self.template_module.insert(name, module_id);
                    }
                    // agent: generics-142 phase 4 — index generic enums
                    // alongside records / machines so call sites that
                    // mention `Option[T]` resolve via the same path.
                    Item::Enum(d) if !d.inner.type_params.is_empty() => {
                        let name = d.inner.name.inner.0.to_string();
                        self.enum_templates.insert(name.clone(), d.inner.clone());
                        self.template_module.insert(name, module_id);
                    }
                    _ => {}
                }
            }
        }

        // agent: protocols-146 — drift lock.  Every explicit `X is Eq`
        // assertion must hold even if X never appears in a bounded
        // generic.  Verify each up front so removing a required machine
        // surfaces at the impl site.
        let impls: Vec<(String, String, SimpleSpan)> = self
            .registry
            .all_impls()
            .flat_map(|i| {
                i.protos
                    .iter()
                    .map(move |p| (i.ty.to_string(), p.to_string(), i.span))
            })
            .collect();
        for (ty, proto, span) in impls {
            self.check_proto_satisfied(&proto, &ty, span);
        }

        // Pass 2: walk every non-generic machine, rewriting call sites.
        let mut modules: Vec<ParsedModule<'src>> = Vec::with_capacity(program.modules.len());
        for module in program.modules.drain(..) {
            let module_id = self
                .registry
                .module_id_for_path(&module.module_path)
                .expect("populate_from must run before mono");
            let ast: Vec<Spanned<Item<'src>>> = module
                .ast
                .into_iter()
                .filter_map(|it| {
                    let span = it.span;
                    match it.inner {
                        // Strip generic templates from the output —
                        // concrete copies queued below replace them.
                        Item::Record(d) if !d.inner.type_params.is_empty() => None,
                        Item::Machine(m) if !m.inner.generics.is_empty() => None,
                        Item::Enum(d) if !d.inner.type_params.is_empty() => None,
                        Item::Machine(m) => {
                            let m_span = m.span;
                            let new_machine = self.walk_machine(m.inner, module_id);
                            Some(
                                Item::Machine(new_machine.with_span(m_span)).with_span(span),
                            )
                        }
                        other => Some(other.with_span(span)),
                    }
                })
                .collect();
            modules.push(ParsedModule {
                module_path: module.module_path,
                source_path: module.source_path,
                ast,
            });
        }

        // Pass 3: drain the worklist.  Each emitted concrete copy is
        // itself walked so it can spawn further mono jobs.
        while let Some(job) = self.work.pop() {
            match job {
                MonoJob::Record { base, args, mangled } => {
                    let module_id = *self
                        .template_module
                        .get(&base)
                        .expect("record template module");
                    let new_record = self.instantiate_record(&base, &args, mangled);
                    // Append to the owning module's AST.
                    let module = modules
                        .iter_mut()
                        .find(|m| {
                            self.registry
                                .module_id_for_path(&m.module_path)
                                .map(|id| id == module_id)
                                .unwrap_or(false)
                        })
                        .expect("owning module present");
                    let span = new_record.ident.span;
                    module.ast.push(
                        Item::Record(new_record.with_span(span)).with_span(span),
                    );
                }
                MonoJob::Machine { base, args, mangled, module } => {
                    let module_id = module;
                    let new_machine = self.instantiate_machine(&base, &args, mangled, module_id);
                    let module = modules
                        .iter_mut()
                        .find(|m| {
                            self.registry
                                .module_id_for_path(&m.module_path)
                                .map(|id| id == module_id)
                                .unwrap_or(false)
                        })
                        .expect("owning module present");
                    let span = new_machine.ident.span;
                    module.ast.push(
                        Item::Machine(new_machine.with_span(span)).with_span(span),
                    );
                }
                MonoJob::Enum { base, args, mangled } => {
                    let module_id = *self
                        .template_module
                        .get(&base)
                        .expect("enum template module");
                    let new_enum = self.instantiate_enum(&base, &args, mangled);
                    let module = modules
                        .iter_mut()
                        .find(|m| {
                            self.registry
                                .module_id_for_path(&m.module_path)
                                .map(|id| id == module_id)
                                .unwrap_or(false)
                        })
                        .expect("owning module present");
                    let span = new_enum.name.span;
                    module.ast.push(
                        Item::Enum(new_enum.with_span(span)).with_span(span),
                    );
                }
            }
        }

        ParsedProgram { modules }
    }

    fn walk_machine(&mut self, machine: Machine<'src>, module_id: ModuleId) -> Machine<'src> {
        let Machine { attrs, vis, ident, generics, bounds, items } = machine;
        let new_items = items
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    MachineItem::Branch(b) => {
                        let new_branch = self.walk_branch(b, module_id, &generics);
                        MachineItem::Branch(new_branch).with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect();
        Machine {
            attrs,
            vis,
            ident,
            generics,
            bounds,
            items: new_items,
        }
    }

    /// Walk one branch.  `type_params` is the enclosing decl's declared
    /// generics — used so type references inside this branch are *not*
    /// treated as type variables (we only mono outwards from concrete
    /// callers).  For non-generic decls the list is empty.
    fn walk_branch(
        &mut self,
        branch: Branch<'src>,
        module_id: ModuleId,
        _enclosing_generics: &[Spanned<Ident<'src>>],
    ) -> Branch<'src> {
        let Branch { label, patterns, ret_ty, body } = branch;
        // #88 — fold concrete generic annotations in param + return
        // types (e.g. `v Vec[i64]` → `v Vec_i64`) so they match the
        // mono'd record/enum names that call sites resolve to.  Empty
        // subst: only already-concrete `NamedGeneric`s collapse; bare
        // type-vars (in a still-generic template) pass through.
        let empty: Subst<'src> = HashMap::new();
        let patterns: Vec<Spanned<Pattern<'src>>> = patterns
            .into_iter()
            .map(|p| {
                let span = p.span;
                let Pattern { ident, ty, kind } = p.inner;
                let new_ty = self.substitute_type(&ty.inner, &empty).with_span(ty.span);
                Pattern { ident, ty: new_ty, kind }.with_span(span)
            })
            .collect();
        let ret_ty = ret_ty.map(|t| {
            let sp = t.span;
            self.substitute_type(&t.inner, &empty).with_span(sp)
        });
        // Build a per-branch scope: pattern binders → declared type.
        let mut scope: HashMap<&'src str, Type<'src>> = HashMap::new();
        for pat in &patterns {
            if !pat.inner.is_wildcard() && !pat.inner.is_literal_guard() {
                scope.insert(pat.inner.ident.inner.0, pat.inner.ty.inner.clone());
            }
        }
        let new_body: Vec<Spanned<Expr<'src>>> = body
            .into_iter()
            .map(|e| self.walk_expr(e, &mut scope, module_id))
            .collect();
        Branch::new(label, patterns, ret_ty, new_body)
    }

    fn walk_expr(
        &mut self,
        expr: Spanned<Expr<'src>>,
        scope: &mut HashMap<&'src str, Type<'src>>,
        module_id: ModuleId,
    ) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Let { ident, ty, default } => {
                // agent: generics-142 phase 4 — when the user writes
                // `let r Result[i64 buf] = ...`, fold the NamedGeneric
                // annotation through `substitute_type` (empty subst).
                // That walk queues the matching enum / record mono job
                // and returns a `Type::Named(<mangled>)` we can stash
                // in scope.  Mirror it for the side-effect of pushing
                // an enum hint so the default expr's bare
                // `Enum.Variant(...)` ctor sees the right concrete
                // arg list.
                let ty_span = ty.span;
                let (new_ty_inner, enum_hint_key) = match ty.inner.clone() {
                    Type::NamedGeneric { name, args }
                        if self.enum_templates.contains_key(name.inner.0) =>
                    {
                        let arg_tys: Vec<Type<'src>> =
                            args.iter().map(|a| a.inner.clone()).collect();
                        let folded =
                            self.substitute_type(&Type::NamedGeneric { name: name.clone(), args }, &HashMap::new());
                        // Stash hint under enum source name.
                        let key = name.inner.0;
                        self.enum_hints
                            .entry(key)
                            .or_default()
                            .push(arg_tys);
                        (folded, Some(key))
                    }
                    Type::NamedGeneric { name, args }
                        if self.record_templates.contains_key(name.inner.0) =>
                    {
                        // Records get folded too — keeps Let.ty in
                        // sync with the call-site rewrite below.
                        let folded = self.substitute_type(
                            &Type::NamedGeneric { name, args },
                            &HashMap::new(),
                        );
                        (folded, None)
                    }
                    other => (other, None),
                };
                let new_ty = new_ty_inner.clone().with_span(ty_span);
                // #88 — if the default is a zero-arg generic call
                // (`vec_new()`), expose the binding's annotation as the
                // return hint so the callee can recover its type param
                // from the declared return.  Only zero-arg defaults, so
                // no nested call can mis-consume the hint.
                let set_ret_hint = matches!(
                    default.as_deref().map(|d| &d.inner),
                    Some(Expr::Jump { args, .. }) if args.is_empty()
                ) && !matches!(new_ty_inner, Type::Unknown);
                if set_ret_hint {
                    self.ret_hint = Some(new_ty_inner.clone());
                }
                let default = default.map(|d| Box::new(self.walk_expr(*d, scope, module_id)));
                if set_ret_hint {
                    self.ret_hint = None;
                }
                // Pop the hint after the default has been walked.
                if let Some(key) = enum_hint_key {
                    if let Some(stack) = self.enum_hints.get_mut(key) {
                        stack.pop();
                        if stack.is_empty() {
                            self.enum_hints.remove(key);
                        }
                    }
                }
                // Infer the binding's type so later references see it.
                // Use declared `ty` (post-mono) when present; else
                // infer from default.
                let bound_ty = if matches!(new_ty.inner, Type::Unknown) {
                    default
                        .as_ref()
                        .map(|d| self.infer_expr_type(&d.inner, scope))
                        .unwrap_or(Type::Unknown)
                } else {
                    new_ty.inner.clone()
                };
                scope.insert(ident.inner.0, bound_ty);
                Expr::Let { ident, ty: new_ty, default }
            }
            Expr::Jump { ident, args } => {
                // Rewrite args first.
                let args: Vec<_> = args
                    .into_iter()
                    .map(|a| self.walk_expr(a, scope, module_id))
                    .collect();
                // agent: generics-142 phase 4 — variant constructor on
                // a generic enum: `Option.Some(42)` / `Result.Ok(7)`.
                // Rewrite the receiver from the generic head to its
                // mono'd name BEFORE the resolver gets to see it.
                let new_ident = if let Some(rewritten) =
                    self.maybe_rewrite_enum_dot_ctor(&ident, &args, scope, span, /*has_args=*/true)
                {
                    rewritten
                } else {
                    self.maybe_rewrite_callee(*ident, &args, scope, module_id, span)
                };
                Expr::Jump {
                    ident: Box::new(new_ident),
                    args,
                }
            }
            // agent: generics-142 phase 4 — nullary variant ctor like
            // `Option.None`.  Pure DotAccess with no Jump.  Same
            // rewrite, no args to drive inference — relies entirely on
            // an `enum_hints` entry pushed by the enclosing Let.
            Expr::DotAccess { receiver, field } => {
                let synthetic_callee = Expr::DotAccess {
                    receiver: receiver.clone(),
                    field: field.clone(),
                }
                .with_span(span);
                if let Some(rewritten) = self.maybe_rewrite_enum_dot_ctor(
                    &synthetic_callee,
                    &[],
                    scope,
                    span,
                    /*has_args=*/ false,
                ) {
                    rewritten.inner
                } else {
                    Expr::DotAccess {
                        receiver: Box::new(self.walk_expr(*receiver, scope, module_id)),
                        field,
                    }
                }
            }
            Expr::Add(l, r) => Expr::Add(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Neg(i) => Expr::Neg(Box::new(self.walk_expr(*i, scope, module_id))),
            Expr::Eq(l, r) => Expr::Eq(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Le(l, r) => Expr::Le(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Ge(l, r) => Expr::Ge(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Lt(l, r) => Expr::Lt(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Gt(l, r) => Expr::Gt(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::BAnd(l, r) => Expr::BAnd(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::BOr(l, r) => Expr::BOr(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::BXor(l, r) => Expr::BXor(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Shl(l, r) => Expr::Shl(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Shr(l, r) => Expr::Shr(
                Box::new(self.walk_expr(*l, scope, module_id)),
                Box::new(self.walk_expr(*r, scope, module_id)),
            ),
            Expr::Ret(i) => Expr::Ret(Box::new(self.walk_expr(*i, scope, module_id))),
            Expr::Pin(i) => Expr::Pin(Box::new(self.walk_expr(*i, scope, module_id))),
            Expr::When { cond, branches } => {
                let cond = Box::new(self.walk_expr(*cond, scope, module_id));
                let branches = branches
                    .into_iter()
                    .map(|(pat, body)| {
                        let mut arm_scope = scope.clone();
                        let pat = self.walk_expr(pat, &mut arm_scope, module_id);
                        let body = body
                            .into_iter()
                            .map(|e| self.walk_expr(e, &mut arm_scope, module_id))
                            .collect();
                        (pat, body)
                    })
                    .collect();
                Expr::When { cond, branches }
            }
            Expr::Wait { handlers, filter } => {
                let filter = filter
                    .into_iter()
                    .map(|f| self.walk_expr(f, scope, module_id))
                    .collect();
                let handlers = handlers
                    .into_iter()
                    .map(|h| {
                        let h_span = h.span;
                        let new_branch = self.walk_branch(h.inner, module_id, &[]);
                        new_branch.with_span(h_span)
                    })
                    .collect();
                Expr::Wait { handlers, filter }
            }
            Expr::Tuple(elems) => Expr::Tuple(
                elems
                    .into_iter()
                    .map(|e| self.walk_expr(e, scope, module_id))
                    .collect(),
            ),
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(self.walk_expr(*receiver, scope, module_id)),
                index,
            },
            // agent: protocols-146 — `recv[i]` desugar.  `buf[i]` stays the
            // built-in postfix load; a non-buf receiver (record / enum /
            // NamedGeneric / mono'd-record) lowers to `index(recv i)` so
            // the overload machinery dispatches to the type's `index`
            // method (e.g. Vec's `vec_get` alias).  See
            // docs/state/features/146_protos.md.
            Expr::Index { receiver, index } => {
                let receiver = self.walk_expr(*receiver, scope, module_id);
                let index = self.walk_expr(*index, scope, module_id);
                let recv_ty = self.infer_expr_type(&receiver.inner, scope);
                if self.is_indexable_via_proto(&recv_ty) {
                    // Build `index(receiver index)` and re-run callee
                    // rewriting so a generic `index[T]` resolves / monos.
                    let idx_name = self.leak_name("index".to_string());
                    let callee = Expr::Var(idx_name, Type::Unknown).with_span(span);
                    let args = vec![receiver, index];
                    let new_ident =
                        self.maybe_rewrite_callee(callee, &args, scope, module_id, span);
                    Expr::Jump {
                        ident: Box::new(new_ident),
                        args,
                    }
                } else {
                    Expr::Index {
                        receiver: Box::new(receiver),
                        index: Box::new(index),
                    }
                }
            }
            Expr::DotAccess { receiver, field } => Expr::DotAccess {
                receiver: Box::new(self.walk_expr(*receiver, scope, module_id)),
                field,
            },
            Expr::MethodCall { receiver, method, args } => Expr::MethodCall {
                receiver: Box::new(self.walk_expr(*receiver, scope, module_id)),
                method,
                args: args
                    .into_iter()
                    .map(|a| self.walk_expr(a, scope, module_id))
                    .collect(),
            },
            Expr::AtAccess { receiver, field } => Expr::AtAccess {
                receiver: Box::new(self.walk_expr(*receiver, scope, module_id)),
                field,
            },
            Expr::StructInit { base, fields } => Expr::StructInit {
                base: Box::new(self.walk_expr(*base, scope, module_id)),
                fields: fields
                    .into_iter()
                    .map(|f| self.walk_expr(f, scope, module_id))
                    .collect(),
            },
            Expr::LetTuple { fields, default } => Expr::LetTuple {
                fields,
                default: Box::new(self.walk_expr(*default, scope, module_id)),
            },
            Expr::RecordLiteral { name, fields } => Expr::RecordLiteral {
                name,
                fields: fields
                    .into_iter()
                    .map(|(k, v)| (k, self.walk_expr(v, scope, module_id)))
                    .collect(),
            },
            other => other,
        };
        inner.with_span(span)
    }

    /// Inspect a `Jump`'s callee Var.  If it resolves to a generic
    /// record or machine, infer a substitution from `args`, queue (or
    /// reuse) a mono instance, and rewrite the callee ident to the
    /// mangled name.  Non-generic / unknown callees pass through.
    fn maybe_rewrite_callee(
        &mut self,
        ident: Spanned<Expr<'src>>,
        args: &[Spanned<Expr<'src>>],
        scope: &HashMap<&'src str, Type<'src>>,
        _module_id: ModuleId,
        call_span: SimpleSpan,
    ) -> Spanned<Expr<'src>> {
        let span = ident.span;
        if let Expr::Var(name, ty) = &ident.inner {
            // Generic record ctor?
            if let Some(decl) = self.record_templates.get(*name).cloned() {
                let subst = match self.unify_against_record_fields(&decl, args, scope, call_span) {
                    Some(s) => s,
                    None => return ident,
                };
                let concrete_args: Vec<Type<'src>> = decl
                    .type_params
                    .iter()
                    .map(|p| {
                        subst
                            .get(p.inner.0)
                            .cloned()
                            .unwrap_or(Type::Unknown)
                    })
                    .collect();
                let mangled = self.intern_record_mono(name, &concrete_args, call_span);
                return Expr::Var(mangled, ty.clone()).with_span(span);
            }
            // Generic machine call?  A name may map to several
            // same-named generic templates from different modules
            // (e.g. `Option.unwrap_or[T]` and `Result.unwrap_or[T E]`).
            // Overload-resolve by argument types: try each candidate
            // quietly and keep the one whose params bind cleanly.
            if let Some(cands) = self.machine_templates.get(*name).cloned() {
                if !cands.is_empty() {
                    for (owner, decl) in &cands {
                        if let Some(subst) =
                            self.unify_against_machine_quiet(decl, args, scope)
                        {
                            let concrete_args: Vec<Type<'src>> = decl
                                .generics
                                .iter()
                                .map(|p| subst.get(p.inner.0).cloned().unwrap_or(Type::Unknown))
                                .collect();
                            let mangled = self
                                .intern_machine_mono(name, &concrete_args, *owner, call_span);
                            return Expr::Var(mangled, ty.clone()).with_span(span);
                        }
                    }
                    // No candidate unified — emit one diagnostic against
                    // the first (best-effort UX), leave the call as-is.
                    let (_, first) = &cands[0];
                    self.unify_against_machine(first, args, scope, call_span);
                    return ident;
                }
            }
        }
        ident
    }

    /// agent: generics-142 phase 4 — rewrite a `Enum.Variant` /
    /// `Enum.Variant(args)` callee whose head is a known generic enum
    /// template.  Returns the rewritten ident with the receiver Var
    /// swapped for the mono'd enum name.  Returns `None` when the
    /// shape doesn't match an enum constructor.
    ///
    /// Inference precedence:
    /// 1. `enum_hints` set by the enclosing Let annotation;
    /// 2. unifying arg types against the variant's declared payload.
    fn maybe_rewrite_enum_dot_ctor(
        &mut self,
        callee: &Spanned<Expr<'src>>,
        args: &[Spanned<Expr<'src>>],
        scope: &HashMap<&'src str, Type<'src>>,
        call_span: SimpleSpan,
        _has_args: bool,
    ) -> Option<Spanned<Expr<'src>>> {
        let Expr::DotAccess { receiver, field } = &callee.inner else {
            return None;
        };
        let Expr::Var(enum_name, _) = &receiver.inner else {
            return None;
        };
        let decl = self.enum_templates.get(*enum_name).cloned()?;
        // Try the Let-supplied hint first — it carries the full arg
        // list (handles `Option.None` where args is empty).
        let concrete_args: Vec<Type<'src>> = if let Some(stack) =
            self.enum_hints.get(*enum_name)
        {
            stack.last().cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let concrete_args = if !concrete_args.is_empty() {
            concrete_args
        } else {
            // Infer from arg types against the variant's declared
            // payload types.
            let variant_name = field.inner.0;
            let payload_tys: Vec<Type<'src>> = decl
                .variants
                .iter()
                .find(|v| v.inner.name.inner.0 == variant_name)
                .map(|v| match &v.inner.payload {
                    VariantPayload::None => Vec::new(),
                    VariantPayload::Tuple(ts) => {
                        ts.iter().map(|t| t.inner.clone()).collect()
                    }
                    VariantPayload::Record(fs) => {
                        fs.iter().map(|(_, t)| t.inner.clone()).collect()
                    }
                })
                .unwrap_or_default();
            let type_params: HashSet<&'src str> =
                decl.type_params.iter().map(|p| p.inner.0).collect();
            let mut subst: Subst<'src> = HashMap::new();
            let n = payload_tys.len().min(args.len());
            for i in 0..n {
                let actual = self.infer_expr_type(&args[i].inner, scope);
                self.unify(&payload_tys[i], &actual, &type_params, &mut subst);
            }
            // Build the ordered concrete arg list — if a type param
            // couldn't be inferred (e.g. `Result.Ok(7)` leaves E
            // unbound) bail out and let the resolver / typeck flag.
            let mut all_args: Vec<Type<'src>> = Vec::with_capacity(decl.type_params.len());
            let mut bound = true;
            for tp in &decl.type_params {
                match subst.get(tp.inner.0) {
                    Some(t) => all_args.push(t.clone()),
                    None => {
                        bound = false;
                        break;
                    }
                }
            }
            if !bound {
                self.errors.push(
                    LakeError::new(
                        format!(
                            "cannot infer type parameter(s) for enum `{}` — \
                             annotate the binding (e.g. `let x EnumName[T] = ...`)",
                            enum_name,
                        ),
                        call_span,
                    )
                    .code("E040"),
                );
                return None;
            }
            all_args
        };
        let mangled = self.intern_enum_mono(enum_name, &concrete_args, call_span);
        // Rewrite the receiver Var to the mono'd enum name.
        let recv_span = receiver.span;
        let new_receiver =
            Expr::Var(mangled, Type::Unknown).with_span(recv_span);
        let new_callee = Expr::DotAccess {
            receiver: Box::new(new_receiver),
            field: field.clone(),
        };
        Some(new_callee.with_span(callee.span))
    }

    /// Unify a list of inferred arg types against a record's declared
    /// field types (in order), binding type parameters in the returned
    /// substitution.
    fn unify_against_record_fields(
        &mut self,
        decl: &RecordDecl<'src>,
        args: &[Spanned<Expr<'src>>],
        scope: &HashMap<&'src str, Type<'src>>,
        call_span: SimpleSpan,
    ) -> Option<Subst<'src>> {
        if args.len() != decl.fields.len() {
            // Arity mismatch is surfaced by typeck; mono just bails.
            return None;
        }
        let type_params: HashSet<&'src str> =
            decl.type_params.iter().map(|p| p.inner.0).collect();
        let mut subst: Subst<'src> = HashMap::new();
        for (arg, field) in args.iter().zip(decl.fields.iter()) {
            let actual = self.infer_expr_type(&arg.inner, scope);
            self.unify(&field.inner.ty.inner, &actual, &type_params, &mut subst);
        }
        // Surface E040 for any unresolved type parameter.
        for tp in &decl.type_params {
            if !subst.contains_key(tp.inner.0) {
                self.errors.push(
                    LakeError::new(
                        format!(
                            "cannot infer type parameter `{}` for `{}`",
                            tp.inner.0, decl.ident.inner.0,
                        ),
                        call_span,
                    )
                    .code("E040")
                    .help(
                        "supply an argument whose type fixes the parameter, \
                         or annotate the binding explicitly",
                    ),
                );
                return None;
            }
        }
        Some(subst)
    }

    /// Unify against any branch of a generic machine.  We try every
    /// branch in order; the first that arity-matches and unifies
    /// without contradiction wins.  Machines typically have one
    /// ret-branch; multi-branch generic machines aren't on the v1 menu
    /// but the loop costs nothing.
    fn unify_against_machine(
        &mut self,
        decl: &Machine<'src>,
        args: &[Spanned<Expr<'src>>],
        scope: &HashMap<&'src str, Type<'src>>,
        call_span: SimpleSpan,
    ) -> Option<Subst<'src>> {
        if let Some(s) = self.unify_against_machine_quiet(decl, args, scope) {
            return Some(s);
        }
        if let Some(tp) = decl.generics.first() {
            self.errors.push(
                LakeError::new(
                    format!(
                        "cannot infer type parameter `{}` for `{}`",
                        tp.inner.0, decl.ident.inner.0,
                    ),
                    call_span,
                )
                .code("E040")
                .help(
                    "supply arguments whose types fix the parameter(s), or \
                     annotate the binding explicitly",
                ),
            );
        }
        None
    }

    /// Argument-driven unification without emitting diagnostics.  Used
    /// by overload resolution: when a name has several same-named
    /// generic templates (e.g. `Option.unwrap_or[T]` /
    /// `Result.unwrap_or[T E]`) the caller tries each candidate and
    /// keeps the one whose params bind cleanly, so a failed attempt on
    /// the wrong candidate must stay silent.
    fn unify_against_machine_quiet(
        &mut self,
        decl: &Machine<'src>,
        args: &[Spanned<Expr<'src>>],
        scope: &HashMap<&'src str, Type<'src>>,
    ) -> Option<Subst<'src>> {
        let type_params: HashSet<&'src str> =
            decl.generics.iter().map(|p| p.inner.0).collect();
        for item in &decl.items {
            let MachineItem::Branch(b) = &item.inner else {
                continue;
            };
            // Build the param list from non-wildcard, non-literal patterns.
            let params: Vec<&Pattern<'src>> = b
                .patterns
                .iter()
                .filter(|p| !p.inner.is_wildcard() && !p.inner.is_literal_guard())
                .map(|p| &p.inner)
                .collect();
            if params.len() != args.len() {
                continue;
            }
            let mut subst: Subst<'src> = HashMap::new();
            let mut ok = true;
            for (pat, arg) in params.iter().zip(args.iter()) {
                let actual = self.infer_expr_type(&arg.inner, scope);
                if !self.unify(&pat.ty.inner, &actual, &type_params, &mut subst) {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            // #88 — bind any leftover params from the return hint
            // (`let x T = f()`): unify the branch's declared return
            // type against the enclosing annotation.  Needed for
            // zero-arg constructors like `vec_new[T]` whose params
            // can't pin T.
            if let (Some(hint), Some(ret_ty)) = (self.ret_hint.clone(), b.ret_ty.as_ref()) {
                self.unify(&ret_ty.inner, &hint, &type_params, &mut subst);
            }
            // Validate every declared type param ended up bound.
            let mut all_bound = true;
            for tp in &decl.generics {
                if !subst.contains_key(tp.inner.0) {
                    all_bound = false;
                    break;
                }
            }
            if all_bound {
                return Some(subst);
            }
        }
        None
    }

    /// Core unification.  Records bindings in `subst` and returns false
    /// only when a previously bound type-param is asked to take a
    /// different concrete type than it already holds.
    fn unify(
        &self,
        declared: &Type<'src>,
        actual: &Type<'src>,
        type_params: &HashSet<&'src str>,
        subst: &mut Subst<'src>,
    ) -> bool {
        // Treat `Type::Named("T")` as a type var when "T" is in
        // type_params — the resolver-rewrite to `TypeVar` hasn't yet
        // happened at mono time.  `Type::TypeVar` is also accepted in
        // case mono ever runs post-resolve.
        match (declared, actual) {
            (Type::Named(d), _) if type_params.contains(d.inner.0) => {
                bind_or_check(d.inner.0, actual.clone(), subst)
            }
            (Type::TypeVar(d), _) if type_params.contains(d.inner.0) => {
                bind_or_check(d.inner.0, actual.clone(), subst)
            }
            // Concrete-vs-concrete: must structurally match.  We accept
            // some looseness (Unknown actual matches anything) because
            // mono runs early and infer can punt.
            (_, Type::Unknown) => true,
            (
                Type::NamedGeneric { name: n1, args: a1 },
                Type::NamedGeneric { name: n2, args: a2 },
            ) => {
                if n1.inner.0 != n2.inner.0 || a1.len() != a2.len() {
                    return false;
                }
                a1.iter()
                    .zip(a2.iter())
                    .all(|(x, y)| self.unify(&x.inner, &y.inner, type_params, subst))
            }
            (Type::Named(d), Type::Named(a)) => d.inner.0 == a.inner.0,
            (Type::Struct(d), Type::Struct(a)) => {
                if d.len() != a.len() {
                    return false;
                }
                d.iter()
                    .zip(a.iter())
                    .all(|(x, y)| self.unify(&x.inner, &y.inner, type_params, subst))
            }
            (Type::Unit, Type::Unit) => true,
            // Allow declared NamedGeneric to unify against an already-
            // mono'd Named.  Happens when a generic machine takes
            // `Box[T]` and the call passes a `Box_i64`.  We can recover
            // T by consulting the mono-record cache.
            (Type::NamedGeneric { name: n1, args: a1 }, Type::Named(a_id)) => {
                let mangled = a_id.inner.0;
                // Look up the cache for the originating (base, args)
                // entry whose mangled name matches the actual.
                for ((base, arg_strs), m) in &self.mono_records {
                    if m == mangled && base == n1.inner.0 && arg_strs.len() == a1.len() {
                        // Reconstruct concrete types from the cache key
                        // and unify per slot.
                        let concrete_args: Vec<Type<'src>> = arg_strs
                            .iter()
                            .map(|s| type_from_string(s))
                            .collect();
                        return a1.iter().zip(concrete_args.iter()).all(|(decl, conc)| {
                            self.unify(&decl.inner, conc, type_params, subst)
                        });
                    }
                }
                // agent: generics-142 phase 4 — same lookup for enums.
                for ((base, arg_strs), m) in &self.mono_enums {
                    if m == mangled && base == n1.inner.0 && arg_strs.len() == a1.len() {
                        let concrete_args: Vec<Type<'src>> = arg_strs
                            .iter()
                            .map(|s| type_from_string(s))
                            .collect();
                        return a1.iter().zip(concrete_args.iter()).all(|(decl, conc)| {
                            self.unify(&decl.inner, conc, type_params, subst)
                        });
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Infer an expression's static type for the purposes of mono.
    /// Mirrors a subset of [`crate::resolver::Resolver::infer_expr_type`]
    /// but works pre-resolver — types live in the AST already for
    /// literals; everything else falls back to `Type::Unknown`.
    fn infer_expr_type(
        &self,
        expr: &Expr<'src>,
        scope: &HashMap<&'src str, Type<'src>>,
    ) -> Type<'src> {
        match expr {
            Expr::Num(_, ty) => {
                if matches!(ty, Type::Unknown) {
                    static_named("i64")
                } else {
                    ty.clone()
                }
            }
            Expr::String(_, ty) => {
                if matches!(ty, Type::Unknown) {
                    static_named("str")
                } else {
                    ty.clone()
                }
            }
            Expr::Bool(_) => static_named("bool"),
            Expr::Atom(_) => static_named("atom"),
            Expr::Unit => Type::Unit,
            Expr::Var(name, ty) => {
                if !matches!(ty, Type::Unknown) {
                    return ty.clone();
                }
                if let Some(t) = scope.get(name) {
                    t.clone()
                } else {
                    Type::Unknown
                }
            }
            Expr::Add(l, _) | Expr::Sub(l, _) | Expr::Mul(l, _) | Expr::Div(l, _) => {
                self.infer_expr_type(&l.inner, scope)
            }
            Expr::Neg(i) => self.infer_expr_type(&i.inner, scope),
            Expr::Eq(_, _)
            | Expr::Le(_, _)
            | Expr::Ge(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _)
            | Expr::BAnd(_, _)
            | Expr::BOr(_, _)
            | Expr::BXor(_, _)
            | Expr::Shl(_, _)
            | Expr::Shr(_, _) => static_named("i64"),
            Expr::Index { .. } => static_named("i64"),
            Expr::Tuple(elems) => Type::Struct(
                elems
                    .iter()
                    .map(|e| {
                        let sp = e.span;
                        self.infer_expr_type(&e.inner, scope).with_span(sp)
                    })
                    .collect(),
            ),
            Expr::Jump { ident, args } => {
                // agent: generics-142 phase 4 — variant constructor on
                // a (possibly mono'd) enum.  Returns `Type::Named(<enum>)`.
                if let Expr::DotAccess { receiver, .. } = &ident.inner {
                    if let Expr::Var(enum_name, _) = &receiver.inner {
                        // Post-rewrite: receiver is already the mono'd
                        // enum name; lookup hits mono_enums.
                        for (_, mangled) in &self.mono_enums {
                            if mangled == enum_name {
                                let s: Box<str> = mangled.clone().into_boxed_str();
                                let leaked: &'static str = Box::leak(s);
                                return src_named(leaked);
                            }
                        }
                        // Non-generic enum (or generic template — caller
                        // is responsible for hint context); fall back to
                        // a plain Named tag.
                        if self.registry.lookup_enum_in(ModuleId::ROOT, enum_name).is_some() {
                            let s: Box<str> = (*enum_name).to_string().into_boxed_str();
                            let leaked: &'static str = Box::leak(s);
                            return src_named(leaked);
                        }
                    }
                }
                if let Expr::Var(name, _) = &ident.inner {
                    // Generic record before rewrite (in tree we walk
                    // top-down so args come first).
                    if let Some(decl) = self.record_templates.get(*name) {
                        // Infer the substitution speculatively to
                        // produce a NamedGeneric tag for this call.
                        let type_params: HashSet<&'src str> =
                            decl.type_params.iter().map(|p| p.inner.0).collect();
                        let mut subst: Subst<'src> = HashMap::new();
                        if args.len() == decl.fields.len() {
                            for (arg, field) in args.iter().zip(decl.fields.iter()) {
                                let actual = self.infer_expr_type(&arg.inner, scope);
                                self.unify(&field.inner.ty.inner, &actual, &type_params, &mut subst);
                            }
                            let concrete_args: Vec<Spanned<Type<'src>>> = decl
                                .type_params
                                .iter()
                                .map(|p| {
                                    let sp = p.span;
                                    subst
                                        .get(p.inner.0)
                                        .cloned()
                                        .unwrap_or(Type::Unknown)
                                        .with_span(sp)
                                })
                                .collect();
                            let name_ident = decl.ident.clone();
                            return Type::NamedGeneric {
                                name: name_ident,
                                args: concrete_args,
                            };
                        }
                    }
                    // Already-mangled mono'd record — look it up.
                    // The mono cache stores the canonical (leaked) name;
                    // recover the leaked &'src str for the leaked entry
                    // matching this callee.
                    for ((_base, _arg_strs), mangled) in &self.mono_records {
                        if mangled == name {
                            // Re-leak so the returned Type has &'src
                            // lifetime tied to the program's source
                            // strings (records emitted here outlive 'a).
                            let s: Box<str> = mangled.clone().into_boxed_str();
                            let leaked: &'static str = Box::leak(s);
                            return src_named(leaked);
                        }
                    }
                    if let Some(Resolution::Machine(m)) = self
                        .registry
                        .resolve_bare_in(ModuleId::ROOT, name)
                    {
                        if m.branches.iter().all(|b| b.ret == "pid") {
                            return static_named("pid");
                        }
                    }
                    // Mono'd machine case: if name matches one we've
                    // emitted, look up the original generic for the
                    // ret type (TypeVar substituted by stored args).
                    // Snapshot the cache entries before calling
                    // `substitute_type_pure` so we don't borrow self
                    // mutably and immutably at once.
                    let snapshot: Vec<(String, Vec<String>, String)> = self
                        .mono_machines
                        .iter()
                        .map(|((b, a), m)| (b.clone(), a.clone(), m.clone()))
                        .collect();
                    for (base, arg_strs, mangled) in snapshot {
                        if mangled == *name {
                            // Pick the same-named template whose generic
                            // arity matches this instance's arg count —
                            // distinguishes e.g. unwrap_or[T] from
                            // unwrap_or[T E].
                            if let Some(decl) = self
                                .machine_templates
                                .get(&base)
                                .and_then(|v| {
                                    v.iter()
                                        .find(|(_, d)| d.generics.len() == arg_strs.len())
                                        .or_else(|| v.first())
                                        .map(|(_, d)| d.clone())
                                })
                            {
                                let subst: Subst<'src> = decl
                                    .generics
                                    .iter()
                                    .zip(arg_strs.iter())
                                    .map(|(p, s)| (p.inner.0, type_from_string(s)))
                                    .collect();
                                for it in &decl.items {
                                    if let MachineItem::Branch(b) = &it.inner {
                                        if let Some(ret) = &b.ret_ty {
                                            return substitute_type_pure(
                                                &ret.inner,
                                                &subst,
                                            );
                                        }
                                    }
                                }
                                return static_named("pid");
                            }
                        }
                    }
                }
                Type::Unknown
            }
            // agent: generics-142 phase 4 — nullary variant ctor:
            // `Option_i64.None` evaluated as a value.  Same lookup as
            // the Jump path so the enclosing Let infers the right
            // `Type::Named(<enum>)` for its binding.
            Expr::DotAccess { receiver, .. } => {
                if let Expr::Var(enum_name, _) = &receiver.inner {
                    for (_, mangled) in &self.mono_enums {
                        if mangled == enum_name {
                            let s: Box<str> = mangled.clone().into_boxed_str();
                            let leaked: &'static str = Box::leak(s);
                            return src_named(leaked);
                        }
                    }
                    if self.registry.lookup_enum_in(ModuleId::ROOT, enum_name).is_some() {
                        let s: Box<str> = (*enum_name).to_string().into_boxed_str();
                        let leaked: &'static str = Box::leak(s);
                        return src_named(leaked);
                    }
                }
                Type::Unknown
            }
            _ => Type::Unknown,
        }
    }

    /// Intern a record mono.  Returns the mangled `&'src str` name.
    fn intern_record_mono(
        &mut self,
        base: &str,
        args: &[Type<'src>],
        _call_span: SimpleSpan,
    ) -> &'src str {
        let arg_strs: Vec<String> = args.iter().map(mangle_type).collect();
        let key = (base.to_string(), arg_strs.clone());
        if let Some(m) = self.mono_records.get(&key) {
            // Re-leak to recover the &'src str — we stored the canonical
            // leaked copy at insert time.  Use the existing entry.
            // SAFETY: only one leak per name; subsequent lookups go
            // through the same slot.
            let s = m.clone();
            return Box::leak(s.into_boxed_str());
        }
        let mangled_owned = format!("{}_{}", base, arg_strs.join("_"));
        let mangled_leaked = self.leak_name(mangled_owned.clone());
        self.mono_records.insert(key, mangled_owned);
        self.work.push(MonoJob::Record {
            base: base.to_string(),
            args: args.to_vec(),
            mangled: mangled_leaked,
        });
        mangled_leaked
    }

    fn intern_machine_mono(
        &mut self,
        base: &str,
        args: &[Type<'src>],
        owner: ModuleId,
        _call_span: SimpleSpan,
    ) -> &'src str {
        let arg_strs: Vec<String> = args.iter().map(mangle_type).collect();
        let key = (base.to_string(), arg_strs.clone());
        if let Some(m) = self.mono_machines.get(&key) {
            let s = m.clone();
            return Box::leak(s.into_boxed_str());
        }
        // agent: protocols-146 — verify `[T: Eq]` bounds against the
        // concrete substituted types before emitting the instance.  Runs
        // once per unique (base, args) pair (after the cache check).
        self.verify_bounds(base, args, owner, _call_span);
        let mangled_owned = format!("{}_{}", base, arg_strs.join("_"));
        let mangled_leaked = self.leak_name(mangled_owned.clone());
        self.mono_machines.insert(key, mangled_owned);
        self.work.push(MonoJob::Machine {
            base: base.to_string(),
            args: args.to_vec(),
            mangled: mangled_leaked,
            module: owner,
        });
        mangled_leaked
    }

    /// agent: protocols-146 — does `recv[i]` on this receiver type lower to
    /// an `index(recv i)` call rather than the built-in `buf[i]` load?
    ///
    /// True for record / enum / NamedGeneric receivers and mono'd-record
    /// named types.  False for `buf` (the built-in stays), for primitives,
    /// and for `Unknown` (no info → leave as built-in; typeck/backend
    /// surface a real error if it's genuinely wrong).
    fn is_indexable_via_proto(&self, ty: &Type<'src>) -> bool {
        match ty {
            // Still-generic use site — index always goes through a method.
            Type::NamedGeneric { .. } => true,
            Type::Named(name) => {
                let n = name.inner.0;
                if n == "buf" {
                    return false;
                }
                // A name resolving to a record / enum (incl. mono'd copies
                // emitted this pass) indexes via a method.
                if self.registry.resolve_bare_in(ModuleId::ROOT, n).map_or(false, |r| {
                    matches!(r, Resolution::Record(_))
                }) {
                    return true;
                }
                if self.registry.lookup_enum_in(ModuleId::ROOT, n).is_some() {
                    return true;
                }
                // Mono'd record / enum names emitted in this pass aren't in
                // the (pre-mono) registry yet — recognise them by cache.
                self.mono_records.values().any(|m| m == n)
                    || self.mono_enums.values().any(|m| m == n)
            }
            _ => false,
        }
    }

    /// agent: protocols-146 — verify the proto bounds on a generic machine
    /// against the concrete type arguments being substituted.
    ///
    /// For each `[Tᵢ: Proto …]` bound, resolve `Tᵢ`'s concrete type, then
    /// for every method the proto requires (including inherited `+Parent`
    /// requirements) check a machine of that name with the `Self := Tᵢ`
    /// substituted signature exists.  Missing → E042.
    fn verify_bounds(
        &mut self,
        base: &str,
        args: &[Type<'src>],
        owner: ModuleId,
        call_span: SimpleSpan,
    ) {
        // Pick the matching template (by module when ambiguous).
        let Some(cands) = self.machine_templates.get(base) else {
            return;
        };
        let template = if cands.len() == 1 {
            cands[0].1.clone()
        } else if let Some((_, d)) = cands.iter().find(|(m, _)| *m == owner) {
            d.clone()
        } else {
            return;
        };
        if template.bounds.is_empty() {
            return;
        }
        // Map generic name → concrete substituted type string (positional).
        let mut concrete: HashMap<&'src str, String> = HashMap::new();
        for (tp, arg) in template.generics.iter().zip(args.iter()) {
            concrete.insert(tp.inner.0, mangle_type(arg));
        }
        for (param, protos) in &template.bounds {
            let Some(cty) = concrete.get(param.inner.0) else {
                continue;
            };
            for proto in protos {
                self.check_proto_satisfied(proto.inner.0, cty, call_span);
            }
        }
    }

    /// Check a single `proto` is satisfied by concrete type string `cty`.
    /// Walks the proto's own methods plus all transitively-inherited
    /// parent requirements; emits E042 per missing method.
    fn check_proto_satisfied(&mut self, proto: &str, cty: &str, call_span: SimpleSpan) {
        // Gather this proto + all ancestors (cycle-guarded).
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = vec![proto.to_string()];
        // (proto_name, method_name, substituted_params, substituted_ret)
        let mut reqs: Vec<(String, String, Vec<String>, String)> = Vec::new();
        while let Some(pname) = stack.pop() {
            if !seen.insert(pname.clone()) {
                continue;
            }
            let Some(entry) = self.registry.lookup_proto(&pname) else {
                // Unknown proto name in a bound — surface once.
                self.errors.push(
                    LakeError::new(
                        format!("unknown proto `{pname}` in bound"),
                        call_span,
                    )
                    .code("E042"),
                );
                continue;
            };
            for parent in &entry.parents {
                stack.push(parent.to_string());
            }
            for (mname, params, ret) in &entry.required {
                let sub_params: Vec<String> = params
                    .iter()
                    .map(|p| if p == "Self" { cty.to_string() } else { p.clone() })
                    .collect();
                let sub_ret = if ret == "Self" { cty.to_string() } else { ret.clone() };
                reqs.push((pname.clone(), (*mname).to_string(), sub_params, sub_ret));
            }
        }
        for (pname, mname, params, ret) in reqs {
            if !self.registry.has_method_with_sig(&mname, &params, &ret) {
                let sig = format!("{}({}) -> {}", mname, params.join(" "), ret);
                self.errors.push(
                    LakeError::new(
                        format!(
                            "type `{cty}` does not implement proto `{pname}`: \
                             missing `{sig}`"
                        ),
                        call_span,
                    )
                    .code("E042")
                    .help(format!(
                        "define a machine `{mname}` with that signature for `{cty}`"
                    )),
                );
            }
        }
    }

    /// Intern an enum mono.  Returns the mangled `&'src str` name.
    fn intern_enum_mono(
        &mut self,
        base: &str,
        args: &[Type<'src>],
        _call_span: SimpleSpan,
    ) -> &'src str {
        let arg_strs: Vec<String> = args.iter().map(mangle_type).collect();
        let key = (base.to_string(), arg_strs.clone());
        if let Some(m) = self.mono_enums.get(&key) {
            let s = m.clone();
            return Box::leak(s.into_boxed_str());
        }
        let mangled_owned = format!("{}_{}", base, arg_strs.join("_"));
        let mangled_leaked = self.leak_name(mangled_owned.clone());
        self.mono_enums.insert(key, mangled_owned);
        self.work.push(MonoJob::Enum {
            base: base.to_string(),
            args: args.to_vec(),
            mangled: mangled_leaked,
        });
        mangled_leaked
    }

    /// Materialize a concrete enum from a template + substitution.
    /// Variant payload types are walked through [`substitute_type`] so
    /// nested generic references (e.g. `Some(Box[T])`) flatten into
    /// their mono'd shape and queue further jobs as needed.
    fn instantiate_enum(
        &mut self,
        base: &str,
        args: &[Type<'src>],
        mangled: &'src str,
    ) -> EnumDecl<'src> {
        let template = self
            .enum_templates
            .get(base)
            .cloned()
            .expect("enum template");
        let subst: Subst<'src> = template
            .type_params
            .iter()
            .zip(args.iter())
            .map(|(p, t)| (p.inner.0, t.clone()))
            .collect();
        let variants: Vec<Spanned<Variant<'src>>> = template
            .variants
            .into_iter()
            .map(|v| {
                let span = v.span;
                let Variant { name, payload } = v.inner;
                let new_payload = match payload {
                    VariantPayload::None => VariantPayload::None,
                    VariantPayload::Tuple(ts) => VariantPayload::Tuple(
                        ts.into_iter()
                            .map(|t| {
                                let sp = t.span;
                                self.substitute_type(&t.inner, &subst).with_span(sp)
                            })
                            .collect(),
                    ),
                    VariantPayload::Record(fs) => VariantPayload::Record(
                        fs.into_iter()
                            .map(|(n, t)| {
                                let sp = t.span;
                                let new_ty =
                                    self.substitute_type(&t.inner, &subst).with_span(sp);
                                (n, new_ty)
                            })
                            .collect(),
                    ),
                };
                Variant {
                    name,
                    payload: new_payload,
                }
                .with_span(span)
            })
            .collect();
        let name_span = template.name.span;
        EnumDecl {
            name: Ident::new(mangled).with_span(name_span),
            type_params: Vec::new(),
            variants,
        }
    }

    /// Materialize a concrete record from a template + substitution.
    fn instantiate_record(
        &mut self,
        base: &str,
        args: &[Type<'src>],
        mangled: &'src str,
    ) -> RecordDecl<'src> {
        let template = self
            .record_templates
            .get(base)
            .cloned()
            .expect("record template");
        let subst: Subst<'src> = template
            .type_params
            .iter()
            .zip(args.iter())
            .map(|(p, t)| (p.inner.0, t.clone()))
            .collect();
        let fields: Vec<Spanned<RecordField<'src>>> = template
            .fields
            .into_iter()
            .map(|f| {
                let span = f.span;
                let RecordField { name, ty } = f.inner;
                let new_ty = self.substitute_type(&ty.inner, &subst).with_span(ty.span);
                RecordField { name, ty: new_ty }.with_span(span)
            })
            .collect();
        let ident_span = template.ident.span;
        RecordDecl {
            vis: template.vis,
            ident: Ident::new(mangled).with_span(ident_span),
            type_params: Vec::new(),
            fields,
        }
    }

    /// Materialize a concrete machine from a template + substitution.
    fn instantiate_machine(
        &mut self,
        base: &str,
        args: &[Type<'src>],
        mangled: &'src str,
        module_id: ModuleId,
    ) -> Machine<'src> {
        let template = self
            .machine_templates
            .get(base)
            .and_then(|v| {
                if v.len() == 1 {
                    Some(v[0].1.clone())
                } else {
                    v.iter()
                        .find(|(m, _)| *m == module_id)
                        .map(|(_, d)| d.clone())
                }
            })
            .expect("machine template");
        let subst: Subst<'src> = template
            .generics
            .iter()
            .zip(args.iter())
            .map(|(p, t)| (p.inner.0, t.clone()))
            .collect();
        let ident_span = template.ident.span;
        let new_items: Vec<Spanned<MachineItem<'src>>> = template
            .items
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    MachineItem::Branch(b) => {
                        MachineItem::Branch(self.substitute_branch(b, &subst, module_id))
                            .with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect();
        Machine {
            attrs: template.attrs,
            vis: template.vis,
            ident: Ident::new(mangled).with_span(ident_span),
            generics: Vec::new(),
            bounds: Vec::new(),
            items: new_items,
        }
    }

    /// Substitute type-vars + walk-rewrite call sites inside a cloned
    /// branch.  After substitution the branch is also driven through
    /// `walk_branch` so it can spawn further mono jobs.
    fn substitute_branch(
        &mut self,
        branch: Branch<'src>,
        subst: &Subst<'src>,
        module_id: ModuleId,
    ) -> Branch<'src> {
        let Branch { label, patterns, ret_ty, body } = branch;
        let patterns: Vec<Spanned<Pattern<'src>>> = patterns
            .into_iter()
            .map(|p| {
                let span = p.span;
                let Pattern { ident, ty, kind } = p.inner;
                let new_ty =
                    self.substitute_type(&ty.inner, subst).with_span(ty.span);
                let new_kind = match kind {
                    PatternKind::Tuple(inner) => PatternKind::Tuple(
                        inner
                            .into_iter()
                            .map(|sp| {
                                let isp = sp.span;
                                let Pattern { ident: id2, ty: ty2, kind: k2 } = sp.inner;
                                let new_ty2 = self
                                    .substitute_type(&ty2.inner, subst)
                                    .with_span(ty2.span);
                                Pattern {
                                    ident: id2,
                                    ty: new_ty2,
                                    kind: k2,
                                }
                                .with_span(isp)
                            })
                            .collect(),
                    ),
                    other => other,
                };
                Pattern {
                    ident,
                    ty: new_ty,
                    kind: new_kind,
                }
                .with_span(span)
            })
            .collect();
        let ret_ty = ret_ty.map(|t| {
            let sp = t.span;
            self.substitute_type(&t.inner, subst).with_span(sp)
        });
        let body: Vec<Spanned<Expr<'src>>> = body
            .into_iter()
            .map(|e| self.substitute_expr(e, subst))
            .collect();
        // Re-walk the substituted body so freshly-revealed call sites
        // (Jump callees that point at other generics) get rewritten
        // and queued.
        let new_branch = Branch::new(label, patterns, ret_ty, body);
        self.walk_branch(new_branch, module_id, &[])
    }

    /// Apply a substitution to one type.  `NamedGeneric` whose head
    /// matches a registered generic record is collapsed to a
    /// monomorphised `Type::Named(<mangled>)`.
    fn substitute_type(&mut self, ty: &Type<'src>, subst: &Subst<'src>) -> Type<'src> {
        match ty {
            Type::TypeVar(id) => subst
                .get(id.inner.0)
                .cloned()
                .unwrap_or_else(|| ty.clone()),
            Type::Named(id) => subst
                .get(id.inner.0)
                .cloned()
                .unwrap_or_else(|| ty.clone()),
            Type::NamedGeneric { name, args } => {
                let new_args: Vec<Spanned<Type<'src>>> = args
                    .iter()
                    .map(|a| {
                        let sp = a.span;
                        self.substitute_type(&a.inner, subst).with_span(sp)
                    })
                    .collect();
                let head = name.inner.0;
                // If `head` is a generic record we know about, fold to
                // a concrete Named.  Queue the mono job if new.
                if self.record_templates.contains_key(head) {
                    let arg_tys: Vec<Type<'src>> =
                        new_args.iter().map(|a| a.inner.clone()).collect();
                    let mangled =
                        self.intern_record_mono(head, &arg_tys, name.span);
                    let id = Ident::new(mangled).with_span(name.span);
                    return Type::Named(id);
                }
                // agent: generics-142 phase 4 — same fold for enums.
                if self.enum_templates.contains_key(head) {
                    let arg_tys: Vec<Type<'src>> =
                        new_args.iter().map(|a| a.inner.clone()).collect();
                    let mangled =
                        self.intern_enum_mono(head, &arg_tys, name.span);
                    let id = Ident::new(mangled).with_span(name.span);
                    return Type::Named(id);
                }
                // Unknown generic head — preserve shape so a later
                // pass can still see it (and typeck can complain).
                Type::NamedGeneric {
                    name: name.clone(),
                    args: new_args,
                }
            }
            Type::Struct(fields) => Type::Struct(
                fields
                    .iter()
                    .map(|f| {
                        let sp = f.span;
                        self.substitute_type(&f.inner, subst).with_span(sp)
                    })
                    .collect(),
            ),
            Type::Path(seg, rest) => {
                let sp = rest.span;
                let new_rest = self.substitute_type(&rest.inner, subst);
                Type::Path(seg.clone(), Box::new(new_rest).with_span(sp))
            }
            Type::Generic(name, args) => {
                let new_args: Vec<Spanned<Type<'src>>> = args
                    .iter()
                    .map(|a| {
                        let sp = a.span;
                        self.substitute_type(&a.inner, subst).with_span(sp)
                    })
                    .collect();
                Type::Generic(name.clone(), new_args)
            }
            Type::Unit | Type::Unknown => ty.clone(),
        }
    }

    /// Walk an expression tree and rewrite any embedded `Type` slots
    /// (Var.ty, Num.ty, Let.ty, …) via [`Self::substitute_type`].  No
    /// call-site rewriting happens here — that's `walk_*`'s job, fired
    /// again on the substituted branch.
    fn substitute_expr(
        &mut self,
        expr: Spanned<Expr<'src>>,
        subst: &Subst<'src>,
    ) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Var(name, ty) => Expr::Var(name, self.substitute_type(&ty, subst)),
            Expr::Num(s, ty) => Expr::Num(s, self.substitute_type(&ty, subst)),
            Expr::String(s, ty) => Expr::String(s, self.substitute_type(&ty, subst)),
            Expr::Let { ident, ty, default } => Expr::Let {
                ident,
                ty: {
                    let sp = ty.span;
                    self.substitute_type(&ty.inner, subst).with_span(sp)
                },
                default: default.map(|d| Box::new(self.substitute_expr(*d, subst))),
            },
            Expr::Jump { ident, args } => {
                // #88 — fold a generic-record constructor inside a
                // template body using the active substitution.  Records
                // like `Vec[T] { data buf, len i64, cap i64 }` have a
                // PHANTOM `T` (storage is a raw buf), so the ctor args
                // can't pin it — but when we instantiate the enclosing
                // generic (e.g. `vec_new_i64`, subst {T:i64}) the ctor's
                // type IS known.  Rewrite `Vec(...)` → `Vec_i64(...)`
                // before the re-walk so it isn't re-inferred from args.
                let folded_ident = if let Expr::Var(rname, vty) = &ident.inner {
                    if let Some(decl) = self.record_templates.get(*rname).cloned() {
                        let concrete: Vec<Type<'src>> = decl
                            .type_params
                            .iter()
                            .map(|p| subst.get(p.inner.0).cloned())
                            .collect::<Option<Vec<_>>>()
                            .unwrap_or_default();
                        if !decl.type_params.is_empty()
                            && concrete.len() == decl.type_params.len()
                        {
                            let mangled =
                                self.intern_record_mono(rname, &concrete, ident.span);
                            Some(
                                Expr::Var(mangled, vty.clone()).with_span(ident.span),
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                let new_ident = match folded_ident {
                    Some(i) => i,
                    None => self.substitute_expr(*ident, subst),
                };
                Expr::Jump {
                    ident: Box::new(new_ident),
                    args: args
                        .into_iter()
                        .map(|a| self.substitute_expr(a, subst))
                        .collect(),
                }
            }
            Expr::Add(l, r) => Expr::Add(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Neg(i) => Expr::Neg(Box::new(self.substitute_expr(*i, subst))),
            Expr::Eq(l, r) => Expr::Eq(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Le(l, r) => Expr::Le(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Ge(l, r) => Expr::Ge(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Lt(l, r) => Expr::Lt(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Gt(l, r) => Expr::Gt(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::BAnd(l, r) => Expr::BAnd(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::BOr(l, r) => Expr::BOr(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::BXor(l, r) => Expr::BXor(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Shl(l, r) => Expr::Shl(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Shr(l, r) => Expr::Shr(
                Box::new(self.substitute_expr(*l, subst)),
                Box::new(self.substitute_expr(*r, subst)),
            ),
            Expr::Ret(i) => Expr::Ret(Box::new(self.substitute_expr(*i, subst))),
            Expr::Pin(i) => Expr::Pin(Box::new(self.substitute_expr(*i, subst))),
            Expr::When { cond, branches } => Expr::When {
                cond: Box::new(self.substitute_expr(*cond, subst)),
                branches: branches
                    .into_iter()
                    .map(|(p, b)| {
                        (
                            self.substitute_expr(p, subst),
                            b.into_iter()
                                .map(|e| self.substitute_expr(e, subst))
                                .collect(),
                        )
                    })
                    .collect(),
            },
            Expr::Wait { handlers, filter } => Expr::Wait {
                filter: filter
                    .into_iter()
                    .map(|f| self.substitute_expr(f, subst))
                    .collect(),
                handlers: handlers
                    .into_iter()
                    .map(|h| {
                        let h_span = h.span;
                        let Branch { label, patterns, ret_ty, body } = h.inner;
                        let patterns = patterns
                            .into_iter()
                            .map(|p| {
                                let sp = p.span;
                                let mut pat = p.inner;
                                pat.ty = self
                                    .substitute_type(&pat.ty.inner, subst)
                                    .with_span(pat.ty.span);
                                pat.with_span(sp)
                            })
                            .collect();
                        let ret_ty = ret_ty.map(|t| {
                            let sp = t.span;
                            self.substitute_type(&t.inner, subst).with_span(sp)
                        });
                        let body = body
                            .into_iter()
                            .map(|e| self.substitute_expr(e, subst))
                            .collect();
                        Branch::new(label, patterns, ret_ty, body).with_span(h_span)
                    })
                    .collect(),
            },
            Expr::Tuple(elems) => Expr::Tuple(
                elems
                    .into_iter()
                    .map(|e| self.substitute_expr(e, subst))
                    .collect(),
            ),
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(self.substitute_expr(*receiver, subst)),
                index,
            },
            Expr::Index { receiver, index } => Expr::Index {
                receiver: Box::new(self.substitute_expr(*receiver, subst)),
                index: Box::new(self.substitute_expr(*index, subst)),
            },
            Expr::DotAccess { receiver, field } => Expr::DotAccess {
                receiver: Box::new(self.substitute_expr(*receiver, subst)),
                field,
            },
            Expr::MethodCall { receiver, method, args } => Expr::MethodCall {
                receiver: Box::new(self.substitute_expr(*receiver, subst)),
                method,
                args: args
                    .into_iter()
                    .map(|a| self.substitute_expr(a, subst))
                    .collect(),
            },
            Expr::AtAccess { receiver, field } => Expr::AtAccess {
                receiver: Box::new(self.substitute_expr(*receiver, subst)),
                field,
            },
            Expr::StructInit { base, fields } => Expr::StructInit {
                base: Box::new(self.substitute_expr(*base, subst)),
                fields: fields
                    .into_iter()
                    .map(|f| self.substitute_expr(f, subst))
                    .collect(),
            },
            Expr::LetTuple { fields, default } => Expr::LetTuple {
                fields,
                default: Box::new(self.substitute_expr(*default, subst)),
            },
            Expr::RecordLiteral { name, fields } => Expr::RecordLiteral {
                name,
                fields: fields
                    .into_iter()
                    .map(|(k, v)| (k, self.substitute_expr(v, subst)))
                    .collect(),
            },
            other => other,
        };
        inner.with_span(span)
    }
}

/// Bind `name` in `subst` to `ty`, or return false if it conflicts with
/// a previously bound concrete type.
fn bind_or_check<'src>(name: &'src str, ty: Type<'src>, subst: &mut Subst<'src>) -> bool {
    use std::collections::hash_map::Entry;
    if matches!(ty, Type::Unknown) {
        return true;
    }
    match subst.entry(name) {
        Entry::Vacant(v) => {
            v.insert(ty);
            true
        }
        Entry::Occupied(o) => types_eq(o.get(), &ty),
    }
}

/// Strict structural equality of two concrete types.  Used by
/// [`bind_or_check`] to detect contradictory bindings.
fn types_eq(a: &Type<'_>, b: &Type<'_>) -> bool {
    match (a, b) {
        (Type::Named(x), Type::Named(y)) => x.inner.0 == y.inner.0,
        (Type::TypeVar(x), Type::TypeVar(y)) => x.inner.0 == y.inner.0,
        (Type::Unit, Type::Unit) => true,
        (Type::Unknown, Type::Unknown) => true,
        (Type::Struct(xs), Type::Struct(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(p, q)| types_eq(&p.inner, &q.inner))
        }
        (
            Type::NamedGeneric { name: n1, args: a1 },
            Type::NamedGeneric { name: n2, args: a2 },
        ) => {
            n1.inner.0 == n2.inner.0
                && a1.len() == a2.len()
                && a1.iter().zip(a2).all(|(p, q)| types_eq(&p.inner, &q.inner))
        }
        _ => false,
    }
}

/// Mangle a type into its mono-name suffix.  Stable across runs since
/// it is derived purely from the structural Display form.
fn mangle_type(ty: &Type<'_>) -> String {
    match ty {
        Type::Named(id) => id.inner.0.to_string(),
        Type::Unit => "unit".to_string(),
        Type::Unknown => "unknown".to_string(),
        Type::Struct(fields) => {
            let parts: Vec<String> = fields.iter().map(|f| mangle_type(&f.inner)).collect();
            format!("struct_{}", parts.join("_"))
        }
        Type::NamedGeneric { name, args } => {
            let parts: Vec<String> = args.iter().map(|a| mangle_type(&a.inner)).collect();
            format!("{}_{}", name.inner.0, parts.join("_"))
        }
        Type::TypeVar(id) => id.inner.0.to_string(),
        Type::Generic(name, args) => {
            let parts: Vec<String> = args.iter().map(|a| mangle_type(&a.inner)).collect();
            format!("{}_{}", name.inner.0, parts.join("_"))
        }
        Type::Path(seg, rest) => format!("{}_{}", seg.inner.0, mangle_type(&rest.inner)),
    }
}

/// Inverse of [`mangle_type`] for the simple cases — used so the mono
/// cache can round-trip a stored arg list back into Type form when
/// reasoning about already-emitted instances.
fn type_from_string<'src>(s: &str) -> Type<'src> {
    // Conservative: any single identifier becomes a Named.  Compound
    // types (struct_/...) round-trip only as Unknown — the consumers
    // that care (unify against NamedGeneric) compare leaf scalars.
    let boxed: Box<str> = s.to_string().into_boxed_str();
    let leaked: &'static str = Box::leak(boxed);
    let span: SimpleSpan = (0..0).into();
    Type::Named(Ident::new(leaked).with_span(span))
}

/// Side-effect-free version of [`MonoCx::substitute_type`].  Used by
/// the inference path that just needs to surface a return type without
/// queueing further mono jobs.  `NamedGeneric` heads stay as
/// `NamedGeneric` even when their head is a known generic record —
/// the caller knows it's only consulting the type for shape.
fn substitute_type_pure<'src>(ty: &Type<'src>, subst: &Subst<'src>) -> Type<'src> {
    match ty {
        Type::TypeVar(id) => subst
            .get(id.inner.0)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Type::Named(id) => subst
            .get(id.inner.0)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Type::NamedGeneric { name, args } => {
            let new_args: Vec<Spanned<Type<'src>>> = args
                .iter()
                .map(|a| {
                    let sp = a.span;
                    substitute_type_pure(&a.inner, subst).with_span(sp)
                })
                .collect();
            Type::NamedGeneric { name: name.clone(), args: new_args }
        }
        Type::Struct(fields) => Type::Struct(
            fields
                .iter()
                .map(|f| {
                    let sp = f.span;
                    substitute_type_pure(&f.inner, subst).with_span(sp)
                })
                .collect(),
        ),
        _ => ty.clone(),
    }
}

fn static_named<'src>(name: &'static str) -> Type<'src> {
    let span: SimpleSpan = (0..0).into();
    Type::Named(Ident::new(name).with_span(span))
}

fn src_named<'src>(name: &'src str) -> Type<'src> {
    let span: SimpleSpan = (0..0).into();
    Type::Named(Ident::new(name).with_span(span))
}
