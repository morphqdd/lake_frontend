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

    // Detect ret-machines whose bodies are statically pure: no spawn /
    // wait / self / send / I/O.  Used by #78 to bypass the spawn+wait
    // mailbox roundtrip for these calls.
    let pure_ret_machines = collect_pure_ret_machines(&program, &ret_machines);
    // Restrict to "simple" pure bodies (single top-level `ret`, no nested).
    // Multi-arm `when` bodies (`k`, `h_init`) are excluded for phase 2a;
    // they need a "valued when" lowering to be inlineable.
    let pure_bodies = if std::env::var("LAKE_NO_PURE_INLINE").is_ok() {
        HashMap::new()
    } else {
        collect_pure_bodies(&program, &pure_ret_machines)
    };
    if std::env::var("LAKE_DUMP_PURE").is_ok() {
        let mut sorted: Vec<_> = pure_ret_machines.iter().collect();
        sorted.sort();
        eprintln!("=== pure ret-machines ===");
        for k in sorted {
            let inlineable = if pure_bodies.contains_key(k) {
                " [inlineable]"
            } else {
                ""
            };
            eprintln!("  {}::{}{}", k.module.display(), k.name, inlineable);
        }
    }

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
                pure_bodies: &pure_bodies,
                counter: counter.clone(),
            };
            cx.lower_module(module)
        })
        .collect();

    let result = ParsedProgram { modules };
    if std::env::var("LAKE_DUMP_LOWERED").is_ok() {
        for m in &result.modules {
            eprintln!("=== module {:?} ===", m.module_path);
            for item in &m.ast {
                eprintln!("{:#?}", item.inner);
            }
        }
    }
    result
}

// ── ret-machine table ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

/// rt-funcs that don't yield the scheduler and don't perform I/O.  Memory
/// ops, allocator, mmap.  Calling these from a pure body keeps the body
/// pure (no scheduler interaction, no observable side effect other than
/// memory writes that the call site already authorised).
fn pure_rt_funcs() -> std::collections::HashSet<&'static str> {
    [
        "rt_allocate",
        "rt_allocate_raw",
        "rt_die_actor",
        "rt_free",
        "rt_store",
        "rt_load_u8",
        "rt_load_u16",
        "rt_load_u32",
        "rt_load_u64",
        "rt_copy_bytes",
        "rt_mmap",
        "rt_munmap",
    ]
    .into_iter()
    .collect()
}

/// Detect ret-machines whose bodies are pure w.r.t. concurrency:
/// no `wait`, no `pin`, no `self()`, no spawn-style call, no I/O.
/// Calls to other pure ret-machines are allowed (transitive).  Phase 1
/// of #78 — non-recursive only; self-recursive purity is a follow-on.
fn collect_pure_ret_machines<'src>(
    program: &ParsedProgram<'src>,
    ret_machines: &HashMap<RetKey, String>,
) -> std::collections::HashSet<RetKey> {
    use std::collections::HashSet;
    let pure_rt = pure_rt_funcs();

    // Index machines by RetKey for fixpoint lookups.
    let mut bodies: HashMap<RetKey, Vec<Spanned<Expr<'src>>>> = HashMap::new();
    for module in &program.modules {
        for item in &module.ast {
            if let Item::Machine(m) = &item.inner {
                let key = RetKey {
                    module: module.module_path.clone(),
                    name: m.inner.ident.inner.0.to_string(),
                };
                if !ret_machines.contains_key(&key) {
                    continue;
                }
                // Concatenate all branch bodies — purity is a per-machine
                // property; any branch with a yield kills it for all.
                let mut body = Vec::new();
                for mi in &m.inner.items {
                    if let MachineItem::Branch(b) = &mi.inner {
                        body.extend(b.body.iter().cloned());
                    }
                }
                bodies.insert(key, body);
            }
        }
    }

    // Initially assume every ret-machine is pure; then strike out the ones
    // with disallowed shapes.  Fixpoint converges in O(depth) iterations.
    let mut candidates: HashSet<RetKey> = bodies.keys().cloned().collect();
    loop {
        let snapshot = candidates.clone();
        let before = candidates.len();
        candidates.retain(|key| {
            let body = bodies.get(key).expect("indexed above");
            body.iter()
                .all(|e| expr_is_pure(&e.inner, &snapshot, &pure_rt, ret_machines))
        });
        if candidates.len() == before {
            break;
        }
    }
    candidates
}

fn expr_is_pure<'src>(
    expr: &Expr<'src>,
    pure_machines: &std::collections::HashSet<RetKey>,
    pure_rt: &std::collections::HashSet<&'static str>,
    ret_machines: &HashMap<RetKey, String>,
) -> bool {
    use Expr::*;
    match expr {
        Num(_, _) | String(_, _) | Bool(_) | Unit | Atom(_) | Var(_, _) | Path(_) => true,
        Let { default, .. } => default
            .as_ref()
            .map(|d| expr_is_pure(&d.inner, pure_machines, pure_rt, ret_machines))
            .unwrap_or(true),
        Ret(inner) => expr_is_pure(&inner.inner, pure_machines, pure_rt, ret_machines),
        // `pin` is sync sugar for a ret-call — same shape as a normal Jump
        // to a ret-machine; treat its inner the same way.
        Pin(_) => false,
        // `wait` and explicit ret-call shapes are scheduler interactions.
        Wait { .. } => false,
        Jump { ident, args } => {
            // Callee is a bare Var or a Path; reject anything else as
            // "can't tell, assume impure".
            let callee_pure = match &ident.inner {
                Var(name, _) => {
                    if *name == "self" {
                        false
                    } else if pure_rt.contains(name) {
                        true
                    } else {
                        // Look up by name across all modules — name-only
                        // collision is rare and phase-1-acceptable.
                        ret_machines.keys().any(|k| k.name == *name)
                            && pure_machines.iter().any(|k| k.name == *name)
                    }
                }
                Path(segs) => {
                    let leaf = segs.last().map(|s| s.inner.0).unwrap_or("");
                    pure_machines.iter().any(|k| k.name == leaf)
                }
                _ => false,
            };
            if !callee_pure {
                return false;
            }
            args.iter()
                .all(|a| expr_is_pure(&a.inner, pure_machines, pure_rt, ret_machines))
        }
        MethodCall { .. } | AtAccess { .. } | DotAccess { .. } | StructInit { .. } => false,
        Tuple(elems) => elems
            .iter()
            .all(|e| expr_is_pure(&e.inner, pure_machines, pure_rt, ret_machines)),
        TupleIndex { receiver, .. } => {
            expr_is_pure(&receiver.inner, pure_machines, pure_rt, ret_machines)
        }
        Index { receiver, index } => {
            expr_is_pure(&receiver.inner, pure_machines, pure_rt, ret_machines)
                && expr_is_pure(&index.inner, pure_machines, pure_rt, ret_machines)
        }
        LetTuple { default, .. } => {
            expr_is_pure(&default.inner, pure_machines, pure_rt, ret_machines)
        }
        When { cond, branches } => {
            expr_is_pure(&cond.inner, pure_machines, pure_rt, ret_machines)
                && branches.iter().all(|(c, body)| {
                    expr_is_pure(&c.inner, pure_machines, pure_rt, ret_machines)
                        && body.iter().all(|e| {
                            expr_is_pure(&e.inner, pure_machines, pure_rt, ret_machines)
                        })
                })
        }
        Mul(l, r) | Div(l, r) | Add(l, r) | Sub(l, r) | Le(l, r) | Ge(l, r) | Eq(l, r)
        | Lt(l, r) | Gt(l, r) | BAnd(l, r) | BOr(l, r) | BXor(l, r) | Shl(l, r) | Shr(l, r) => {
            expr_is_pure(&l.inner, pure_machines, pure_rt, ret_machines)
                && expr_is_pure(&r.inner, pure_machines, pure_rt, ret_machines)
        }
        Neg(inner) => expr_is_pure(&inner.inner, pure_machines, pure_rt, ret_machines),
    }
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

/// Body template for an inlineable pure ret-machine.  Captures the
/// pre-lowering AST so that callers can substitute parameter names and
/// splice the body into their own scope.  Only "simple" pure bodies
/// qualify — a single `ret <expr>` at top level, no nested `ret`.  This
/// excludes `k(i)` / `h_init(i)` whose multi-arm `when` bodies need a
/// later "valued when" lowering (see #78 phase 2b).
#[derive(Debug, Clone)]
struct PureBodyTemplate<'src> {
    params: Vec<Spanned<Pattern<'src>>>,
    body: Vec<Spanned<Expr<'src>>>,
}

/// True when the body has exactly one `Expr::Ret` at the top level
/// (the last statement) and none nested elsewhere.  Side-effecting
/// statements before the final `ret` are allowed (`set(...)`, etc.).
fn is_simple_pure_body<'src>(body: &[Spanned<Expr<'src>>]) -> bool {
    if body.is_empty() {
        return false;
    }
    let last = body.last().unwrap();
    if !matches!(last.inner, Expr::Ret(_)) {
        return false;
    }
    // Final Ret's inner expression must contain no Ret.
    if let Expr::Ret(inner) = &last.inner {
        if expr_contains_ret(&inner.inner) {
            return false;
        }
    }
    // Prior statements must not contain Ret either.
    for stmt in &body[..body.len() - 1] {
        if expr_contains_ret(&stmt.inner) {
            return false;
        }
    }
    true
}

fn expr_contains_ret<'src>(expr: &Expr<'src>) -> bool {
    use Expr::*;
    match expr {
        Ret(_) => true,
        Let { default, .. } => default
            .as_ref()
            .map(|d| expr_contains_ret(&d.inner))
            .unwrap_or(false),
        Pin(inner) | Neg(inner) => expr_contains_ret(&inner.inner),
        Jump { ident, args } => {
            expr_contains_ret(&ident.inner)
                || args.iter().any(|a| expr_contains_ret(&a.inner))
        }
        MethodCall { receiver, args, .. } => {
            expr_contains_ret(&receiver.inner)
                || args.iter().any(|a| expr_contains_ret(&a.inner))
        }
        AtAccess { receiver, .. } | DotAccess { receiver, .. } => {
            expr_contains_ret(&receiver.inner)
        }
        StructInit { base, fields } => {
            expr_contains_ret(&base.inner)
                || fields.iter().any(|f| expr_contains_ret(&f.inner))
        }
        Tuple(elems) => elems.iter().any(|e| expr_contains_ret(&e.inner)),
        TupleIndex { receiver, .. } => expr_contains_ret(&receiver.inner),
        Index { receiver, index } => {
            expr_contains_ret(&receiver.inner) || expr_contains_ret(&index.inner)
        }
        LetTuple { default, .. } => expr_contains_ret(&default.inner),
        When { cond, branches } => {
            expr_contains_ret(&cond.inner)
                || branches
                    .iter()
                    .any(|(c, b)| expr_contains_ret(&c.inner) || b.iter().any(|e| expr_contains_ret(&e.inner)))
        }
        Wait { handlers, filter } => {
            filter.iter().any(|f| expr_contains_ret(&f.inner))
                || handlers
                    .iter()
                    .any(|h| h.inner.body.iter().any(|e| expr_contains_ret(&e.inner)))
        }
        Add(l, r) | Sub(l, r) | Mul(l, r) | Div(l, r) | Eq(l, r) | Le(l, r)
        | Ge(l, r) | Lt(l, r) | Gt(l, r) | BAnd(l, r) | BOr(l, r) | BXor(l, r)
        | Shl(l, r) | Shr(l, r) => {
            expr_contains_ret(&l.inner) || expr_contains_ret(&r.inner)
        }
        Num(_, _) | String(_, _) | Bool(_) | Unit | Atom(_) | Var(_, _) | Path(_) => false,
    }
}

/// Index simple-pure ret-machines by their (module, name) key, storing
/// the original branch parameters + body.  Only machines that pass both
/// `collect_pure_ret_machines` AND `is_simple_pure_body` are included.
fn collect_pure_bodies<'src>(
    program: &ParsedProgram<'src>,
    pure_set: &std::collections::HashSet<RetKey>,
) -> HashMap<RetKey, PureBodyTemplate<'src>> {
    let mut out = HashMap::new();
    for module in &program.modules {
        for item in &module.ast {
            if let Item::Machine(m) = &item.inner {
                let key = RetKey {
                    module: module.module_path.clone(),
                    name: m.inner.ident.inner.0.to_string(),
                };
                if !pure_set.contains(&key) {
                    continue;
                }
                // Expect a single branch (phase 2a restriction).  Bail if not.
                let mut branches = m.inner.items.iter().filter_map(|mi| {
                    if let MachineItem::Branch(b) = &mi.inner {
                        Some(b)
                    } else {
                        None
                    }
                });
                let Some(branch) = branches.next() else { continue };
                if branches.next().is_some() {
                    // Multi-branch — skip for phase 2a.
                    continue;
                }
                if !is_simple_pure_body(&branch.body) {
                    continue;
                }
                out.insert(
                    key,
                    PureBodyTemplate {
                        params: branch.patterns.clone(),
                        body: branch.body.clone(),
                    },
                );
            }
        }
    }
    out
}

/// Substitute identifier names in an expression tree according to `map`.
/// Used by the inline-pure phase to rename a pure body's parameters and
/// internal locals to fresh names per inline site.  Var references and
/// Let bindings get the substitution applied; Jump callees do NOT (they
/// reference machine names, not local variables).
fn substitute_in_expr<'src>(
    expr: Spanned<Expr<'src>>,
    map: &HashMap<String, &'static str>,
) -> Spanned<Expr<'src>> {
    let span = expr.span;
    use Expr::*;
    let new_inner = match expr.inner {
        Var(name, ty) => {
            if let Some(fresh) = map.get(name) {
                Var(*fresh, ty)
            } else {
                Var(name, ty)
            }
        }
        Let { ident, ty, default } => {
            let ident_str: std::string::String = ident.inner.0.to_string();
            let new_ident = if let Some(fresh) = map.get(&ident_str) {
                Ident::new(*fresh).with_span(ident.span)
            } else {
                ident
            };
            Let {
                ident: new_ident,
                ty,
                default: default.map(|d| Box::new(substitute_in_expr(*d, map))),
            }
        }
        Ret(inner) => Ret(Box::new(substitute_in_expr(*inner, map))),
        Pin(inner) => Pin(Box::new(substitute_in_expr(*inner, map))),
        Jump { ident, args } => Jump {
            // Don't substitute the callee's name — it's a machine reference.
            // But DO walk it in case it's a Path with its own structure.
            ident,
            args: args
                .into_iter()
                .map(|a| substitute_in_expr(a, map))
                .collect(),
        },
        When { cond, branches } => When {
            cond: Box::new(substitute_in_expr(*cond, map)),
            branches: branches
                .into_iter()
                .map(|(pat, body)| {
                    (
                        substitute_in_expr(pat, map),
                        body.into_iter()
                            .map(|e| substitute_in_expr(e, map))
                            .collect(),
                    )
                })
                .collect(),
        },
        Wait { handlers, filter } => Wait {
            handlers,
            filter: filter
                .into_iter()
                .map(|f| substitute_in_expr(f, map))
                .collect(),
        },
        Tuple(elems) => Tuple(
            elems
                .into_iter()
                .map(|e| substitute_in_expr(e, map))
                .collect(),
        ),
        TupleIndex { receiver, index } => TupleIndex {
            receiver: Box::new(substitute_in_expr(*receiver, map)),
            index,
        },
        Index { receiver, index } => Index {
            receiver: Box::new(substitute_in_expr(*receiver, map)),
            index: Box::new(substitute_in_expr(*index, map)),
        },
        LetTuple { fields, default } => LetTuple {
            fields,
            default: Box::new(substitute_in_expr(*default, map)),
        },
        MethodCall {
            receiver,
            method,
            args,
        } => MethodCall {
            receiver: Box::new(substitute_in_expr(*receiver, map)),
            method,
            args: args
                .into_iter()
                .map(|a| substitute_in_expr(a, map))
                .collect(),
        },
        AtAccess { receiver, field } => AtAccess {
            receiver: Box::new(substitute_in_expr(*receiver, map)),
            field,
        },
        DotAccess { receiver, field } => DotAccess {
            receiver: Box::new(substitute_in_expr(*receiver, map)),
            field,
        },
        StructInit { base, fields } => StructInit {
            base: Box::new(substitute_in_expr(*base, map)),
            fields: fields
                .into_iter()
                .map(|f| substitute_in_expr(f, map))
                .collect(),
        },
        Add(l, r) => Add(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Sub(l, r) => Sub(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Mul(l, r) => Mul(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Div(l, r) => Div(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Neg(inner) => Neg(Box::new(substitute_in_expr(*inner, map))),
        Eq(l, r) => Eq(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Le(l, r) => Le(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Ge(l, r) => Ge(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Lt(l, r) => Lt(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Gt(l, r) => Gt(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        BAnd(l, r) => BAnd(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        BOr(l, r) => BOr(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        BXor(l, r) => BXor(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Shl(l, r) => Shl(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        Shr(l, r) => Shr(
            Box::new(substitute_in_expr(*l, map)),
            Box::new(substitute_in_expr(*r, map)),
        ),
        other => other,
    };
    Spanned {
        inner: new_inner,
        span,
    }
}

/// Walk a body collecting names introduced by `let` so they can be
/// added to the substitution map (with fresh `__inl_<id>_<name>`
/// renames).  Avoids accidental shadowing when a pure body's internal
/// local collides with a caller's local.
fn collect_local_let_names<'src>(
    body: &[Spanned<Expr<'src>>],
    map: &mut HashMap<String, &'static str>,
    id: u32,
) {
    for stmt in body {
        collect_let_names_in_expr(&stmt.inner, map, id);
    }
}

fn collect_let_names_in_expr<'src>(
    expr: &Expr<'src>,
    map: &mut HashMap<String, &'static str>,
    id: u32,
) {
    use Expr::*;
    match expr {
        Let { ident, default, .. } => {
            let name = ident.inner.0.to_string();
            if !map.contains_key(&name) {
                let fresh: &'static str =
                    Box::leak(format!("__inl_{}_{}", id, ident.inner.0).into_boxed_str());
                map.insert(name, fresh);
            }
            if let Some(d) = default {
                collect_let_names_in_expr(&d.inner, map, id);
            }
        }
        Ret(inner) | Pin(inner) | Neg(inner) => {
            collect_let_names_in_expr(&inner.inner, map, id);
        }
        Jump { args, .. } => {
            for a in args {
                collect_let_names_in_expr(&a.inner, map, id);
            }
        }
        When { cond, branches } => {
            collect_let_names_in_expr(&cond.inner, map, id);
            for (_, body) in branches {
                collect_local_let_names(body, map, id);
            }
        }
        Wait { handlers, .. } => {
            for h in handlers {
                collect_local_let_names(&h.inner.body, map, id);
            }
        }
        Add(l, r) | Sub(l, r) | Mul(l, r) | Div(l, r) | Eq(l, r) | Le(l, r) | Ge(l, r)
        | Lt(l, r) | Gt(l, r) | BAnd(l, r) | BOr(l, r) | BXor(l, r) | Shl(l, r)
        | Shr(l, r) => {
            collect_let_names_in_expr(&l.inner, map, id);
            collect_let_names_in_expr(&r.inner, map, id);
        }
        Tuple(elems) => {
            for e in elems {
                collect_let_names_in_expr(&e.inner, map, id);
            }
        }
        TupleIndex { receiver, .. } => collect_let_names_in_expr(&receiver.inner, map, id),
        Index { receiver, index } => {
            collect_let_names_in_expr(&receiver.inner, map, id);
            collect_let_names_in_expr(&index.inner, map, id);
        }
        _ => {}
    }
}

/// Given a body whose last statement is `Expr::Ret(e)` (asserted by
/// `is_simple_pure_body`), return (prior statements, the inner `e`).
fn split_final_ret<'src>(
    mut body: Vec<Spanned<Expr<'src>>>,
) -> (Vec<Spanned<Expr<'src>>>, Spanned<Expr<'src>>) {
    let last = body.pop().expect("body is non-empty for pure machine");
    let Expr::Ret(inner) = last.inner else {
        panic!("split_final_ret: last statement is not Ret (is_simple_pure_body should guarantee)");
    };
    (body, *inner)
}

// ── per-module context ─────────────────────────────────────────────────────

struct LowerCx<'a, 'src> {
    registry: &'a ProgramRegistry<'src>,
    module_id: ModuleId,
    ret_machines: &'a HashMap<RetKey, String>,
    /// Inline-eligible pure ret-machines indexed by their RetKey.  Calls
    /// to these get expanded at the call site by `inline_pure_calls`
    /// instead of going through spawn+wait.
    pure_bodies: &'a HashMap<RetKey, PureBodyTemplate<'src>>,
    counter: Rc<Cell<u32>>,
}

impl<'a, 'src> LowerCx<'a, 'src> {
    fn lower_module(&self, module: ParsedModule<'src>) -> ParsedModule<'src> {
        let module_path = module.module_path.clone();
        let ast = module
            .ast
            .into_iter()
            .filter_map(|item| {
                let span = item.span;
                match item.inner {
                    Item::Machine(m) => {
                        let machine_span = m.span;
                        let name = m.inner.ident.inner.0.to_string();
                        let key = RetKey {
                            module: module_path.clone(),
                            name,
                        };
                        // Pure machines are fully inlined at their call
                        // sites by `inline_pure_calls` — drop them from
                        // the AST so the backend never sees their raw
                        // `Expr::Ret(e)` bodies.  If a pure machine is
                        // never called, it disappears with no codegen,
                        // saving binary size.
                        if self.pure_bodies.contains_key(&key) {
                            None
                        } else {
                            Some(
                                Item::Machine(
                                    self.lower_machine(m.inner).with_span(machine_span),
                                )
                                .with_span(span),
                            )
                        }
                    }
                    other => Some(other.with_span(span)),
                }
            })
            .collect();

        ParsedModule {
            module_path,
            source_path: module.source_path,
            ast,
        }
    }

    /// Look up an inlineable pure ret-machine by source-visible name.
    /// Resolves both unqualified calls (same module) and module-imported
    /// callees.  Phase-1 conservative match: by bare name only; in
    /// practice pure machines in stdlib have unique names.
    fn lookup_pure(&self, name: &str) -> Option<&'a PureBodyTemplate<'src>> {
        self.pure_bodies
            .iter()
            .find(|(k, _)| k.name == name)
            .map(|(_, v)| v)
    }

    /// Run the inline-pure-calls phase recursively on a body until no
    /// more pure calls remain.  Each iteration:
    ///   1. Walks each statement.
    ///   2. For `let r = pure_f(args)` (or bare `pure_f(args)` wrapped
    ///      by Phase 1.c), expands to arg-binding lets + substituted
    ///      body + final-`ret` rewritten to bind `r`.
    ///   3. Substituted body may contain calls to other pure machines
    ///      — recursive call inlines those too.
    fn inline_pure_calls(&self, body: Vec<Spanned<Expr<'src>>>) -> Vec<Spanned<Expr<'src>>> {
        if self.pure_bodies.is_empty() {
            return body;
        }
        // Lift any nested pure (or impure) ret-machine calls into
        // separate lets first, so each pure call is the top-level RHS
        // of some `let r = ...`.  Without this, `let s = big_s0(x)`
        // expands to a body containing `BXor(rotr32(x 2), rotr32(x
        // 13), rotr32(x 22))` where the rotr32 calls are in arg
        // position — un-inlineable until lifted.
        let body = self.lift_nested_calls(body);

        let mut out = Vec::with_capacity(body.len());
        for stmt in body {
            if let Some(expanded) = self.try_inline_pure_let(&stmt) {
                // Recurse: the substituted body may itself contain
                // pure calls.  Recursion also re-lifts after the
                // substitution introduces fresh nested calls.
                let recursed = self.inline_pure_calls(expanded);
                out.extend(recursed);
            } else {
                out.push(stmt);
            }
        }
        out
    }

    /// Attempt to expand a `let r = pure_f(args)` statement.  Returns
    /// `None` if the statement is not a let-bound pure call.
    fn try_inline_pure_let(
        &self,
        stmt: &Spanned<Expr<'src>>,
    ) -> Option<Vec<Spanned<Expr<'src>>>> {
        let Expr::Let {
            ident: user_ident,
            ty: user_ty,
            default: Some(rhs),
        } = &stmt.inner
        else {
            return None;
        };
        let Expr::Jump { ident: callee, args } = &rhs.inner else {
            return None;
        };
        let callee_name = match &callee.inner {
            Expr::Var(name, _) => *name,
            Expr::Path(segs) => segs.last().map(|s| s.inner.0).unwrap_or(""),
            _ => return None,
        };
        let template = self.lookup_pure(callee_name)?;
        // Arity must match.  Otherwise downstream typeck would fail
        // anyway; safer to leave alone.
        if template.params.len() != args.len() {
            return None;
        }

        let span = stmt.span;
        let id = self.counter.get();
        self.counter.set(id + 1);

        // Build the rename map: each param's source name → a fresh
        // `__inl_<id>_<param>` name.  Also rename any `let` bindings
        // inside the body so they can't shadow caller's locals.
        let mut map: HashMap<String, &'static str> = HashMap::new();
        for pat in &template.params {
            let param_name = pat.inner.ident.inner.0;
            let fresh: &'static str = Box::leak(
                format!("__inl_{}_{}", id, param_name).into_boxed_str(),
            );
            map.insert(param_name.to_string(), fresh);
        }
        collect_local_let_names(&template.body, &mut map, id);

        // Emit arg-binding lets at the start of the expansion.
        let mut out: Vec<Spanned<Expr<'src>>> = Vec::new();
        for (pat, arg) in template.params.iter().zip(args.iter()) {
            let param_name = pat.inner.ident.inner.0;
            let fresh = map[&param_name.to_string()];
            out.push(
                Expr::Let {
                    ident: Ident::new(fresh).with_span(span),
                    ty: pat.inner.ty.clone(),
                    default: Some(Box::new(arg.clone())),
                }
                .with_span(span),
            );
        }

        // Substitute params + locals in body.
        let substituted: Vec<Spanned<Expr<'src>>> = template
            .body
            .iter()
            .cloned()
            .map(|e| substitute_in_expr(e, &map))
            .collect();

        // Rewrite the final `ret <expr>` to `let <user_ident> = <expr>`
        // so the caller sees the value bound to its target name.  All
        // prior statements in the substituted body (rt_store etc.) are
        // kept as-is.
        let (prefix, ret_expr) = split_final_ret(substituted);
        out.extend(prefix);
        out.push(
            Expr::Let {
                ident: user_ident.clone(),
                ty: user_ty.clone(),
                default: Some(Box::new(ret_expr)),
            }
            .with_span(span),
        );
        Some(out)
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
        self.lower_body_impl(body, is_ret_branch, /* is_outermost */ true)
    }

    /// Internal worker — `is_outermost` controls whether Phase 3 (tail
    /// wrap into `__caller(self <expr>)`) fires.  The outer branch body
    /// owns the wrap; recursive calls from Phase 4 on `when` / `wait`
    /// arm bodies pass `false` so each arm doesn't sprout its own
    /// wrap (which would double-wrap the user's tail and feed
    /// `__caller(...)` a self()-state-change argument the backend
    /// can't fold into a value).
    fn lower_body_impl(
        &self,
        body: Vec<Spanned<Expr<'src>>>,
        is_ret_branch: bool,
        is_outermost: bool,
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

        // Phase 1.0: thread `__caller` through `self(args)` calls
        // inside a ret-typed branch.  Without this, a tail-recursive
        // `self(...)` would have one fewer arg than the branch
        // signature (which now begins with the synthetic `__caller
        // pid` parameter), surfacing as an arity mismatch at typeck.
        let body: Vec<_> = if is_ret_branch {
            body.into_iter()
                .map(|e| self.thread_caller_in_self_calls(e))
                .collect()
        } else {
            body
        };

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

        // Phase 1c: bare `M(args)` where M is a ret-machine becomes
        // `let __discard_N = M(args)` so the call is awaited.  Without
        // this, the body races ahead while the callee's side effects
        // (mutating shared heap state in helpers like `set_be32`,
        // `process_block`, …) are still pending.  Sequencing matters
        // — SHA-256's inner loop wouldn't terminate correctly without
        // it.  The Phase 2 sweep below sees the synth let just like
        // any other and expands it into wait + pid_let.
        let body: Vec<_> = body
            .into_iter()
            .map(|e| self.wrap_bare_ret_call_in_let(e))
            .collect();

        // Phase 1.d: inline pure ret-machine calls (#78 phase 2a).
        // Each `let r = pure_f(args)` expands to:
        //   let __inl_K_<param> = arg                         (per param)
        //   <substituted body, minus its `ret e`>
        //   let r = e
        // Phase 2 below then sees no pure calls left — only spawns
        // for impure ret-machines.
        let body = self.inline_pure_calls(body);

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
            new_body.push(expr);
        }

        // Phase 3: tail wrap for ret-branches.  Skip when the last
        // expression is already a __caller send (the user wrote `ret X`
        // there) or when the body is empty.  Only the outermost body
        // wraps — Phase 4 calls back into `lower_body_impl` with
        // `is_outermost=false` for each arm, since the outer wrap
        // already accounts for the fall-through value.
        if is_ret_branch
            && is_outermost
            && let Some(last) = new_body.last_mut()
            && !is_caller_send(&last.inner)
        {
            wrap_in_caller_send(last);
        }

        // Phase 4: re-enter `when` / `wait` arm bodies as their own
        // sub-bodies so lift + expand_let_with_ret get the same shot
        // at them they get at the top-level body.  Without this,
        // `let s = ret_machine(x)` written inside an arm stays
        // unlowered (typeck then fails on the un-prepended `self`).
        // Phase 1 (Ret rewrite) is idempotent, Phase 1.0 has an
        // explicit `already_threaded` guard, so re-running both on
        // an arm body that the existing recursion has already
        // touched is safe.
        new_body = new_body
            .into_iter()
            .map(|e| self.lower_arm_bodies_in_expr(e, is_ret_branch))
            .collect();

        new_body
    }

    /// Walk an expression and re-run `lower_body` on each nested
    /// `when` / `wait` arm body.  Each arm carries its own scope, so
    /// each arm's body is treated as an independent body pass.  The
    /// `is_ret_branch` flag propagates so arms inherit the enclosing
    /// branch's return discipline.
    fn lower_arm_bodies_in_expr(
        &self,
        expr: Spanned<Expr<'src>>,
        is_ret_branch: bool,
    ) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::When { cond, branches } => {
                let cond = Box::new(self.lower_arm_bodies_in_expr(*cond, is_ret_branch));
                let branches = branches
                    .into_iter()
                    .map(|(pat, body)| {
                        let pat = self.lower_arm_bodies_in_expr(pat, is_ret_branch);
                        let body = self.lower_body_impl(body, is_ret_branch, false);
                        (pat, body)
                    })
                    .collect();
                Expr::When { cond, branches }
            }
            Expr::Wait { handlers, filter } => {
                let filter = filter
                    .into_iter()
                    .map(|f| self.lower_arm_bodies_in_expr(f, is_ret_branch))
                    .collect();
                let handlers = handlers
                    .into_iter()
                    .map(|h| {
                        let h_span = h.span;
                        let mut br = h.inner;
                        br.body = self.lower_body_impl(br.body, is_ret_branch, false);
                        br.with_span(h_span)
                    })
                    .collect();
                Expr::Wait { handlers, filter }
            }
            Expr::Let { ident, ty, default } => Expr::Let {
                ident,
                ty,
                default: default
                    .map(|d| Box::new(self.lower_arm_bodies_in_expr(*d, is_ret_branch))),
            },
            // Phase 3 may have wrapped the tail in `__caller(self
            // <expr>)`.  Descend through the synthetic Jump so a
            // wrapped `when` / `wait` still gets its arm bodies
            // lowered.  Other AST shapes that can host a control-flow
            // node (Ret, Pin, Tuple, TupleIndex, Neg) get the same
            // treatment.
            Expr::Jump { ident, args } => Expr::Jump {
                ident: Box::new(self.lower_arm_bodies_in_expr(*ident, is_ret_branch)),
                args: args
                    .into_iter()
                    .map(|a| self.lower_arm_bodies_in_expr(a, is_ret_branch))
                    .collect(),
            },
            Expr::Ret(inner) => Expr::Ret(Box::new(
                self.lower_arm_bodies_in_expr(*inner, is_ret_branch),
            )),
            Expr::Pin(inner) => Expr::Pin(Box::new(
                self.lower_arm_bodies_in_expr(*inner, is_ret_branch),
            )),
            Expr::Tuple(elems) => Expr::Tuple(
                elems
                    .into_iter()
                    .map(|e| self.lower_arm_bodies_in_expr(e, is_ret_branch))
                    .collect(),
            ),
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(
                    self.lower_arm_bodies_in_expr(*receiver, is_ret_branch),
                ),
                index,
            },
            Expr::Neg(inner) => Expr::Neg(Box::new(
                self.lower_arm_bodies_in_expr(*inner, is_ret_branch),
            )),
            other => other,
        };
        inner.with_span(span)
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

    /// Recursively prepend `__caller` to every `Expr::Jump` whose
    /// callee is a bare `self`.  Run only inside ret-typed branches
    /// where the synthetic `__caller pid` parameter has already been
    /// inserted at position 0; non-ret branches keep the user's args
    /// untouched so `self(n - 1)` in a counter machine still works.
    fn thread_caller_in_self_calls(
        &self,
        expr: Spanned<Expr<'src>>,
    ) -> Spanned<Expr<'src>> {
        let span = expr.span;
        let inner = match expr.inner {
            Expr::Jump { ident, args } => {
                // Detect `self(...)` — the callee is a Var named "self".
                let is_self_call = matches!(
                    &ident.inner,
                    Expr::Var("self", _)
                );
                // Idempotency guard: if a previous pass already
                // prepended `__caller`, don't add a second copy.
                // This matters because Phase 4 re-runs lower_body on
                // when/wait arm bodies, which fires this pass again
                // on already-threaded calls.
                let already_threaded = is_self_call
                    && args
                        .first()
                        .map(|a| matches!(&a.inner, Expr::Var("__caller", _)))
                        .unwrap_or(false);
                let new_args: Vec<_> = args
                    .into_iter()
                    .map(|a| self.thread_caller_in_self_calls(a))
                    .collect();
                let final_args = if is_self_call && !already_threaded {
                    let mut prepended = Vec::with_capacity(new_args.len() + 1);
                    prepended.push(
                        Expr::Var("__caller", Type::Named(
                            crate::api::ast::Ident::new("pid").with_span(span),
                        ))
                        .with_span(span),
                    );
                    prepended.extend(new_args);
                    prepended
                } else {
                    new_args
                };
                Expr::Jump {
                    ident: Box::new(self.thread_caller_in_self_calls(*ident)),
                    args: final_args,
                }
            }
            Expr::Let { ident, ty, default } => Expr::Let {
                ident,
                ty,
                default: default.map(|d| Box::new(self.thread_caller_in_self_calls(*d))),
            },
            Expr::When { cond, branches } => Expr::When {
                cond: Box::new(self.thread_caller_in_self_calls(*cond)),
                branches: branches
                    .into_iter()
                    .map(|(p, body)| {
                        (
                            self.thread_caller_in_self_calls(p),
                            body.into_iter()
                                .map(|e| self.thread_caller_in_self_calls(e))
                                .collect(),
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
                            .map(|e| self.thread_caller_in_self_calls(e))
                            .collect();
                        br.with_span(span)
                    })
                    .collect(),
                filter: filter
                    .into_iter()
                    .map(|f| self.thread_caller_in_self_calls(f))
                    .collect(),
            },
            Expr::Tuple(elems) => Expr::Tuple(
                elems
                    .into_iter()
                    .map(|e| self.thread_caller_in_self_calls(e))
                    .collect(),
            ),
            Expr::TupleIndex { receiver, index } => Expr::TupleIndex {
                receiver: Box::new(self.thread_caller_in_self_calls(*receiver)),
                index,
            },
            Expr::Ret(inner) => {
                Expr::Ret(Box::new(self.thread_caller_in_self_calls(*inner)))
            }
            Expr::Pin(inner) => {
                Expr::Pin(Box::new(self.thread_caller_in_self_calls(*inner)))
            }
            Expr::Neg(inner) => {
                Expr::Neg(Box::new(self.thread_caller_in_self_calls(*inner)))
            }
            other => other,
        };
        inner.with_span(span)
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
            // Binary operators recurse into both operands — `lift_arg`
            // hoists any nested ret-machine call out of arithmetic /
            // bitwise / compare positions into a fresh `let __lift_<id>`
            // so expand_let_with_ret can later cascade it through a
            // wait-handler.  Without this, `rotr32(x 2) ^ rotr32(x 13)`
            // stays as two unlifted Jumps and typeck fails because
            // they never gain the synthetic `self` first arg the
            // ret-machine signature expects after lowering.
            Expr::Add(l, r) => Expr::Add(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Sub(l, r) => Expr::Sub(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Mul(l, r) => Expr::Mul(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Div(l, r) => Expr::Div(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Eq(l, r) => Expr::Eq(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Lt(l, r) => Expr::Lt(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Gt(l, r) => Expr::Gt(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Le(l, r) => Expr::Le(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Ge(l, r) => Expr::Ge(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::BAnd(l, r) => Expr::BAnd(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::BOr(l, r) => Expr::BOr(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::BXor(l, r) => Expr::BXor(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Shl(l, r) => Expr::Shl(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            Expr::Shr(l, r) => Expr::Shr(
                Box::new(self.lift_arg(*l, lifts)),
                Box::new(self.lift_arg(*r, lifts)),
            )
            .with_span(span),
            // when / wait arm bodies have their own scope; Phase 4 in
            // `lower_body` re-enters each arm by recursively calling
            // lower_body, which fires this pass on the arm body in
            // isolation.  Recursing here would bleed lifts across
            // arms (lifts pushed into the outer body, referenced from
            // an arm body that the lift's binding is no longer
            // visible in).
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
    /// Wrap a bare top-level call `M(args)` whose callee is a
    /// ret-machine in a synthetic `let __discard_<id> = M(args)`.
    /// The Phase 2 sweep then expands the let into wait + pid_let,
    /// turning the bare call into a sync await.  Non-ret callees
    /// (rt-funcs, regular machines, pid sends) pass through.
    fn wrap_bare_ret_call_in_let(
        &self,
        expr: Spanned<Expr<'src>>,
    ) -> Spanned<Expr<'src>> {
        // Only act on top-level Jump expressions.  Let / When / Wait /
        // arith etc are walked by other phases; the bare-call shape
        // is exactly `M(args)` at statement position.
        if !matches!(expr.inner, Expr::Jump { .. }) {
            return expr;
        }
        if self.ret_target(&expr.inner).is_none() {
            return expr;
        }
        let span = expr.span;
        let id = self.fresh_id();
        let name: &'static str =
            Box::leak(format!("__discard_{id}").into_boxed_str());
        Expr::Let {
            ident: Ident::new(name).with_span(span),
            ty: Type::Unknown.with_span(span),
            default: Some(Box::new(expr)),
        }
        .with_span(span)
    }

    #[allow(dead_code)]
    fn try_rewrite_bare_ret_call(
        &self,
        expr: &Spanned<Expr<'src>>,
    ) -> Option<Spanned<Expr<'src>>> {
        let Expr::Jump { ident, args } = &expr.inner else {
            return None;
        };
        let _ = self.ret_target(&expr.inner)?;
        // Idempotency guard: a pid-typed first arg means a previous
        // pass already prepended either a `self` (caller-side
        // expand_let_with_ret) or a `0` null sentinel (this pass).
        // Phase 4 re-runs lower_body on `when` / `wait` arm bodies,
        // which would otherwise stack one extra null pid per pass.
        if let Some(first) = args.first()
            && first_arg_is_pid(&first.inner)
        {
            return None;
        }
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

/// Cheap shape check: does `expr` evaluate to a pid?  Used by the
/// bare-ret-call rewrite as an idempotency marker — if the first
/// argument is already a pid expression, the prepend has already
/// fired on a previous pass.  We accept both the `0`-pid sentinel
/// and `self` since both shapes pre-date a re-run.
fn first_arg_is_pid<'src>(expr: &Expr<'src>) -> bool {
    match expr {
        Expr::Var(_, Type::Named(t)) if t.inner.0 == "pid" => true,
        Expr::Num(_, Type::Named(t)) if t.inner.0 == "pid" => true,
        _ => false,
    }
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
    let s = s.trim();
    // Anonymous struct: `{ T1 T2 ... }` rendered by Type::Display.
    if let Some(inner) = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
        let inner = inner.trim();
        if inner.is_empty() {
            return Type::Unit;
        }
        // Split by whitespace at top level.  Lake's current Type set is flat
        // (no nested generics inside anonymous structs at the surface
        // syntax), so naive whitespace splitting is sufficient.
        let fields: Vec<_> = inner
            .split_whitespace()
            .map(|tok| type_from_string::<'src>(tok, span).with_span(span))
            .collect();
        return Type::Struct(fields);
    }
    let static_name: &'static str = match s {
        "i64" => "i64",
        "str" => "str",
        "buf" => "buf",
        "bool" => "bool",
        "pid" => "pid",
        "atom" => "atom",
        _ => return Type::Unknown,
    };
    Type::Named(Ident::new(static_name).with_span(span))
}
