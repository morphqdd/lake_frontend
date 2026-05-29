//! track_caller-style source-location injection for `panic` / `assert`.
//!
//! Lake compiles to a CPS scheduler trampoline, so there is no native
//! call stack to walk for a backtrace.  As a pragmatic stand-in we give
//! `panic` (and `assert`) the *call-site* location — the first line of a
//! Rust panic (`panicked at file:line:col`).  This pass runs right after
//! cfg-filtering, before the registry is first populated, so the
//! injected arguments are visible to type-checking and monomorphisation.
//!
//! Mechanism: a user-written `panic(msg)` (arity 1) is rewritten to
//! `panic(msg "<path>" <line>)` (arity 3); `assert(cond msg)` (arity 2)
//! to `assert(cond msg "<path>" <line>)` (arity 4).  Calls that already
//! carry the extra args (the internal forward inside `assert`'s body)
//! are left untouched, so injection is idempotent and non-recursive.
//!
//! Only statement positions are walked (branch bodies, `when` / `wait`
//! arm bodies, and `pin` / `ret` wrappers).  `panic` is a control-flow
//! statement in practice; nesting it inside an arithmetic expression is
//! not meaningful, so deep-expression coverage is intentionally skipped.

use chumsky::span::{SimpleSpan, Spanned};

use crate::api::{
    ast::{Ident, Item, Machine, MachineItem, Type},
    expr::Expr,
};
use crate::loader::ParsedProgram;

/// Source-name arity of the user-facing entry points.  A call with this
/// many args is a bare user call → inject; anything else is left alone
/// (already-located internal forward, or an arity error for typeck).
const PANIC_ARITY: usize = 1;
const ASSERT_ARITY: usize = 2;

/// Walk every module, rewriting `panic` / `assert` statement calls to
/// carry their call-site `(file, line)`.
pub fn inject_panic_location(mut program: ParsedProgram<'_>) -> ParsedProgram<'_> {
    for module in &mut program.modules {
        // The src text isn't carried on ParsedModule; re-read from disk
        // to map byte spans → (line, col).  Best-effort: an unreadable
        // path just yields line 0 (location still names the file).
        let src = std::fs::read_to_string(&module.source_path).unwrap_or_default();
        let path = module.source_path.to_string_lossy().into_owned();
        let cx = Ctx {
            line_starts: line_starts(&src),
            path,
        };
        for item in &mut module.ast {
            if let Item::Machine(m) = &mut item.inner {
                rewrite_machine(&mut m.inner, &cx);
            }
        }
    }
    program
}

struct Ctx {
    /// Byte offset of the start of each line (line N begins at index N).
    line_starts: Vec<usize>,
    /// Display path of the module's source file.
    path: String,
}

impl Ctx {
    /// 1-based (line, col) for a byte offset.
    fn line_col(&self, off: usize) -> (usize, usize) {
        // Last line-start <= off.
        let line = match self.line_starts.binary_search(&off) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let col = off - self.line_starts[line];
        (line + 1, col + 1)
    }
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

fn rewrite_machine<'src>(m: &mut Machine<'src>, cx: &Ctx) {
    for item in &mut m.items {
        if let MachineItem::Branch(b) = &mut item.inner {
            rewrite_stmts(&mut b.body, cx);
        }
    }
}

/// Rewrite a statement list in place.
fn rewrite_stmts<'src>(stmts: &mut [Spanned<Expr<'src>>], cx: &Ctx) {
    for s in stmts.iter_mut() {
        rewrite_stmt(s, cx);
    }
}

fn rewrite_stmt<'src>(stmt: &mut Spanned<Expr<'src>>, cx: &Ctx) {
    let span = stmt.span;
    match &mut stmt.inner {
        // Direct call statement: `panic(msg)` / `assert(cond msg)`.
        Expr::Jump { ident, args } => {
            if let Some((_name, want)) = panic_like(&ident.inner) {
                if args.len() == want {
                    let (line, col) = cx.line_col(span.start as usize);
                    // Single combined `path:line:col` literal — keeps the
                    // call one arg wider and needs no runtime int→string.
                    let loc = format!("{}:{}:{}", cx.path, line, col);
                    args.push(str_lit(&loc, span));
                }
            }
            // Recurse into argument exprs too (e.g. `f(panic(x))`).
            for a in args.iter_mut() {
                rewrite_stmt(a, cx);
            }
        }
        // `pin <call>` / `ret <call>` wrap a single statement.
        Expr::Pin(inner) | Expr::Ret(inner) => rewrite_stmt(inner, cx),
        // `when cond { pat -> { body } ... }` — recurse arm bodies.
        Expr::When { cond, branches } => {
            rewrite_stmt(cond, cx);
            for (_pat, body) in branches.iter_mut() {
                rewrite_stmts(body, cx);
            }
        }
        // `wait … { @label pat -> { body } }` — recurse handler bodies.
        Expr::Wait { handlers, filter } => {
            for f in filter.iter_mut() {
                rewrite_stmt(f, cx);
            }
            for h in handlers.iter_mut() {
                rewrite_stmts(&mut h.inner.body, cx);
            }
        }
        // `let x = <call>` — the initialiser may itself be a panic.
        Expr::Let { default: Some(d), .. } => rewrite_stmt(d, cx),
        _ => {}
    }
}

/// If `callee` names `panic` / `assert`, return (name, user-arity).
fn panic_like<'src>(callee: &Expr<'src>) -> Option<(&'src str, usize)> {
    match callee {
        Expr::Var(name, _) => match *name {
            "panic" => Some(("panic", PANIC_ARITY)),
            "assert" => Some(("assert", ASSERT_ARITY)),
            _ => None,
        },
        _ => None,
    }
}

/// `Expr::String` literal carrying a leaked `&'src str` (the pass runs
/// before any borrow of the program outlives it; leaking keeps the
/// 'src lifetime without threading an arena through every call site).
fn str_lit<'src>(s: &str, span: SimpleSpan) -> Spanned<Expr<'src>> {
    use chumsky::span::SpanWrap;
    let leaked: &'src str = Box::leak(s.to_string().into_boxed_str());
    Expr::String(leaked, Type::Named(Ident::new("str").with_span(span))).with_span(span)
}

fn num_lit<'src>(n: usize, span: SimpleSpan) -> Spanned<Expr<'src>> {
    use chumsky::span::SpanWrap;
    let leaked: &'src str = Box::leak(n.to_string().into_boxed_str());
    Expr::Num(leaked, Type::Named(Ident::new("i64").with_span(span))).with_span(span)
}
