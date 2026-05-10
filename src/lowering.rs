//! Lowering pass — desugars `ret`-typed machines into plain message-passing
//! actors so the rest of the pipeline (resolver, typeck, codegen) can stay
//! oblivious to the synchronous-return convention.
//!
//! Two cooperating transformations:
//!
//!   * **Callee side.** Every branch with a `-> ret <type>` annotation gets
//!     an implicit `__caller pid` parameter prepended to its pattern list.
//!     Inside the body, `Expr::Ret(x)` is rewritten as a 2-arg send to
//!     that `__caller` carrying `(self, x)`, and the body's last
//!     expression — the Rust-style implicit return — is wrapped the same
//!     way unless it already terminates with a `__caller(...)` send.
//!
//!   * **Caller side.** When `let r = M(args)` is seen and `M` resolves to
//!     a ret-machine:
//!
//!       1. The call is rewritten as `let __ret_N_pid = M(self ...args)`
//!          so the caller captures the spawned actor's pid.
//!       2. The remainder of the enclosing branch body is wrapped in
//!          `wait __ret_N_pid { __sender pid r <ret_ty> -> { ...rest } }`,
//!          where the wait filter restricts dispatch to that one pid so
//!          unrelated messages don't accidentally satisfy the receive.
//!
//!     A bare `M(args)` (no let) is rewritten as `M(0 ...args)` — the
//!     null-pid sentinel checked by `send_expr`'s runtime guard.  The
//!     ret-machine still runs but its eventual `__caller(self X)` send is
//!     silently dropped.
//!
//! The lowering only inspects [`ProgramRegistry`]; nothing in the registry
//! changes.  Run between two `populate_from` calls so the second populate
//! captures the freshly-prepended `__caller` parameter on every
//! ret-machine signature (and so caller-site lookups see the rewritten
//! AST).

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use chumsky::span::{SimpleSpan, SpanWrap, Spanned};

use crate::api::ast::{Branch, Ident, Item, Machine, MachineItem, Pattern, Type};
use crate::api::expr::Expr;
use crate::loader::{ParsedModule, ParsedProgram};
use crate::registry::{ImportBinding, ModuleId, ModulePath, ProgramRegistry};

/// Lower every machine in every loaded module.  Idempotent at the AST
/// level: running twice on already-lowered output is a no-op (because
/// `Expr::Ret` won't appear and ret-typed branches already have
/// `__caller` prepended).
pub fn lower_program<'src>(
    program: ParsedProgram<'src>,
    registry: &ProgramRegistry<'src>,
) -> ParsedProgram<'src> {
    // Walk the AST once to collect every ret-machine's name + return type.
    // Cross-module lookups consult this map plus the registry's import
    // bindings.
    let ret_machines = collect_ret_machines(&program);

    let counter = Rc::new(Cell::new(0u32));

    let modules = program
        .modules
        .into_iter()
        .map(|module| {
            let module_id = registry
                .module_id_for_path(&module.module_path)
                .expect("populate_from must run before lower_program");
            let cx = LowerCx {
                registry,
                module_id,
                ret_machines: &ret_machines,
                counter: counter.clone(),
            };
            cx.lower_module(module)
        })
        .collect();

    ParsedProgram { modules }
}

// ── ret-machine table ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RetKey {
    module: ModulePath,
    /// Source-visible machine name.
    name: String,
}

fn collect_ret_machines<'src>(program: &ParsedProgram<'src>) -> HashMap<RetKey, String> {
    let mut out = HashMap::new();
    for module in &program.modules {
        for item in &module.ast {
            if let Item::Machine(m) = &item.inner {
                if let Some(ret_ty) = first_ret_ty(&m.inner) {
                    out.insert(
                        RetKey {
                            module: module.module_path.clone(),
                            name: m.inner.ident.inner.0.to_string(),
                        },
                        ret_ty,
                    );
                }
            }
        }
    }
    out
}

/// The return type of the first branch that has one.  Branches of a single
/// machine that disagree on `ret_ty` are a typeck issue, not a lowering
/// one — we just take the first answer.
fn first_ret_ty(machine: &Machine<'_>) -> Option<String> {
    machine.items.iter().find_map(|item| {
        if let MachineItem::Branch(b) = &item.inner {
            b.ret_ty.as_ref().map(|t| t.inner.to_string())
        } else {
            None
        }
    })
}

// ── per-module context ─────────────────────────────────────────────────────

struct LowerCx<'a, 'src> {
    registry: &'a ProgramRegistry<'src>,
    module_id: ModuleId,
    ret_machines: &'a HashMap<RetKey, String>,
    counter: Rc<Cell<u32>>,
}

impl<'a, 'src> LowerCx<'a, 'src> {
    fn lower_module(&self, module: ParsedModule<'src>) -> ParsedModule<'src> {
        let ast = module
            .ast
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    Item::Machine(m) => {
                        let machine_span = m.span;
                        Item::Machine(self.lower_machine(m.inner).with_span(machine_span))
                            .with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect();

        ParsedModule {
            module_path: module.module_path,
            source_path: module.source_path,
            ast,
        }
    }

    fn lower_machine(&self, mut machine: Machine<'src>) -> Machine<'src> {
        machine.items = machine
            .items
            .into_iter()
            .map(|item| {
                let span = item.span;
                match item.inner {
                    MachineItem::Branch(b) => {
                        MachineItem::Branch(self.lower_branch(b)).with_span(span)
                    }
                    other => other.with_span(span),
                }
            })
            .collect();
        machine
    }

    fn lower_branch(&self, branch: Branch<'src>) -> Branch<'src> {
        let is_ret_branch = branch.ret_ty.is_some();
        let mut patterns = branch.patterns;
        let body = branch.body;

        // Idempotency: a branch already lowered will start with a
        // `__caller pid` pattern.  Skip prepending in that case so a
        // second pass over the same AST doesn't double-wrap.
        let already_lowered = patterns
            .first()
            .map(|p| p.inner.ident.inner.0 == "__caller")
            .unwrap_or(false);

        if is_ret_branch && !already_lowered {
            let synth_span = patterns.first().map(|p| p.span).unwrap_or((0..0).into());
            patterns.insert(0, synth_caller_pattern(synth_span));
        }

        let body = self.lower_body(body, is_ret_branch);

        Branch::new(branch.label, patterns, branch.ret_ty, body)
    }

    /// Walk a body sequentially, applying both kinds of lowering.  Returns
    /// the new body; the length may change because `let r = M(args)`
    /// expands to two statements (the call, then a wait that wraps the
    /// remainder).
    fn lower_body(
        &self,
        body: Vec<Spanned<Expr<'src>>>,
        is_ret_branch: bool,
    ) -> Vec<Spanned<Expr<'src>>> {
        // Phase -1: collapse module-qualified Path callees into bare
        // Var references.  Backend's jump_expr only knows how to call
        // by name (flat symbol space at the codegen layer), so the
        // path-form `io:println(s)` is rewritten to `println(s)` once
        // resolve_path has confirmed the target.  Same-named items
        // across modules will collide at predeclare_machine — a
        // problem for whoever introduces the collision, not this
        // pass.
        let body: Vec<_> = body
            .into_iter()
            .map(|e| self.flatten_paths(e))
            .collect();

        // Phase 0a: inline `const` values.  Every `Expr::Var(name, _)` whose
        // bare-name resolution lands on a `Resolution::Const(_)` is
        // replaced by the literal expression encoded by the const.
        // After this pass the rest of the pipeline never sees const
        // references — codegen needs no special-case lookup.
        let body: Vec<_> = body
            .into_iter()
            .map(|e| self.inline_consts(e))
            .collect();

        // Phase 0b: expand `let { a b } = expr` destructure.  Each
        // LetTuple statement is replaced by N+1 ordinary lets:
        // a synthetic `__dst_<id>` for the source value, plus one
        // `let f = __dst_<id>.<i>` per field.  Sequential expansion
        // keeps source ordering stable.
        let body = self.expand_let_tuple(body);

        // Phase 1: recursive `Expr::Ret` → `__caller(self X)` rewrite.
        let body: Vec<_> = body
            .into_iter()
            .map(|e| self.rewrite_ret_in_expr(e))
            .collect();

        // Phase 1.5: lift nested calls out of argument position.
        // Backend's pure_expr can fold a Var / Num / arith but not a
        // Jump, so `f(g(x))` becomes `let __lift_<id> = g(x); f(__lift_<id>)`.
        // Applied AFTER ret-rewrite because that pass synthesises the
        // most common nested-call shape: `__caller(self X)` where X is
        // a call.  Recursive across all expressions in the body so
        // tuple elements (`{ ok g(x) }`) and nested arg lists
        // (`f(g(h(x)))`) both flatten.
        let body = self.lift_nested_calls(body);

        // Phase 1b: `pin <expr>` → `let __pin_<id> = <expr>`.  Done
        // before the let-with-ret-target sweep so the synthesised let
        // is itself caught by that pass when the inner expr is a
        // ret-machine call.  Non-ret callees pass through as a let
        // with an unused binding, which is harmless.
        let body: Vec<_> = body
            .into_iter()
            .map(|e| self.rewrite_pin(e))
            .collect();

        // Phase 2: caller-side desugaring.  We iterate top-down; a
        // let-with-ret-call folds the remainder of the body into a wait
        // body, which itself may contain further ret-calls.  The
        // recursive call to `lower_body` on that nested body handles them.
        let mut new_body: Vec<Spanned<Expr<'src>>> = Vec::with_capacity(body.len());
        let mut iter = body.into_iter();
        while let Some(expr) = iter.next() {
            if self.is_let_with_ret_target(&expr) {
                let rest: Vec<_> = iter.by_ref().collect();
                let mut emitted = self.expand_let_with_ret(expr, rest, is_ret_branch);
                new_body.append(&mut emitted);
                break;
            }
            if let Some(rewritten) = self.try_rewrite_bare_ret_call(&expr) {
                new_body.push(rewritten);
                continue;
            }
            new_body.push(expr);
        }

        // Phase 3: tail wrap for ret-branches.  Skip when the last
        // expression is already a __caller send (the user wrote `ret X`
        // there) or when the body is empty (ret-typed branch with no
        // value is a typeck issue, not ours to solve).
        if is_ret_branch
            && let Some(last) = new_body.last_mut()
            && !is_caller_send(&last.inner)
        {
            wrap_in_caller_send(last);
        }

        new_body
    }

    /// Expand every `Expr::LetTuple { fields, default }` in a body
    /// into a synthetic temporary binding plus one ordinary `Let`
    /// per field.  Run after const inlining so the destructured
    /// expression has already had any const folded into a literal.
    fn expand_let_tuple(
        &self,
        body: Vec<Spanned<Expr<'src>>>,
    ) -> Vec<Spanned<Expr<'src>>> {
        let mut out = Vec::with_capacity(body.len());
        for expr in body {
            let span = expr.span;
            match expr.inner {
                Expr::LetTuple { fields, default } => {
                    let id = self.fresh_id();
                    let synth_name: &'static str =
                        Box::leak(format!("__dst_{id}").into_boxed_str());
                    let synth_ident =
                        Ident::new(synth_name).with_span(span);

                    // let __dst_<id> = <default>
                    out.push(
                        Expr::Let {
                            ident: synth_ident.clone(),
                            ty: Type::Unknown.with_span(span),
                            default: Some(default),
                        }
                        .with_span(span),
                    );

                    // let <field_i> = __dst_<id>.<i>
                    for (i, fname) in fields.into_iter().enumerate() {
                        let receiver =
                            Expr::Var(synth_name, Type::Unknown).with_span(span);
                        out.push(
                            Expr::Let {
                                ident: fname.clone(),
                                ty: Type::Unknown.with_span(fname.span),
                                default: Some(Box::new(
                                    Expr::TupleIndex {
                                        receiver: Box::new(receiver),
                                        index: i,
                                    }
                                    .with_span(span),
                                )),
                            }
                            .with_span(fname.span),
                        );
                    }
                }
                other => out.push(other.with_span(span)),
            }
        }
        out
    }

    fn fresh_id(&self) -> u32 {
        let id = self.counter.get();
        self.counter.set(id + 1);
        id
    }

    /// Walk the expression and rewrite every `Expr::Path` whose target
    /// resolves into a Var(item_name).  Backend's jump dispatcher only
    /// understands bare-name callees (flat symbol space across the
    /// whole compilation), so collapsing the path here keeps the
    /// pipeline simple.  Unresolvable paths (e.g. `unknown:foo`) pass
    /// through; typeck reports them with a proper diagnostic.
    fn flatten_paths(&self, expr: Spanned<Expr<'src>>) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Path(ref segments) => {
                if let Some((_, item)) =
                    self.registry.resolve_path(self.module_id, segments)
                {
                    // Need a 'src lifetime — leak the item name so the
                    // resulting Var slice lives for the whole compile.
                    let leaked: &'static str =
                        Box::leak(item.to_string().into_boxed_str());
                    Expr::Var(leaked, Type::Unknown)
                } else {
                    expr.inner
                }
            }
            Expr::Let { ident, ty, default } => Expr::Let {
                ident,
                ty,
                default: default.map(|d| Box::new(self.flatten_paths(*d))),
            },
            Expr::Jump { ident, args } => Expr::Jump {
                ident: Box::new(self.flatten_paths(*ident)),
                args: args.into_iter().map(|a| self.flatten_paths(a)).collect(),
            },
            Expr::MethodCall { receiver, method, args } => Expr::MethodCall {
                receiver: Box::new(self.flatten_paths(*receiver)),
                method,
                args: args.into_iter().map(|a| self.flatten_paths(a)).collect(),
            },
            Expr::AtAccess { receiver, field } => Expr::AtAccess {
                receiver: Box::new(self.flatten_paths(*receiver)),
                field,
            },
            Expr::DotAccess { receiver, field } => Expr::DotAccess {
                receiver: Box::new(self.flatten_paths(*receiver)),
                field,
            },
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(self.flatten_paths(*receiver)),
                index,
            },
            Expr::StructInit { base, fields } => Expr::StructInit {
                base: Box::new(self.flatten_paths(*base)),
                fields: fields.into_iter().map(|f| self.flatten_paths(f)).collect(),
            },
            Expr::Tuple(elems) => Expr::Tuple(
                elems.into_iter().map(|e| self.flatten_paths(e)).collect(),
            ),
            Expr::When { cond, branches } => Expr::When {
                cond: Box::new(self.flatten_paths(*cond)),
                branches: branches
                    .into_iter()
                    .map(|(p, body)| {
                        (
                            self.flatten_paths(p),
                            body.into_iter().map(|e| self.flatten_paths(e)).collect(),
                        )
                    })
                    .collect(),
            },
            Expr::Wait { handlers, filter } => Expr::Wait {
                handlers: handlers
                    .into_iter()
                    .map(|h| {
                        let span = h.span;
                        let mut br = h.inner;
                        br.body = br.body
                            .into_iter()
                            .map(|e| self.flatten_paths(e))
                            .collect();
                        br.with_span(span)
                    })
                    .collect(),
                filter: filter.into_iter().map(|f| self.flatten_paths(f)).collect(),
            },
            Expr::Ret(inner) => Expr::Ret(Box::new(self.flatten_paths(*inner))),
            Expr::Pin(inner) => Expr::Pin(Box::new(self.flatten_paths(*inner))),
            Expr::Neg(inner) => Expr::Neg(Box::new(self.flatten_paths(*inner))),
            Expr::Add(l, r) => Expr::Add(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Le(l, r) => Expr::Le(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Ge(l, r) => Expr::Ge(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Eq(l, r) => Expr::Eq(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Lt(l, r) => Expr::Lt(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Gt(l, r) => Expr::Gt(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::BAnd(l, r) => Expr::BAnd(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::BOr(l, r) => Expr::BOr(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::BXor(l, r) => Expr::BXor(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Shl(l, r) => Expr::Shl(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            Expr::Shr(l, r) => Expr::Shr(
                Box::new(self.flatten_paths(*l)),
                Box::new(self.flatten_paths(*r)),
            ),
            other => other,
        };
        inner.with_span(span)
    }

    /// Lift every `Jump`-shaped argument into a fresh `let` binding
    /// before the enclosing expression.  Backend's pure_expr fold is
    /// happy with Var / Num / arith but bails on a Jump in argument
    /// position, so flattening these here means the rest of the
    /// pipeline never has to special-case nested calls.
    ///
    /// Applied per body statement: each lifted call lands as its own
    /// preceding `let __lift_<id> = inner_call`, the original
    /// expression keeps its position, and ordering is preserved.
    fn lift_nested_calls(
        &self,
        body: Vec<Spanned<Expr<'src>>>,
    ) -> Vec<Spanned<Expr<'src>>> {
        let mut out = Vec::with_capacity(body.len());
        for expr in body {
            let lifted = self.lift_in_expr(expr, &mut out);
            out.push(lifted);
        }
        out
    }

    fn lift_in_expr(
        &self,
        expr: Spanned<Expr<'src>>,
        lifts: &mut Vec<Spanned<Expr<'src>>>,
    ) -> Spanned<Expr<'src>> {
        let span = expr.span;
        match expr.inner {
            Expr::Jump { ident, args } => {
                // Recurse into the callee (rare — usually a Var) AND
                // every arg.  An arg that is itself a Jump is hoisted
                // into a fresh let; otherwise it is recursively
                // descended in case it contains nested calls deeper
                // (e.g. `f(g(x) + 1)` lifts `g(x)`).
                let new_callee = Box::new(self.lift_in_expr(*ident, lifts));
                let new_args: Vec<_> = args
                    .into_iter()
                    .map(|a| self.lift_arg(a, lifts))
                    .collect();
                Expr::Jump {
                    ident: new_callee,
                    args: new_args,
                }
                .with_span(span)
            }
            Expr::Let { ident, ty, default } => {
                let default = default.map(|d| Box::new(self.lift_in_expr(*d, lifts)));
                Expr::Let { ident, ty, default }.with_span(span)
            }
            Expr::Tuple(elems) => {
                let new_elems: Vec<_> = elems
                    .into_iter()
                    .map(|e| self.lift_arg(e, lifts))
                    .collect();
                Expr::Tuple(new_elems).with_span(span)
            }
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(self.lift_in_expr(*receiver, lifts)),
                index,
            }
            .with_span(span),
            Expr::Ret(inner) => {
                Expr::Ret(Box::new(self.lift_in_expr(*inner, lifts))).with_span(span)
            }
            Expr::Pin(inner) => {
                Expr::Pin(Box::new(self.lift_in_expr(*inner, lifts))).with_span(span)
            }
            Expr::Neg(inner) => {
                Expr::Neg(Box::new(self.lift_in_expr(*inner, lifts))).with_span(span)
            }
            // when / wait have their own scoped bodies — recursing into
            // them would bleed lifts across arms, which is wrong.  Lifts
            // for those should happen inside the arm's own body when the
            // pipeline re-enters lower_body for that scope.  For now the
            // outer arm cond / filter slot can still contain a call;
            // surface that as-is and let backend handle (or fail) — this
            // matches the pre-lift behaviour.
            other => other.with_span(span),
        }
    }

    /// Lift a single sub-expression that sits in a *value* position
    /// (call arg, tuple element).  If the expr is a `Jump`, push a
    /// `let __lift_<id> = <expr>` into `lifts` and return a `Var`
    /// reference; otherwise descend recursively for deeper nests.
    fn lift_arg(
        &self,
        expr: Spanned<Expr<'src>>,
        lifts: &mut Vec<Spanned<Expr<'src>>>,
    ) -> Spanned<Expr<'src>> {
        let span = expr.span;
        if matches!(expr.inner, Expr::Jump { .. }) {
            let recursed = self.lift_in_expr(expr, lifts);
            let id = self.fresh_id();
            let name: &'static str = Box::leak(format!("__lift_{id}").into_boxed_str());
            let ident = Ident::new(name).with_span(span);
            lifts.push(
                Expr::Let {
                    ident: ident.clone(),
                    ty: Type::Unknown.with_span(span),
                    default: Some(Box::new(recursed)),
                }
                .with_span(span),
            );
            Expr::Var(name, Type::Unknown).with_span(span)
        } else {
            self.lift_in_expr(expr, lifts)
        }
    }

    /// Replace every `Expr::Var(name, _)` whose bare-name resolution
    /// lands on a `Resolution::Const` with the literal Expr that the
    /// const declares.  Recursive across all Expr shapes.
    fn inline_consts(&self, expr: Spanned<Expr<'src>>) -> Spanned<Expr<'src>> {
        use crate::registry::{ConstValue, Resolution};
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Var(name, ref ty) => {
                if let Some(Resolution::Const(c)) =
                    self.registry.resolve_bare_in(self.module_id, name)
                {
                    let ty_named = |s: &'static str| {
                        Type::Named(Ident::new(s).with_span(span))
                    };
                    return match &c.value {
                        ConstValue::Int(v) => {
                            let leaked: &'static str =
                                Box::leak(v.to_string().into_boxed_str());
                            Expr::Num(leaked, ty_named("i64")).with_span(span)
                        }
                        ConstValue::Bool(b) => Expr::Bool(*b).with_span(span),
                        ConstValue::Str(s) => {
                            let leaked: &'static str =
                                Box::leak(s.clone().into_boxed_str());
                            Expr::String(leaked, ty_named("str")).with_span(span)
                        }
                        ConstValue::Atom(s) => {
                            let leaked: &'static str =
                                Box::leak(s.clone().into_boxed_str());
                            Expr::Atom(leaked).with_span(span)
                        }
                    };
                }
                Expr::Var(name, ty.clone())
            }
            Expr::Let { ident, ty, default } => Expr::Let {
                ident,
                ty,
                default: default.map(|d| Box::new(self.inline_consts(*d))),
            },
            Expr::Jump { ident, args } => Expr::Jump {
                ident: Box::new(self.inline_consts(*ident)),
                args: args.into_iter().map(|a| self.inline_consts(a)).collect(),
            },
            Expr::MethodCall { receiver, method, args } => Expr::MethodCall {
                receiver: Box::new(self.inline_consts(*receiver)),
                method,
                args: args.into_iter().map(|a| self.inline_consts(a)).collect(),
            },
            Expr::AtAccess { receiver, field } => Expr::AtAccess {
                receiver: Box::new(self.inline_consts(*receiver)),
                field,
            },
            Expr::DotAccess { receiver, field } => Expr::DotAccess {
                receiver: Box::new(self.inline_consts(*receiver)),
                field,
            },
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(self.inline_consts(*receiver)),
                index,
            },
            Expr::StructInit { base, fields } => Expr::StructInit {
                base: Box::new(self.inline_consts(*base)),
                fields: fields.into_iter().map(|f| self.inline_consts(f)).collect(),
            },
            Expr::Tuple(elems) => Expr::Tuple(
                elems.into_iter().map(|e| self.inline_consts(e)).collect(),
            ),
            Expr::When { cond, branches } => Expr::When {
                cond: Box::new(self.inline_consts(*cond)),
                branches: branches
                    .into_iter()
                    .map(|(p, body)| {
                        (
                            self.inline_consts(p),
                            body.into_iter().map(|e| self.inline_consts(e)).collect(),
                        )
                    })
                    .collect(),
            },
            Expr::Wait { handlers, filter } => Expr::Wait {
                handlers: handlers
                    .into_iter()
                    .map(|h| {
                        let span = h.span;
                        let mut br = h.inner;
                        br.body = br.body
                            .into_iter()
                            .map(|e| self.inline_consts(e))
                            .collect();
                        br.with_span(span)
                    })
                    .collect(),
                filter: filter.into_iter().map(|f| self.inline_consts(f)).collect(),
            },
            Expr::Add(l, r) => Expr::Add(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Le(l, r) => Expr::Le(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Ge(l, r) => Expr::Ge(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Eq(l, r) => Expr::Eq(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Lt(l, r) => Expr::Lt(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Gt(l, r) => Expr::Gt(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::BAnd(l, r) => Expr::BAnd(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::BOr(l, r) => Expr::BOr(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::BXor(l, r) => Expr::BXor(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Shl(l, r) => Expr::Shl(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Shr(l, r) => Expr::Shr(
                Box::new(self.inline_consts(*l)),
                Box::new(self.inline_consts(*r)),
            ),
            Expr::Neg(inner) => Expr::Neg(Box::new(self.inline_consts(*inner))),
            Expr::Ret(inner) => Expr::Ret(Box::new(self.inline_consts(*inner))),
            Expr::Pin(inner) => Expr::Pin(Box::new(self.inline_consts(*inner))),
            other => other,
        };
        inner.with_span(span)
    }

    /// Rewrite `pin <expr>` at the top level of a body sequence into
    /// `let __pin_<id> = <expr>`.  We don't recurse into nested
    /// expressions: `pin` only makes sense as a statement-position
    /// wrapper around a call whose value would otherwise be discarded.
    /// Callers writing `pin` mid-expression (e.g. `1 + pin foo()`) are
    /// silently passed through here and will fail later in typeck.
    fn rewrite_pin(&self, expr: Spanned<Expr<'src>>) -> Spanned<Expr<'src>> {
        let span = expr.span;
        match expr.inner {
            Expr::Pin(inner) => {
                let id = self.counter.get();
                self.counter.set(id + 1);
                let name: &'static str =
                    Box::leak(format!("__pin_{id}").into_boxed_str());
                Expr::Let {
                    ident: Ident::new(name).with_span(span),
                    ty: Type::Unknown.with_span(span),
                    default: Some(Box::new(*inner)),
                }
                .with_span(span)
            }
            other => Spanned { inner: other, span },
        }
    }

    /// Recursively rewrite `Expr::Ret(x)` into `__caller(self, x)`.
    fn rewrite_ret_in_expr(&self, mut expr: Spanned<Expr<'src>>) -> Spanned<Expr<'src>> {
        let span = expr.span;
        expr.inner = match expr.inner {
            Expr::Ret(inner) => {
                let inner = self.rewrite_ret_in_expr(*inner);
                build_caller_send(span, inner)
            }
            Expr::When { cond, branches } => {
                let cond = Box::new(self.rewrite_ret_in_expr(*cond));
                let branches = branches
                    .into_iter()
                    .map(|(pat, body)| {
                        let pat = self.rewrite_ret_in_expr(pat);
                        let body = body
                            .into_iter()
                            .map(|e| self.rewrite_ret_in_expr(e))
                            .collect();
                        (pat, body)
                    })
                    .collect();
                Expr::When { cond, branches }
            }
            Expr::Wait { handlers, filter } => {
                let filter = filter
                    .into_iter()
                    .map(|f| self.rewrite_ret_in_expr(f))
                    .collect();
                let handlers = handlers
                    .into_iter()
                    .map(|h| {
                        let span = h.span;
                        let body = h
                            .inner
                            .body
                            .into_iter()
                            .map(|e| self.rewrite_ret_in_expr(e))
                            .collect();
                        Branch::new(h.inner.label, h.inner.patterns, h.inner.ret_ty, body)
                            .with_span(span)
                    })
                    .collect();
                Expr::Wait { handlers, filter }
            }
            Expr::Let { ident, ty, default } => Expr::Let {
                ident,
                ty,
                default: default.map(|d| Box::new(self.rewrite_ret_in_expr(*d))),
            },
            Expr::Jump { ident, args } => Expr::Jump {
                ident: Box::new(self.rewrite_ret_in_expr(*ident)),
                args: args
                    .into_iter()
                    .map(|a| self.rewrite_ret_in_expr(a))
                    .collect(),
            },
            Expr::Add(l, r) => Expr::Add(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Eq(l, r) => Expr::Eq(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Le(l, r) => Expr::Le(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Ge(l, r) => Expr::Ge(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Lt(l, r) => Expr::Lt(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Gt(l, r) => Expr::Gt(
                Box::new(self.rewrite_ret_in_expr(*l)),
                Box::new(self.rewrite_ret_in_expr(*r)),
            ),
            Expr::Neg(inner) => Expr::Neg(Box::new(self.rewrite_ret_in_expr(*inner))),
            Expr::Pin(inner) => Expr::Pin(Box::new(self.rewrite_ret_in_expr(*inner))),
            other => other,
        };
        expr
    }

    fn is_let_with_ret_target(&self, expr: &Spanned<Expr<'src>>) -> bool {
        let Expr::Let { ty, default, .. } = &expr.inner else {
            return false;
        };
        let Some(default) = default else {
            return false;
        };
        // `let r pid = M(args)` — explicit pid annotation tells us the
        // user wants the spawned actor's pid, not the synchronous return.
        // Skip desugaring.
        if matches!(&ty.inner, Type::Named(t) if t.inner.0 == "pid") {
            return false;
        }
        self.ret_target(&default.inner).is_some()
    }

    /// Expand `let r = M(args); ...rest` into the equivalent two-step
    /// pattern `let __ret_N_pid = M(self ...args)` followed by
    /// `wait __ret_N_pid { sender pid r <ret_ty> -> { ...rest } }`.
    fn expand_let_with_ret(
        &self,
        let_expr: Spanned<Expr<'src>>,
        rest: Vec<Spanned<Expr<'src>>>,
        is_ret_branch: bool,
    ) -> Vec<Spanned<Expr<'src>>> {
        let span = let_expr.span;
        let Expr::Let { ident, default, .. } = let_expr.inner else {
            unreachable!("checked by is_let_with_ret_target");
        };
        let default = *default.expect("checked");
        let (_, ret_ty_str) = self
            .ret_target(&default.inner)
            .expect("checked by is_let_with_ret_target");

        // Allocate a fresh local name for the spawned pid and leak it so
        // it lives as a `&'src str` (matching what AST `Expr::Var`
        // borrows).  Compilation is short-lived; the leak is bounded.
        let id = self.counter.get();
        self.counter.set(id + 1);
        let pid_name: &'static str = Box::leak(format!("__ret_{id}_pid").into_boxed_str());
        let sender_name: &'static str = Box::leak(format!("__ret_{id}_sender").into_boxed_str());

        // If the user wrote `let _ = M(args)`, the value pattern would be
        // a wildcard `_`.  wait_expr's dequeue counts only non-wildcard,
        // non-guard patterns when sizing the message read, so a wildcard
        // value pattern leaves the value half of the (sender, value)
        // message stranded in the mailbox — the next wait then sees it
        // as a stray sender pid and drops or mismatches.  Substitute a
        // fresh synthetic name so the slot is consumed even though the
        // bound value is unused.
        let value_ident = if ident.inner.0 == "_" {
            let synth: &'static str =
                Box::leak(format!("__ret_{id}_value").into_boxed_str());
            crate::api::ast::Ident::new(synth).with_span(ident.span)
        } else {
            ident
        };

        // Step 1: `let __ret_N_pid pid = M(self ...args)`.  The original
        // call's args get `self` prepended — the ret-machine reads it as
        // its `__caller` first parameter.
        let new_call = self.prepend_self_arg(default, span);
        let pid_let = Expr::Let {
            ident: Ident::new(pid_name).with_span(span),
            ty: Type::Named(Ident::new("pid").with_span(span)).with_span(span),
            default: Some(Box::new(new_call)),
        }
        .with_span(span);

        // Step 2: `wait __ret_N_pid { __ret_N_sender pid <user_ident>
        // <ret_ty> -> { ...rest } }`.  Filter on the captured pid
        // ensures only the real reply matches.  The handler binds
        // sender (anonymous) + the user's let target.
        let rest_lowered = self.lower_body(rest, is_ret_branch);
        let ret_ty_node = type_from_string(&ret_ty_str, span);
        let sender_pat = Pattern::new(
            Ident::new(sender_name).with_span(span),
            Type::Named(Ident::new("pid").with_span(span)).with_span(span),
        );
        let value_pat = Pattern::new(value_ident, ret_ty_node.with_span(span));
        let handler = Branch::new(
            None,
            vec![sender_pat.with_span(span), value_pat.with_span(span)],
            None,
            rest_lowered,
        )
        .with_span(span);
        let filter = vec![
            Expr::Var(pid_name, Type::Named(Ident::new("pid").with_span(span))).with_span(span),
        ];
        let wait_expr = Expr::Wait {
            handlers: vec![handler],
            filter,
        }
        .with_span(span);

        vec![pid_let, wait_expr]
    }

    /// Bare `M(args)` (no let) calling a ret-machine: prepend the null
    /// pid sentinel `0` so the callee's `__caller(self X)` send is a
    /// no-op at runtime.
    fn try_rewrite_bare_ret_call(
        &self,
        expr: &Spanned<Expr<'src>>,
    ) -> Option<Spanned<Expr<'src>>> {
        let Expr::Jump { ident, args } = &expr.inner else {
            return None;
        };
        let _ = self.ret_target(&expr.inner)?;
        let span = expr.span;
        let null_pid = synth_null_pid(span);
        let mut new_args = Vec::with_capacity(args.len() + 1);
        new_args.push(null_pid);
        new_args.extend(args.iter().cloned());
        Some(
            Expr::Jump {
                ident: ident.clone(),
                args: new_args,
            }
            .with_span(span),
        )
    }

    fn prepend_self_arg(
        &self,
        call: Spanned<Expr<'src>>,
        span: SimpleSpan,
    ) -> Spanned<Expr<'src>> {
        let Expr::Jump { ident, args } = call.inner else {
            return call;
        };
        let mut new_args = Vec::with_capacity(args.len() + 1);
        new_args.push(synth_self(span));
        new_args.extend(args);
        Expr::Jump {
            ident,
            args: new_args,
        }
        .with_span(span)
    }

    /// If `expr` is a `Jump` whose target resolves to a ret-machine,
    /// return the machine's resolved name plus its return type.  Cross-
    /// module imports are followed via the registry.
    fn ret_target(&self, expr: &Expr<'src>) -> Option<(String, String)> {
        let Expr::Jump { ident, .. } = expr else {
            return None;
        };
        let name = match &ident.inner {
            Expr::Var(name, _) => name.to_string(),
            Expr::Path(segments) if segments.len() >= 2 => {
                // Inline `core:io:writer(...)` form, possibly via a
                // namespace alias (`io:println` after `+std.io`).
                // resolve_path expands the head segment when it's a
                // namespace alias in the current module.
                let (target_id, item) = self
                    .registry
                    .resolve_path(self.module_id, segments)?;
                let item = item.to_string();
                let target_path =
                    self.registry.module(target_id).path.clone();
                let key = RetKey {
                    module: target_path,
                    name: item.clone(),
                };
                return self.ret_machines.get(&key).cloned().map(|t| (item, t));
            }
            _ => return None,
        };

        // Local module first.
        let scope = self.registry.module(self.module_id);
        let local_key = RetKey {
            module: scope.path.clone(),
            name: name.clone(),
        };
        if let Some(ret_ty) = self.ret_machines.get(&local_key) {
            return Some((name, ret_ty.clone()));
        }
        // Imported alias?  Resolve via the registry's import binding.
        if let Some(binding) = scope.imports.get(&name) {
            if let Some(ret_ty) = self.resolve_import_ret(binding) {
                return Some((name, ret_ty));
            }
        }
        // Last-resort global scan — matches the resolve_bare_in
        // fallback used after flatten_paths collapses Path callees
        // (`io:println` → bare `println`) into the flat symbol space.
        for (key, ret_ty) in self.ret_machines.iter() {
            if key.name == name {
                return Some((name, ret_ty.clone()));
            }
        }
        None
    }

    fn resolve_import_ret(&self, binding: &ImportBinding) -> Option<String> {
        // Namespace imports don't carry a single item name — there's
        // nothing to resolve as a ret-machine here.
        let item_name = binding.target_item.as_ref()?;
        let target_module = self
            .registry
            .modules
            .get(binding.target_module.0 as usize)?
            .path
            .clone();
        let key = RetKey {
            module: target_module,
            name: item_name.clone(),
        };
        self.ret_machines.get(&key).cloned()
    }
}

// ── synthetic AST builders ─────────────────────────────────────────────────

fn synth_caller_pattern<'src>(span: SimpleSpan) -> Spanned<Pattern<'src>> {
    Pattern::new(
        Ident::new("__caller").with_span(span),
        Type::Named(Ident::new("pid").with_span(span)).with_span(span),
    )
    .with_span(span)
}

fn synth_null_pid<'src>(span: SimpleSpan) -> Spanned<Expr<'src>> {
    Expr::Num("0", Type::Named(Ident::new("pid").with_span(span))).with_span(span)
}

fn synth_self<'src>(span: SimpleSpan) -> Spanned<Expr<'src>> {
    Expr::Var("self", Type::Named(Ident::new("pid").with_span(span))).with_span(span)
}

fn build_caller_send<'src>(span: SimpleSpan, value: Spanned<Expr<'src>>) -> Expr<'src> {
    Expr::Jump {
        ident: Box::new(
            Expr::Var(
                "__caller",
                Type::Named(Ident::new("pid").with_span(span)),
            )
            .with_span(span),
        ),
        args: vec![synth_self(span), value],
    }
}

fn wrap_in_caller_send<'src>(expr: &mut Spanned<Expr<'src>>) {
    let span = expr.span;
    let placeholder = Expr::Unit.with_span(span);
    let original = std::mem::replace(expr, placeholder);
    let wrapped = build_caller_send(span, original);
    *expr = Spanned {
        inner: wrapped,
        span,
    };
}

/// True when the expression is guaranteed to terminate via a `__caller`
/// send.  Used by the tail wrapping in `lower_body` — an expression that
/// already returns (directly or through every branch / handler) doesn't
/// need an extra implicit-return wrap.
///
/// Recurses through `when` and `wait` so a body whose tail is e.g.
/// `when 0 <= n { true -> { ret a }  _ -> { ret b } }` is recognised as
/// fully-returning even though the top-level expression is `When`, not
/// `Jump { __caller }`.
fn is_caller_send(expr: &Expr<'_>) -> bool {
    match expr {
        Expr::Jump { ident, .. } => {
            matches!(&ident.inner, Expr::Var("__caller", _))
        }
        Expr::When { branches, .. } => {
            // Every arm must terminate in a __caller send.  We don't
            // know whether `when` has a "no-arm-matches" fall-through
            // path at runtime; conservatively treat that as a missing
            // return, so we still emit a tail wrap if any user-level
            // arm's body could fall through.  In practice ret-machines
            // are written so every arm returns, and the conservative
            // wrap is harmless when one doesn't (the unreachable wrap
            // never fires).
            !branches.is_empty()
                && branches.iter().all(|(_, body)| {
                    body.last().map(|e| is_caller_send(&e.inner)).unwrap_or(false)
                })
        }
        Expr::Wait { handlers, .. } => {
            !handlers.is_empty()
                && handlers.iter().all(|h| {
                    h.inner
                        .body
                        .last()
                        .map(|e| is_caller_send(&e.inner))
                        .unwrap_or(false)
                })
        }
        _ => false,
    }
}

fn type_from_string<'src>(s: &str, span: SimpleSpan) -> Type<'src> {
    let static_name: &'static str = match s {
        "i64" => "i64",
        "str" => "str",
        "bool" => "bool",
        "pid" => "pid",
        _ => return Type::Unknown,
    };
    Type::Named(Ident::new(static_name).with_span(span))
}
