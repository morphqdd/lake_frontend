use std::collections::HashMap;

use chumsky::span::{SimpleSpan, SpanWrap, Spanned};

use crate::api::{
    ast::{Branch, Ident, Item, Machine, MachineItem, Type},
    expr::Expr,
};
use crate::error_handle::LakeError;
use crate::loader::ParsedProgram;
#[allow(unused_imports)]
use crate::registry::{
    EnumEntry, ModuleId, ModulePath, ProgramRegistry, VariantPayloadEntry,
};

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
    /// Diagnostics emitted by enum-aware rewrites: unknown variant
    /// (E030), constructor arity mismatch (E031), non-exhaustive enum
    /// match warning (E032).  See docs/state/features/145_enums.md
    /// Phase 2.
    errors: Vec<LakeError>,
    /// agent: generics-142 — set of `<T>` type-parameter names currently
    /// in scope.  Pushed when entering a generic record / machine, popped
    /// when leaving.  Bare type idents matching one of these get rewritten
    /// from `Type::Named` to `Type::TypeVar`.
    type_var_scope: HashMap<&'src str, ()>,
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
            errors: Vec::new(),
            type_var_scope: HashMap::new(),
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
            errors: Vec::new(),
            type_var_scope: HashMap::new(),
        }
    }

    /// Drain diagnostics accumulated by enum-aware rewrites.  Caller
    /// owns them after the resolver pass completes.
    pub fn take_errors(&mut self) -> Vec<LakeError> {
        std::mem::take(&mut self.errors)
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

    /// If `ty` is `Type::Named(<record>)`, return its DEEPLY expanded
    /// `Type::Struct(field_types)` form: each field whose declared type
    /// is itself a record gets recursively expanded too.  All other
    /// shapes pass through unchanged.  Lets backend TupleIndex chain
    /// walkers find a `Type::Struct` at every nesting level without
    /// querying the registry (#058 chained dot-access followup).
    fn expand_record_type(&self, ty: &Type<'src>) -> Type<'src> {
        let Some(reg) = self.registry else {
            return ty.clone();
        };
        self.expand_record_type_in(ty, reg, self.current_module)
    }

    /// Worker for [`expand_record_type`] that threads the OWNING
    /// module of the parent record as the lookup scope.  Cross-module
    /// records (e.g. `Request` imported from `http` whose field type
    /// is `HTTPMethod` — defined in `http` but NOT imported into the
    /// calling module) resolve through `http`'s scope, not the
    /// current module's import set.
    fn expand_record_type_in(
        &self,
        ty: &Type<'src>,
        reg: &crate::registry::ProgramRegistry<'src>,
        lookup_module: crate::registry::ModuleId,
    ) -> Type<'src> {
        let Type::Named(name_id) = ty else {
            return ty.clone();
        };
        // Try the lookup module first, then the current module (handles
        // both cross-module field types and current-module record refs).
        let entry = reg
            .module(lookup_module)
            .records
            .get(name_id.inner.0)
            .or_else(|| reg.module(self.current_module).records.get(name_id.inner.0))
            .or_else(|| {
                // Cross-module: record name might be defined in a module
                // that isn't `lookup_module` or `current_module` (e.g. a
                // field type that wasn't imported into either).  Records
                // are nominally unique across the program — fall back to
                // a linear scan.
                (0..reg.modules.len()).find_map(|i| {
                    let mid = crate::registry::ModuleId(i as u32);
                    reg.module(mid).records.get(name_id.inner.0)
                })
            });
        let Some(entry) = entry else {
            return ty.clone();
        };
        let owning_module = entry.module;
        let span = name_id.span;
        let fields: Vec<Spanned<Type<'src>>> = entry
            .fields
            .iter()
            .map(|(_, fty)| {
                let expanded = self.expand_record_type_in(&fty.inner, reg, owning_module);
                Spanned { inner: expanded, span }
            })
            .collect();
        Type::Struct(fields)
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
                // Strict: type `.N` from the declared element at position
                // N on the receiver's struct.  Covers both user-declared
                // tuples (`ret {i64 buf}`) and the rt-synthesized
                // `{atom buf}` shape (rt_allocate, rt_cstr_to_buf) — the
                // latter already carries `atom` as field 0, so no special
                // case is needed.  Receivers whose type didn't resolve
                // stay `Unknown` rather than silently widening to atom.
                //
                // Records (Type::Named after lowering rewrites DotAccess
                // to TupleIndex) resolve by looking up the indexed field
                // in the registry's RecordEntry.  Without this, code
                // generated by lowering's Phase 1.a `srv.port → srv.0`
                // would type back as Unknown and let_expr would emit
                // `Unknown type '?'`.  #058 followup.
                let recv_ty = self.infer_expr_type(&receiver.inner, receiver.span);
                if let Type::Struct(fields) = recv_ty {
                    fields.get(*index).map(|f| f.inner.clone()).unwrap_or(Type::Unknown)
                } else if let Type::Named(name_id) = &recv_ty {
                    if let Some(reg) = self.registry {
                        let scope = reg.module(self.current_module);
                        if let Some(rec) = scope.records.get(name_id.inner.0) {
                            return rec
                                .fields
                                .get(*index)
                                .map(|(_, ty)| ty.inner.clone())
                                .unwrap_or(Type::Unknown);
                        }
                    }
                    Type::Unknown
                } else {
                    Type::Unknown
                }
            }
            // `buf[i]` always yields a zero-extended byte → i64.
            Expr::Index { .. } => static_named("i64", span),
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
            // Arithmetic, negation, comparisons, and bitwise ops all
            // evaluate to i64.
            Expr::Add(_, _)
            | Expr::Sub(_, _)
            | Expr::Mul(_, _)
            | Expr::Div(_, _)
            | Expr::Neg(_)
            | Expr::Eq(_, _)
            | Expr::Le(_, _)
            | Expr::Ge(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _)
            | Expr::BAnd(_, _)
            | Expr::BOr(_, _)
            | Expr::BXor(_, _)
            | Expr::Shl(_, _)
            | Expr::Shr(_, _) => static_named("i64", span),
            Expr::Jump { ident, .. } => self.infer_jump_return(&ident.inner, span),
            // `pin <call>` is the sync-await sugar for ret-machines —
            // the value flowing back is exactly the inner Jump's
            // return type.  Without this, `let x = pin foo(...)`
            // leaves `x` typed `Unknown`, which trips downstream
            // call-arg matching with `?` placeholders.
            Expr::Pin(inner) => self.infer_expr_type(&inner.inner, inner.span),
            // Record literal: type tag = the record's own name.  Typeck
            // verifies field correctness; here we just name the type.
            Expr::RecordLiteral { name, .. } => src_named(name.inner.0, span),
            // Named field access on a record-typed receiver.
            Expr::DotAccess { receiver, field } => {
                let recv_ty = self.infer_expr_type(&receiver.inner, receiver.span);
                if let Type::Named(rec_name_ident) = &recv_ty {
                    let rec_name = rec_name_ident.inner.0;
                    if let Some(reg) = self.registry {
                        // Local module first.
                        let scope = reg.module(self.current_module);
                        if let Some(entry) = scope.records.get(rec_name) {
                            if let Some((_, field_ty)) =
                                entry.fields.iter().find(|(n, _)| n.inner.0 == field.inner.0)
                            {
                                return field_ty.inner.clone();
                            }
                        }
                        // Cross-module: a record from a sibling module
                        // imported via `+other.{ Foo }` (or transitively
                        // visible as a return type) won't be in the
                        // current module's `records` map.  Scan every
                        // loaded module for a matching pub record so
                        // dot-access propagates field types through
                        // imported records.  Required by stdlib code that
                        // returns records across module boundaries.
                        for m in &reg.modules {
                            if let Some(entry) = m.records.get(rec_name) {
                                if entry.is_pub {
                                    if let Some((_, field_ty)) = entry
                                        .fields
                                        .iter()
                                        .find(|(n, _)| n.inner.0 == field.inner.0)
                                    {
                                        return field_ty.inner.clone();
                                    }
                                }
                            }
                        }
                    }
                }
                Type::Unknown
            }
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
                // Records: check module's records map directly so we can
                // borrow RecordEntry<'src> without going through Resolution<'r>.
                let scope = reg.module(self.current_module);
                if let Some(entry) = scope.records.get(*name) {
                    return src_named(entry.name, span);
                }
                if let Some(r) =
                    reg.resolve_in_module(self.current_module, name)
                {
                    return type_from_str(r.return_type(), span);
                }
                // Imported alias: walk the current module's import table.
                // Namespace imports (target_item == None) can't be invoked
                // bare; the path-form rule handles them via resolve_path.
                if let Some(binding) = scope.imports.get(*name) {
                    if let Some(item) = &binding.target_item {
                        let target_scope = reg.module(binding.target_module);
                        if let Some(entry) = target_scope.records.get(item.as_str()) {
                            return src_named(entry.name, span);
                        }
                        if let Some(r) =
                            reg.resolve_in_module(binding.target_module, item)
                        {
                            return type_from_str(r.return_type(), span);
                        }
                    }
                }
                Type::Unknown
            }
            Expr::Path(segments) => {
                let Some((target_id, item)) =
                    reg.resolve_path(self.current_module, segments)
                else {
                    return Type::Unknown;
                };
                reg.resolve_in_module(target_id, item)
                    .map(|r| type_from_str(r.return_type(), span))
                    .unwrap_or(Type::Unknown)
            }
            _ => Type::Unknown,
        }
    }

    // ── Phase 2 of #145: enum constructor / pattern rewrites ─────────────

    /// Look up an enum by source name through the resolver's registry.
    /// Returns `None` when there is no registry (single-file test mode)
    /// or no enum with that name in scope.
    fn lookup_enum(&self, name: &str) -> Option<&'r EnumEntry<'src>> {
        self.registry.and_then(|r| r.lookup_enum_in(self.current_module, name))
    }

    /// Build the synthetic tagged-tuple `{ tag, payload... }` that
    /// every variant value lowers to.  `tag` is an `i64` literal so
    /// the existing `when` discriminator dispatch handles it without
    /// a special pass.  Caller passes pre-resolved payload arguments
    /// in declaration order.
    fn build_variant_tuple(
        &self,
        tag: u64,
        payload: Vec<Spanned<Expr<'src>>>,
        span: SimpleSpan,
    ) -> Expr<'src> {
        let tag_str: &'static str = Box::leak(tag.to_string().into_boxed_str());
        let i64_ty = Type::Named(Ident::new("i64").with_span(span));
        let mut elems: Vec<Spanned<Expr<'src>>> =
            Vec::with_capacity(1 + payload.len());
        elems.push(Expr::Num(tag_str, i64_ty).with_span(span));
        elems.extend(payload);
        Expr::Tuple(elems)
    }

    /// Inspect a not-yet-resolved expression and return the enum name
    /// when it is a variant constructor — either `Enum.Variant` (a
    /// `DotAccess` with a Var receiver) or `Enum.Variant(args)` (a
    /// `Jump` whose callee is that DotAccess).  Used by the Let-arm
    /// type-inference path so `let x = Enum.Variant(...)` binds `x`
    /// with `Type::Named(EnumName)` rather than the synthetic tagged
    /// tuple shape.
    fn peek_variant_ctor_enum(&self, expr: &Expr<'src>) -> Option<&'src str> {
        let (receiver, _field) = match expr {
            Expr::DotAccess { receiver, field } => (&receiver.inner, field),
            Expr::Jump { ident, .. } => match &ident.inner {
                Expr::DotAccess { receiver, field } => (&receiver.inner, field),
                _ => return None,
            },
            _ => return None,
        };
        let Expr::Var(name, _) = receiver else { return None };
        let entry = self.lookup_enum(name)?;
        Some(entry.name)
    }

    /// Attempt to rewrite `Expr::DotAccess { Var(EnumName), Variant }`
    /// into a tagged tuple.  Returns `None` when the receiver isn't a
    /// known enum.  Variant existence and arity errors land in
    /// `self.errors`.
    fn try_rewrite_enum_dot_ctor(
        &mut self,
        receiver: &Expr<'src>,
        field: &Spanned<Ident<'src>>,
        whole_span: SimpleSpan,
    ) -> Option<Expr<'src>> {
        let Expr::Var(name, _) = receiver else {
            return None;
        };
        let enum_entry = self.lookup_enum(name)?;
        // Cloning the EnumEntry's variants is expensive; copy out just
        // what we need to keep the immutable borrow short.
        let enum_name: &'src str = enum_entry.name;
        let variants_snapshot: Vec<(u64, &'src str, usize)> = enum_entry
            .variants
            .iter()
            .map(|v| {
                let arity = match &v.payload {
                    VariantPayloadEntry::None => 0,
                    VariantPayloadEntry::Tuple(t) => t.len(),
                    VariantPayloadEntry::Record(r) => r.len(),
                };
                (v.tag, v.name, arity)
            })
            .collect();
        let variant_name = field.inner.0;
        let Some((tag, _, expected_arity)) = variants_snapshot
            .iter()
            .find(|(_, n, _)| *n == variant_name)
            .copied()
        else {
            self.errors.push(
                LakeError::new(
                    format!("enum `{}` has no variant `{}`", enum_name, variant_name),
                    field.span,
                )
                .code("E030"),
            );
            return Some(self.build_variant_tuple(0, vec![], whole_span));
        };
        if expected_arity != 0 {
            self.errors.push(
                LakeError::new(
                    format!(
                        "variant `{}.{}` expects {} payload value(s), got 0",
                        enum_name, variant_name, expected_arity,
                    ),
                    whole_span,
                )
                .code("E031"),
            );
        }
        Some(self.build_variant_tuple(tag, vec![], whole_span))
    }

    /// Attempt to rewrite `Expr::Jump { ident: DotAccess(Enum, Variant),
    /// args }` into a tagged tuple.  Args are resolved before being
    /// folded into the tuple.  Returns `None` when the callee isn't a
    /// known enum constructor.
    fn try_rewrite_enum_jump_ctor(
        &mut self,
        callee: &Spanned<Expr<'src>>,
        args: Vec<Spanned<Expr<'src>>>,
        whole_span: SimpleSpan,
    ) -> Option<Result<Expr<'src>, Vec<Spanned<Expr<'src>>>>> {
        let Expr::DotAccess { receiver, field } = &callee.inner else {
            return None;
        };
        let Expr::Var(enum_name, _) = &receiver.inner else {
            return None;
        };
        let enum_entry = self.lookup_enum(enum_name)?;
        let owned_enum_name: &'src str = enum_entry.name;
        let variants_snapshot: Vec<(u64, &'src str, usize)> = enum_entry
            .variants
            .iter()
            .map(|v| {
                let arity = match &v.payload {
                    VariantPayloadEntry::None => 0,
                    VariantPayloadEntry::Tuple(t) => t.len(),
                    VariantPayloadEntry::Record(r) => r.len(),
                };
                (v.tag, v.name, arity)
            })
            .collect();
        let variant_name = field.inner.0;
        let Some((tag, _, expected_arity)) = variants_snapshot
            .iter()
            .find(|(_, n, _)| *n == variant_name)
            .copied()
        else {
            self.errors.push(
                LakeError::new(
                    format!(
                        "enum `{}` has no variant `{}`",
                        owned_enum_name, variant_name,
                    ),
                    field.span,
                )
                .code("E030"),
            );
            return Some(Ok(self.build_variant_tuple(0, vec![], whole_span)));
        };
        // Resolve args first so any nested forms (literals, vars) are
        // typed before they enter the tuple.
        let resolved_args: Vec<Spanned<Expr<'src>>> =
            args.into_iter().map(|a| self.resolve_expr(a)).collect();
        if resolved_args.len() != expected_arity {
            self.errors.push(
                LakeError::new(
                    format!(
                        "variant `{}.{}` expects {} payload value(s), got {}",
                        owned_enum_name, variant_name, expected_arity, resolved_args.len(),
                    ),
                    whole_span,
                )
                .code("E031"),
            );
        }
        Some(Ok(self.build_variant_tuple(tag, resolved_args, whole_span)))
    }

    /// Infer the enum that a `when` scrutinee is matching against.
    /// Walks the cond expression looking for a `Var` whose declared
    /// type in scope (or inline `ty`) is a `Type::Named` matching a
    /// known enum.  Returns the enum's source name when found.
    fn infer_when_enum(&self, cond: &Expr<'src>) -> Option<&'src str> {
        let ty = match cond {
            Expr::Var(name, ty) => {
                if let Type::Named(id) = ty {
                    Some(id.inner.0)
                } else {
                    // Pattern bindings sometimes leave the inline ty
                    // Unknown; the scope still carries the real type.
                    self.scope.get(name).and_then(|t| match t {
                        Type::Named(id) => Some(id.inner.0),
                        _ => None,
                    })
                }
            }
            _ => None,
        }?;
        // Confirm it's actually an enum by name.
        self.lookup_enum(ty).map(|e| e.name)
    }

    /// Rewrite a single `when` arm pattern when the scrutinee is an
    /// enum-typed value.  Each arm `Get` becomes `{ tag }`,
    /// `Post(body)` becomes `{ tag body }`, and the existing tuple-arm
    /// lowering pass in `lowering.rs` then splits the discriminator
    /// off and binds the payload via synthetic lets.
    ///
    /// Returns the rewritten arm pattern.  Wildcards and literal
    /// guards pass through unchanged.
    fn rewrite_when_arm_pattern(
        &mut self,
        enum_name: &'src str,
        arm: Spanned<Expr<'src>>,
        used_variants: &mut Vec<&'src str>,
        has_wildcard: &mut bool,
    ) -> Spanned<Expr<'src>> {
        let arm_span = arm.span;
        // #145 phase 3 — also snapshot per-variant payload types so the
        // payload binder Vars (`Ok(n)` → `Var("n", i64)`) get the right
        // declared type.  Without this the post-relift
        // `let n = __wtp.<i>` keeps `Type::Unknown` and let_expr's
        // codegen rejects the binding.
        let variants_snapshot: Vec<(u64, &'src str, usize, Vec<Type<'src>>)> = {
            let Some(entry) = self.lookup_enum(enum_name) else {
                return arm;
            };
            entry
                .variants
                .iter()
                .map(|v| {
                    let (arity, payload_tys): (usize, Vec<Type<'src>>) = match &v.payload {
                        VariantPayloadEntry::None => (0, Vec::new()),
                        VariantPayloadEntry::Tuple(t) => (t.len(), t.clone()),
                        VariantPayloadEntry::Record(r) => {
                            (r.len(), r.iter().map(|(_, ty)| ty.clone()).collect())
                        }
                    };
                    (v.tag, v.name, arity, payload_tys)
                })
                .collect()
        };
        match arm.inner {
            // `_ -> ...` — wildcard.
            Expr::Var("_", _) => {
                *has_wildcard = true;
                arm
            }
            // `Foo -> ...` — bare variant name (nullary) or a Var
            // catch-all if no variant matches.  We only rewrite when
            // the name is actually a variant of this enum.
            Expr::Var(name, ty) => {
                if let Some((tag, vname, expected)) = variants_snapshot
                    .iter()
                    .find(|(_, n, _, _)| *n == name)
                    .map(|(t, n, e, _)| (*t, *n, *e))
                {
                    if expected != 0 {
                        self.errors.push(
                            LakeError::new(
                                format!(
                                    "variant `{}.{}` requires {} payload binder(s) in pattern",
                                    enum_name, vname, expected,
                                ),
                                arm_span,
                            )
                            .code("E031"),
                        );
                    }
                    used_variants.push(vname);
                    self.build_variant_tuple(tag, vec![], arm_span).with_span(arm_span)
                } else {
                    // Catch-all binding — treated as a wildcard arm by
                    // the existing tuple-arm lowering pass.
                    *has_wildcard = true;
                    Expr::Var(name, ty).with_span(arm_span)
                }
            }
            // `Foo(payload...)` — variant pattern with binders.  We
            // rewrite `Jump { Var("Foo"), [Var("body")] }` into a
            // tuple pattern `{ Num(tag) Var("body") }`.
            Expr::Jump { ident, args } => {
                let variant_name = if let Expr::Var(n, _) = &ident.inner {
                    Some(*n)
                } else {
                    None
                };
                let Some(name) = variant_name else {
                    // Not a variant-shape — re-wrap and resolve normally.
                    let restored = Expr::Jump { ident, args };
                    return restored.with_span(arm_span);
                };
                let matched = variants_snapshot
                    .iter()
                    .find(|(_, n, _, _)| *n == name)
                    .map(|(t, n, e, ts)| (*t, *n, *e, ts.clone()));
                if let Some((tag, vname, expected, payload_tys)) = matched {
                    if expected != args.len() {
                        self.errors.push(
                            LakeError::new(
                                format!(
                                    "variant `{}.{}` pattern expects {} binder(s), got {}",
                                    enum_name, vname, expected, args.len(),
                                ),
                                arm_span,
                            )
                            .code("E031"),
                        );
                    }
                    used_variants.push(vname);
                    // Args in pattern position are binding names (or
                    // `_`); keep them as Var so the tuple-arm lowering
                    // pass emits `let <name> = __wtp.<i>`.  #145 phase 3:
                    // tag each binder Var with its declared payload
                    // type so the synth let inherits a concrete type
                    // (let_expr's codegen rejects Type::Unknown).
                    let payload: Vec<Spanned<Expr<'src>>> = args
                        .into_iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let span = a.span;
                            let payload_ty = payload_tys.get(i).cloned();
                            match a.inner {
                                Expr::Var(name, prev_ty) => {
                                    // Wildcard `_` keeps Type::Unknown
                                    // — lower_when_tuple_patterns omits
                                    // the let entirely so the type
                                    // doesn't matter.
                                    let new_ty = if name == "_" {
                                        prev_ty
                                    } else if matches!(prev_ty, Type::Unknown) {
                                        payload_ty.unwrap_or(prev_ty)
                                    } else {
                                        prev_ty
                                    };
                                    Expr::Var(name, new_ty).with_span(span)
                                }
                                Expr::Num(_, _) => a,
                                other => other.with_span(span),
                            }
                        })
                        .collect();
                    self.build_variant_tuple(tag, payload, arm_span).with_span(arm_span)
                } else {
                    // Unknown variant name in this enum's pattern arm.
                    self.errors.push(
                        LakeError::new(
                            format!("enum `{}` has no variant `{}`", enum_name, name),
                            ident.span,
                        )
                        .code("E030"),
                    );
                    Expr::Jump { ident, args }.with_span(arm_span)
                }
            }
            other => other.with_span(arm_span),
        }
    }

    /// Emit E032 when an enum-typed `when` does not cover every
    /// variant and lacks a wildcard arm.  Phase 2 surfaces this as a
    /// warning by routing through the diagnostic stream (severity
    /// upgrade is a Phase 3 task — see #145).
    fn check_exhaustiveness(
        &mut self,
        enum_name: &'src str,
        used: &[&'src str],
        has_wildcard: bool,
        when_span: SimpleSpan,
    ) {
        if has_wildcard {
            return;
        }
        let Some(entry) = self.lookup_enum(enum_name) else {
            return;
        };
        let missing: Vec<&'src str> = entry
            .variants
            .iter()
            .map(|v| v.name)
            .filter(|n| !used.contains(n))
            .collect();
        if missing.is_empty() {
            return;
        }
        let missing_str = missing.join(", ");
        self.errors.push(
            LakeError::new(
                format!(
                    "non-exhaustive `when` on enum `{}`: missing variant(s) `{}`",
                    enum_name, missing_str,
                ),
                when_span,
            )
            .code("E032"),
        );
    }

    fn resolve_expr(&mut self, expr: Spanned<Expr<'src>>) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Var(name, ty) => Expr::Var(name, self.resolve_var_ty(name, ty)),

            Expr::Let { ident, ty, default } => {
                // #145 phase 3 — if the default is a variant constructor
                // (`Enum.Variant` or `Enum.Variant(args)`), peek the
                // pre-resolved AST for the enum name so the let infers
                // as `Type::Named(EnumName)`.  Without this, the
                // post-rewrite `Tuple([...])` infers as
                // `Type::Struct([i64, …])` and subsequent calls
                // (`unwrap(good)` where `unwrap` takes a `Result`) fail
                // typeck with a "no branch matches" error.
                let variant_enum_name = default
                    .as_ref()
                    .and_then(|d| self.peek_variant_ctor_enum(&d.inner));

                // Pre-bind so the default can reference the binding (matches
                // existing semantics).
                self.bind(ident.inner.0, ty.inner.clone());
                let default = default.map(|d| Box::new(self.resolve_expr(*d)));

                // If the user did not write an annotation, infer the type
                // from the default's resolved expression.  Registry-aware
                // inference covers literals, arithmetic, and call returns.
                let final_ty_inner = if is_unknown(&ty.inner) {
                    if let Some(enum_name) = variant_enum_name {
                        // Variant constructor — type is the enum itself,
                        // not the synthetic tagged tuple shape.
                        src_named(enum_name, ty.span)
                    } else if let Some(d) = &default {
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
                // Enum constructor call: `HTTPMethod.Post("body")`.  We
                // rewrite into a tagged tuple BEFORE the args are
                // resolved (try_rewrite_enum_jump_ctor resolves them
                // itself so it can validate arity against the variant's
                // declared payload).  Non-enum callees fall through to
                // the normal Jump path.
                if let Some(rewritten) =
                    self.try_rewrite_enum_jump_ctor(&ident, args.clone(), span)
                {
                    match rewritten {
                        Ok(e) => e,
                        // The Err variant is unused today; reserved for
                        // future "rewrite refused" paths.
                        Err(restored_args) => Expr::Jump {
                            ident: Box::new(self.resolve_expr(*ident)),
                            args: restored_args
                                .into_iter()
                                .map(|a| self.resolve_expr(a))
                                .collect(),
                        },
                    }
                } else {
                    let ident = Box::new(self.resolve_expr(*ident));
                    let args =
                        args.into_iter().map(|a| self.resolve_expr(a)).collect();
                    Expr::Jump { ident, args }
                }
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
            Expr::BAnd(l, r) => Expr::BAnd(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::BOr(l, r) => Expr::BOr(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::BXor(l, r) => Expr::BXor(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Shl(l, r) => Expr::Shl(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Shr(l, r) => Expr::Shr(
                Box::new(self.resolve_expr(*l)),
                Box::new(self.resolve_expr(*r)),
            ),
            Expr::Neg(inner) => Expr::Neg(Box::new(self.resolve_expr(*inner))),
            Expr::Ret(inner) => Expr::Ret(Box::new(self.resolve_expr(*inner))),
            Expr::Pin(inner) => Expr::Pin(Box::new(self.resolve_expr(*inner))),

            Expr::When { cond, branches } => {
                let cond = Box::new(self.resolve_expr(*cond));
                // Did the scrutinee resolve to an enum-typed value?
                // If so, arms with bare variant names (`Get -> …`) or
                // variant call shapes (`Post(body) -> …`) get
                // rewritten into the tagged-tuple form the existing
                // tuple-arm lowering pass already understands.
                let enum_name = self.infer_when_enum(&cond.inner);
                let mut used_variants: Vec<&'src str> = Vec::new();
                let mut has_wildcard = false;
                let branches: Vec<_> = branches
                    .into_iter()
                    .map(|(pat, body)| {
                        let pat = if let Some(en) = enum_name {
                            // First resolve sub-expressions (e.g.
                            // nested calls) THEN apply the variant
                            // rewrite.  The variant rewrite operates
                            // on the un-resolved arm shape (Var or
                            // Jump-with-Var-callee), so we must
                            // dispatch BEFORE the generic resolve_expr
                            // path mangles it (e.g. turning a Var into
                            // a resolved Var with a non-Unknown ty).
                            self.rewrite_when_arm_pattern(
                                en,
                                pat,
                                &mut used_variants,
                                &mut has_wildcard,
                            )
                        } else {
                            self.resolve_expr(pat)
                        };
                        // #145 phase 4 — variant pattern binders
                        // (`Ok(n)` → Tuple([Num, Var("n", i64)])) must
                        // be in scope while the arm body resolves so
                        // references like `ret n + 100` carry the
                        // declared payload type.  Snapshot/restore the
                        // surrounding scope to keep bindings local to
                        // the arm.  See docs/state/features/145_enums.md.
                        let saved_scope = self.scope.clone();
                        if let Expr::Tuple(elems) = &pat.inner {
                            for (i, sub) in elems.iter().enumerate() {
                                if i == 0 {
                                    continue;
                                }
                                if let Expr::Var(name, ty) = &sub.inner {
                                    if *name != "_" {
                                        self.bind(name, ty.clone());
                                    }
                                }
                            }
                        }
                        let body = body
                            .into_iter()
                            .map(|e| self.resolve_expr(e))
                            .collect();
                        self.scope = saved_scope;
                        (pat, body)
                    })
                    .collect();
                if let Some(en) = enum_name {
                    self.check_exhaustiveness(en, &used_variants, has_wildcard, span);
                }
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
            Expr::TupleIndex { receiver, index } => {
                // Resolve the receiver normally first, then — if it's a
                // bare Var with a record type — rewrite its `ty` to the
                // structural expansion (`Type::Named(Foo)` →
                // `Type::Struct([fields...])`).  Only this Var instance is
                // touched; other references to the same name (call args,
                // self()-dispatch) keep the nominal tag they need for
                // branch lookup.  Without this, typeck + the call-hash
                // arg classifier see `?` for `r.0` because both code
                // paths only know how to read `Type::Struct(fields)`.
                let resolved_recv = self.resolve_expr(*receiver);
                let recv_span = resolved_recv.span;
                let rewritten_inner = match resolved_recv.inner {
                    Expr::Var(name, ty) => {
                        let expanded = self.expand_record_type(&ty);
                        Expr::Var(name, expanded)
                    }
                    other => other,
                };
                Expr::TupleIndex {
                    receiver: Box::new(rewritten_inner.with_span(recv_span)),
                    index,
                }
            }
            Expr::Index { receiver, index } => Expr::Index {
                receiver: Box::new(self.resolve_expr(*receiver)),
                index: Box::new(self.resolve_expr(*index)),
            },
            Expr::DotAccess { receiver, field } => {
                // Nullary enum constructor: `HTTPMethod.Get`.  Detect
                // before resolving the receiver, otherwise `HTTPMethod`
                // would surface as an undeclared variable diagnostic.
                if let Some(rewritten) =
                    self.try_rewrite_enum_dot_ctor(&receiver.inner, &field, span)
                {
                    rewritten
                } else {
                    Expr::DotAccess {
                        receiver: Box::new(self.resolve_expr(*receiver)),
                        field,
                    }
                }
            }
            Expr::RecordLiteral { name, fields } => Expr::RecordLiteral {
                name,
                fields: fields
                    .into_iter()
                    .map(|(k, v)| (k, self.resolve_expr(v)))
                    .collect(),
            },

            other => other,
        };
        inner.with_span(span)
    }

    /// agent: generics-142 — rewrite any `Type::Named(T)` whose name is
    /// currently in the type-var scope into `Type::TypeVar(T)`.  Walks
    /// recursively so nested type expressions (`Vec<T>`, `{ T i64 }`)
    /// are handled uniformly.  No-op when the type-var scope is empty,
    /// which is the case for every non-generic decl.
    fn rewrite_type_vars(&self, ty: Type<'src>) -> Type<'src> {
        if self.type_var_scope.is_empty() {
            return ty;
        }
        match ty {
            Type::Named(ident) => {
                if self.type_var_scope.contains_key(ident.inner.0) {
                    Type::TypeVar(ident)
                } else {
                    Type::Named(ident)
                }
            }
            Type::Generic(name, args) => {
                let args = args
                    .into_iter()
                    .map(|a| {
                        let span = a.span;
                        self.rewrite_type_vars(a.inner).with_span(span)
                    })
                    .collect();
                Type::Generic(name, args)
            }
            Type::NamedGeneric { name, args } => {
                let args = args
                    .into_iter()
                    .map(|a| {
                        let span = a.span;
                        self.rewrite_type_vars(a.inner).with_span(span)
                    })
                    .collect();
                Type::NamedGeneric { name, args }
            }
            Type::Path(seg, rest) => {
                let span = rest.span;
                let rewritten = self.rewrite_type_vars(*rest.inner);
                Type::Path(seg, Box::new(rewritten).with_span(span))
            }
            Type::Struct(fields) => {
                let fields = fields
                    .into_iter()
                    .map(|f| {
                        let span = f.span;
                        self.rewrite_type_vars(f.inner).with_span(span)
                    })
                    .collect();
                Type::Struct(fields)
            }
            // TypeVar / Unit / Unknown pass through unchanged.
            other => other,
        }
    }

    /// agent: generics-142 — rewrite types inside a single [`Spanned<Type>`].
    fn rewrite_spanned_type(&self, ty: Spanned<Type<'src>>) -> Spanned<Type<'src>> {
        let span = ty.span;
        self.rewrite_type_vars(ty.inner).with_span(span)
    }

    fn resolve_branch(&mut self, branch: Branch<'src>) -> Branch<'src> {
        let saved = self.scope.clone();

        // agent: generics-142 — rewrite pattern types and ret_ty so that
        // any reference to a `<T>` in the enclosing decl resolves to
        // `Type::TypeVar` instead of `Type::Named`.  Cheap no-op when
        // the type-var scope is empty.
        let patterns: Vec<Spanned<crate::api::ast::Pattern<'src>>> = branch
            .patterns
            .into_iter()
            .map(|p| {
                let span = p.span;
                let mut pat = p.inner;
                pat.ty = self.rewrite_spanned_type(pat.ty);
                pat.with_span(span)
            })
            .collect();
        let ret_ty = branch.ret_ty.map(|t| self.rewrite_spanned_type(t));

        // Bind all non-wildcard, non-literal-guard pattern variables into scope.
        for pat in &patterns {
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
        Branch::new(branch.label, patterns, ret_ty, body)
    }

    fn resolve_machine(&mut self, machine: Machine<'src>) -> Machine<'src> {
        // agent: generics-142 — push declared `<T U ...>` into the
        // type-var scope before walking branches so `T` in patterns /
        // returns becomes `Type::TypeVar`.  Restored at the end.
        let saved_type_vars = self.type_var_scope.clone();
        for tp in &machine.generics {
            self.type_var_scope.insert(tp.inner.0, ());
        }

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

        self.type_var_scope = saved_type_vars;

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
    program: ParsedProgram<'src>,
    registry: &mut ProgramRegistry<'src>,
) -> ParsedProgram<'src> {
    // Drop the diagnostic stream — callers that need it use
    // [`resolve_program_with_errors`] directly.  Existing call sites
    // (tests, the lowering pipeline) keep their pre-Phase-2 contract.
    resolve_program_with_errors(program, registry).0
}

/// Same as [`resolve_program`] but also returns diagnostics emitted by
/// the resolver's enum-aware rewrites (E030 / E031 / E032).  Callers
/// that participate in the multi-stage error bag (the prelude
/// `build_program` pipeline) use this entry point so per-module
/// diagnostics get tagged with the right source path.
pub fn resolve_program_with_errors<'src>(
    mut program: ParsedProgram<'src>,
    registry: &mut ProgramRegistry<'src>,
) -> (ParsedProgram<'src>, Vec<LakeError>) {
    let modules = std::mem::take(&mut program.modules);
    let mut resolved_modules = Vec::with_capacity(modules.len());
    let mut all_errors: Vec<LakeError> = Vec::new();

    for module in modules {
        let id = registry
            .module_id_for_path(&module.module_path)
            .expect("populate_from must run before resolve_program");
        registry.set_current(id);

        let mut resolver = Resolver::with_registry(registry, id);
        let resolved_ast = resolver.resolve(module.ast);
        let mp = module.source_path.clone();
        for mut e in resolver.take_errors() {
            if e.source_path.is_none() {
                e.source_path = Some(mp.clone());
            }
            all_errors.push(e);
        }

        resolved_modules.push(crate::loader::ParsedModule {
            module_path: module.module_path,
            source_path: module.source_path,
            ast: resolved_ast,
        });
    }

    program.modules = resolved_modules;
    (program, all_errors)
}

// ── inference helpers ──────────────────────────────────────────────────────

/// Build a `Type::Named` from a `&'static` name plus a span.  Used by the
/// inference engine where the type's textual identifier is fixed at compile
/// time (e.g. `"i64"` from arithmetic).
fn static_named<'src>(name: &'static str, span: SimpleSpan) -> Type<'src> {
    Type::Named(Ident::new(name).with_span(span))
}

/// Build a `Type::Named` from a source-lifetime name plus a span.  Used when
/// the name comes from source text (e.g. a record declaration name).
fn src_named<'src>(name: &'src str, span: SimpleSpan) -> Type<'src> {
    Type::Named(Ident::new(name).with_span(span))
}

/// Render a registry-supplied return-type string into a [`Type`].  Only the
/// small set of types Lake currently exposes through call signatures
/// (i64 / str / bool / pid / unit) round-trip; richer return types stay
/// unresolved so the caller can fall back to "user must annotate".
fn type_from_str<'src>(s: &str, span: SimpleSpan) -> Type<'src> {
    let s = s.trim();
    // Anonymous struct `{ T1 T2 ... }` — round-trips the Display form
    // used by the rt-fn signature registry (e.g. rt_allocate now
    // returns `{atom buf}`).
    if let Some(inner) = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
        let inner = inner.trim();
        if inner.is_empty() {
            return Type::Unit;
        }
        let fields: Vec<_> = inner
            .split_whitespace()
            .map(|tok| type_from_str::<'src>(tok, span).with_span(span))
            .collect();
        return Type::Struct(fields);
    }
    match s {
        "i64" => static_named("i64", span),
        "str" => static_named("str", span),
        "buf" => static_named("buf", span),
        "bool" => static_named("bool", span),
        "pid" => static_named("pid", span),
        "atom" => static_named("atom", span),
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

    /// Regression: declared `{i64 buf}` type on a let binding must flow into scope
    /// so that `.N` indexing on the bound variable resolves to the declared field
    /// types rather than `Unknown`.  Without the re-bind after default inference the
    /// scope entry stayed `Unknown` and r.0 / r.1 both resolved to `Unknown`.
    /// See docs/state/bugs/tuple_field_atom_assumption.md in the outer repo.
    #[test]
    fn let_struct_annotation_propagates_to_tuple_index() {
        // Explicit annotation: `let r {i64 buf} = { 1 "x" }`.
        // The declared type must win; r.0 → i64, r.1 → buf.
        let body = first_branch_body(r#"m is { _ -> { let r {i64 buf} = { 1 "x" } r.0 r.1 } }"#);

        // body[0] is the let — confirm declared type is preserved.
        let Expr::Let { ty: let_ty, .. } = &body[0] else {
            panic!("expected Let, got {:?}", body[0])
        };
        assert!(
            matches!(&let_ty.inner, Type::Struct(_)),
            "expected Struct on let ty, got {:?}", let_ty.inner
        );

        // body[1] is `r.0` — receiver must carry {i64 buf} after resolution.
        let Expr::TupleIndex { receiver: recv0, index: idx0 } = &body[1] else {
            panic!("expected TupleIndex for r.0, got {:?}", body[1])
        };
        assert_eq!(*idx0, 0);
        let Expr::Var(_, ty0) = &recv0.inner else {
            panic!("expected Var inside r.0 receiver, got {:?}", recv0.inner)
        };
        let Type::Struct(fields0) = ty0 else {
            panic!("expected Struct in resolved r for r.0, got {:?}", ty0)
        };
        assert!(
            matches!(&fields0[0].inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected r.0 field type = i64, got {:?}", fields0[0].inner
        );

        // body[2] is `r.1` — field 1 must resolve to buf.
        let Expr::TupleIndex { receiver: recv1, index: idx1 } = &body[2] else {
            panic!("expected TupleIndex for r.1, got {:?}", body[2])
        };
        assert_eq!(*idx1, 1);
        let Expr::Var(_, ty1) = &recv1.inner else {
            panic!("expected Var inside r.1 receiver, got {:?}", recv1.inner)
        };
        let Type::Struct(fields1) = ty1 else {
            panic!("expected Struct in resolved r for r.1, got {:?}", ty1)
        };
        assert!(
            matches!(&fields1[1].inner, Type::Named(i) if i.inner == Ident::new("buf")),
            "expected r.1 field type = buf, got {:?}", fields1[1].inner
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

    /// Collect inferred let-binding types from `main`'s first branch body.
    /// Returns a map of variable name → display string of the inferred type.
    fn infer_let_types_in_main(src: &str) -> std::collections::HashMap<String, String> {
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
            .find(|m| m.inner.ident.inner.0 == "main")
            .expect("no main machine");
        let MachineItem::Branch(b) = &machine.inner.items[0].inner else {
            panic!("expected Branch")
        };
        let mut map = std::collections::HashMap::new();
        for e in &b.body {
            if let Expr::Let { ident, ty, .. } = &e.inner {
                map.insert(ident.inner.0.to_string(), format!("{}", ty.inner));
            }
        }
        map
    }

    #[test]
    fn record_ctor_call_types_as_record_name() {
        let src = r#"
            Request is { method buf  path buf }
            main is { _ -> { let r = Request("GET" "/") } }
        "#;
        let types = infer_let_types_in_main(src);
        let r_ty = types.get("r").expect("let r missing");
        assert_eq!(r_ty, "Request");
    }

    #[test]
    fn record_dot_access_returns_field_type() {
        let src = r#"
            Request is { method buf  path buf }
            main is { _ -> {
                let r = Request("GET" "/")
                let m = r.method
            } }
        "#;
        let types = infer_let_types_in_main(src);
        assert_eq!(types.get("m").unwrap(), "buf");
    }

    #[test]
    fn record_literal_types_as_record_name() {
        let src = r#"
            Request is { method buf  path buf }
            main is { _ -> { let r = Request { method = "GET" path = "/" } } }
        "#;
        let types = infer_let_types_in_main(src);
        assert_eq!(types.get("r").unwrap(), "Request");
    }

    /// Regression: `.N` on a value returned from a ret-machine that
    /// declares `ret {i64 buf}` must read the field types from the
    /// declared tuple, not assume `{atom T}`.  Before the resolver
    /// learned to thread `Var.ty` into `TupleIndex.receiver`, typeck
    /// reported `r.0` as `atom`, breaking call-arg matching for any
    /// non-`{atom T}` tuple shape.
    #[test]
    fn tuple_index_on_machine_struct_return_keeps_declared_types() {
        let src = r#"
parse_request is { _ buf -> ret { i64 buf } { ret { 1 b } } }
caller is { _ -> { let r = parse_request(b) let m = r.0 let p = r.1 } }
"#;
        let body = resolve_with_reg(src);
        // body[1] is `let m = r.0` — declared element type is i64.
        let Expr::Let { ty: ty_m, .. } = &body[1] else {
            panic!("expected second Let (m = r.0), got {:?}", body[1])
        };
        assert!(
            matches!(&ty_m.inner, Type::Named(i) if i.inner == Ident::new("i64")),
            "expected r.0 -> i64, got {ty_m:?}"
        );
        // body[2] is `let p = r.1` — declared element type is buf.
        let Expr::Let { ty: ty_p, .. } = &body[2] else {
            panic!("expected third Let (p = r.1), got {:?}", body[2])
        };
        assert!(
            matches!(&ty_p.inner, Type::Named(i) if i.inner == Ident::new("buf")),
            "expected r.1 -> buf, got {ty_p:?}"
        );
    }

    // ── Phase 2 of #145: enum constructor / pattern rewrites ─────────────

    /// Resolve `src`, return both the first branch body of `main` AND
    /// the resolver-stage diagnostics so each Phase-2 test can inspect
    /// the rewritten AST and any E03x errors in one shot.
    fn resolve_with_reg_errs(
        src: &str,
    ) -> (Vec<Expr<'_>>, Vec<crate::error_handle::LakeError>) {
        let module = parsed_module(src);
        let program = crate::loader::ParsedProgram { modules: vec![module] };
        let mut reg: crate::registry::ProgramRegistry<'_> =
            crate::registry::ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populate");
        let (resolved, errs) =
            super::resolve_program_with_errors(program, &mut reg);
        let m = resolved.root();
        let machine = m
            .ast
            .iter()
            .filter_map(|it| match &it.inner {
                Item::Machine(mm) => Some(mm),
                _ => None,
            })
            .last()
            .expect("at least one machine");
        let MachineItem::Branch(b) = &machine.inner.items[0].inner else {
            panic!()
        };
        let body = b.body.iter().map(|e| e.inner.clone()).collect();
        // Leak the errors with the same lifetime as `body` — they share
        // the source &'src str.  We immediately consume them.
        (body, errs)
    }

    fn extract_tuple<'a, 'src>(e: &'a Expr<'src>) -> &'a Vec<chumsky::span::Spanned<Expr<'src>>> {
        match e {
            Expr::Tuple(elems) => elems,
            other => panic!("expected Tuple, got {other:?}"),
        }
    }

    fn extract_num(e: &Expr<'_>) -> i64 {
        match e {
            Expr::Num(s, _) => crate::api::expr::parse_int_literal(s).expect("int"),
            other => panic!("expected Num, got {other:?}"),
        }
    }

    #[test]
    fn enum_nullary_ctor_rewrites_to_tag_tuple() {
        let src = r#"
            HTTPMethod is enum { Get Post(buf) }
            main is { _ -> { let m = HTTPMethod.Get } }
        "#;
        let (body, errs) = resolve_with_reg_errs(src);
        assert!(errs.is_empty(), "unexpected errs: {:?}", errs);
        let Expr::Let { default: Some(d), .. } = &body[0] else {
            panic!("expected Let, got {:?}", body[0])
        };
        let elems = extract_tuple(&d.inner);
        assert_eq!(elems.len(), 1);
        assert_eq!(extract_num(&elems[0].inner), 0);
    }

    #[test]
    fn enum_tuple_ctor_rewrites_with_payload() {
        let src = r#"
            HTTPMethod is enum { Get Post(buf) }
            main is { _ -> { let m = HTTPMethod.Post("body") } }
        "#;
        let (body, errs) = resolve_with_reg_errs(src);
        assert!(errs.is_empty(), "unexpected errs: {:?}", errs);
        let Expr::Let { default: Some(d), .. } = &body[0] else {
            panic!("expected Let, got {:?}", body[0])
        };
        let elems = extract_tuple(&d.inner);
        assert_eq!(elems.len(), 2, "tag + payload");
        assert_eq!(extract_num(&elems[0].inner), 1);
        assert!(
            matches!(&elems[1].inner, Expr::String("body", _)),
            "payload should be the literal, got {:?}", elems[1].inner
        );
    }

    #[test]
    fn enum_record_payload_ctor_positional_form() {
        // Phase 2 ships positional form for record-payload variants
        // (`Patch("x")`); the named-field surface `Patch { from = "x" }`
        // depends on a parser extension and is deferred.
        let src = r#"
            HTTPMethod is enum { Get Post(buf) Patch { from buf } }
            main is { _ -> { let m = HTTPMethod.Patch("x") } }
        "#;
        let (body, errs) = resolve_with_reg_errs(src);
        assert!(errs.is_empty(), "unexpected errs: {:?}", errs);
        let Expr::Let { default: Some(d), .. } = &body[0] else { panic!() };
        let elems = extract_tuple(&d.inner);
        assert_eq!(elems.len(), 2);
        assert_eq!(extract_num(&elems[0].inner), 2);
    }

    #[test]
    fn enum_unknown_variant_errors_e030() {
        let src = r#"
            HTTPMethod is enum { Get Post(buf) }
            main is { _ -> { let m = HTTPMethod.Bogus } }
        "#;
        let (_body, errs) = resolve_with_reg_errs(src);
        assert!(
            errs.iter().any(|e| e.code.as_deref() == Some("E030")),
            "expected E030, got {:?}", errs.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn enum_wrong_arity_errors_e031() {
        let src = r#"
            HTTPMethod is enum { Get Post(buf) }
            main is { _ -> { let m = HTTPMethod.Post() } }
        "#;
        let (_body, errs) = resolve_with_reg_errs(src);
        assert!(
            errs.iter().any(|e| e.code.as_deref() == Some("E031")),
            "expected E031, got {:?}", errs.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn enum_when_patterns_rewrite_to_tagged_tuples() {
        let src = r#"
            HTTPMethod is enum { Get Post(buf) }
            main is { m HTTPMethod -> { when m { Get -> { 1 } Post(body) -> { 2 } _ -> { 0 } } } }
        "#;
        let (body, errs) = resolve_with_reg_errs(src);
        assert!(errs.is_empty(), "unexpected errs: {:?}", errs);
        let Expr::When { branches, .. } = &body[0] else {
            panic!("expected When, got {:?}", body[0])
        };
        assert_eq!(branches.len(), 3);
        // Arm 0: Get → { 0 }
        let elems = extract_tuple(&branches[0].0.inner);
        assert_eq!(elems.len(), 1);
        assert_eq!(extract_num(&elems[0].inner), 0);
        // Arm 1: Post(body) → { 1, body }
        let elems = extract_tuple(&branches[1].0.inner);
        assert_eq!(elems.len(), 2);
        assert_eq!(extract_num(&elems[0].inner), 1);
        assert!(matches!(&elems[1].inner, Expr::Var("body", _)));
        // Arm 2: _ stays as wildcard.
        assert!(matches!(&branches[2].0.inner, Expr::Var("_", _)));
    }

    #[test]
    fn enum_when_non_exhaustive_warns_e032() {
        let src = r#"
            HTTPMethod is enum { Get Post(buf) }
            main is { m HTTPMethod -> { when m { Get -> { 1 } } } }
        "#;
        let (_body, errs) = resolve_with_reg_errs(src);
        assert!(
            errs.iter().any(|e| e.code.as_deref() == Some("E032")),
            "expected E032, got {:?}", errs.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn enum_when_with_wildcard_is_exhaustive() {
        let src = r#"
            HTTPMethod is enum { Get Post(buf) }
            main is { m HTTPMethod -> { when m { Get -> { 1 } _ -> { 0 } } } }
        "#;
        let (_body, errs) = resolve_with_reg_errs(src);
        assert!(
            !errs.iter().any(|e| e.code.as_deref() == Some("E032")),
            "unexpected E032: {:?}", errs.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }
}
