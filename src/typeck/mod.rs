//! Type checker for Lake.
//!
//! Runs on the **resolved** AST (after [`crate::resolver::resolve`]) and emits
//! [`LakeError`]s. For the full list of error codes see `ERROR_CODES.md`.

use std::collections::HashMap;

use chumsky::span::SimpleSpan;

use crate::{
    api::{
        ast::{Branch, Machine, MachineItem, Type},
        expr::Expr,
    },
    error_handle::{LakeError, LakeErrors},
    resolver::is_unknown,
};

// ─── Branch signature ─────────────────────────────────────────────────────────

/// The type-string of each non-default, non-wildcard parameter in a branch.
#[derive(Debug, Clone)]
struct BranchSig {
    /// Human-readable type list, e.g. `["i64", "str"]`.
    param_types: Vec<String>,
}

impl BranchSig {
    fn matches_arg_types(&self, arg_types: &[String]) -> bool {
        self.param_types == arg_types
    }

    fn display(&self) -> String {
        if self.param_types.is_empty() {
            "_".to_string()
        } else {
            self.param_types.join(" ")
        }
    }
}

// ─── TypeChecker ─────────────────────────────────────────────────────────────

/// Single-pass type checker.
pub struct TypeChecker<'src> {
    /// All machine signatures collected in pass 1.
    machines: HashMap<&'src str, Vec<BranchSig>>,
    errors: Vec<LakeError>,
}

impl<'src> TypeChecker<'src> {
    fn new() -> Self {
        Self {
            machines: HashMap::new(),
            errors: vec![],
        }
    }

    // ── Pass 1: collect machine signatures ───────────────────────────────────

    fn collect_signatures(&mut self, program: &'src [chumsky::span::Spanned<Expr<'src>>]) {
        for item in program {
            if let Expr::Machine(m) = &item.inner {
                self.collect_machine_sig(m.inner.ident.inner.0, &m.inner);
            }
        }
    }

    fn collect_machine_sig(&mut self, name: &'src str, machine: &'src Machine<'src>) {
        let sigs = machine
            .items
            .iter()
            .filter_map(|item| {
                if let MachineItem::Branch(b) = &item.inner {
                    Some(branch_sig(b))
                } else {
                    None
                }
            })
            .collect();
        self.machines.insert(name, sigs);
    }

    // ── Pass 2: check each machine ───────────────────────────────────────────

    fn check_program(&mut self, program: &'src [chumsky::span::Spanned<Expr<'src>>]) {
        for item in program {
            if let Expr::Machine(m) = &item.inner {
                let machine_name = m.inner.ident.inner.0;
                for machine_item in &m.inner.items {
                    if let MachineItem::Branch(b) = &machine_item.inner {
                        self.check_branch(machine_name, b);
                    }
                }
            }
        }
    }

    fn check_branch(&mut self, machine_name: &'src str, branch: &'src Branch<'src>) {
        // Build the local scope from non-wildcard, non-default patterns.
        let mut scope: HashMap<&'src str, &'src Type<'src>> = HashMap::new();
        for pat in &branch.patterns {
            if !pat.inner.is_wildcard() {
                scope.insert(pat.inner.ident.inner.0, &pat.inner.ty.inner);
            }
        }

        for expr in &branch.body {
            self.check_expr(machine_name, &scope, &expr.inner, expr.span);
        }
    }

    fn check_expr(
        &mut self,
        machine_name: &'src str,
        scope: &HashMap<&'src str, &'src Type<'src>>,
        expr: &'src Expr<'src>,
        span: SimpleSpan,
    ) {
        match expr {
            // ── Variable reference ────────────────────────────────────────────
            Expr::Var(name, ty) => {
                // `self` is a keyword; its type placeholder is intentional.
                if *name == "self" {
                    return;
                }
                if is_unknown(ty) && !scope.contains_key(name) {
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

            // ── `let` binding ─────────────────────────────────────────────────
            // The binding itself is always valid; check the default expression.
            Expr::Let {
                default: Some(default),
                ..
            } => {
                self.check_expr(machine_name, scope, &default.inner, default.span);
            }

            // ── Call / spawn / self-transition ────────────────────────────────
            Expr::Jump { ident, args } => {
                // Check argument expressions (not the callee itself).
                for arg in args {
                    self.check_expr(machine_name, scope, &arg.inner, arg.span);
                }

                // `self(…)` — verify a matching branch exists.
                if let Expr::Var("self", _) = &ident.inner {
                    self.check_self_call(machine_name, args, span);
                }
            }

            // ── Arithmetic ────────────────────────────────────────────────────
            Expr::Add(l, r) | Expr::Sub(l, r) | Expr::Mul(l, r) | Expr::Div(l, r) => {
                self.check_arith_operand(machine_name, scope, l);
                self.check_arith_operand(machine_name, scope, r);
            }

            // ── When ──────────────────────────────────────────────────────────
            Expr::When { cond, branches } => {
                self.check_expr(machine_name, scope, &cond.inner, cond.span);
                for (pat, body) in branches {
                    self.check_expr(machine_name, scope, &pat.inner, pat.span);
                    for e in body {
                        self.check_expr(machine_name, scope, &e.inner, e.span);
                    }
                }
            }

            _ => {}
        }
    }

    /// Check that `self(args)` has a matching branch in the enclosing machine.
    fn check_self_call(
        &mut self,
        machine_name: &'src str,
        args: &'src [chumsky::span::Spanned<Expr<'src>>],
        call_span: SimpleSpan,
    ) {
        let arg_types: Vec<String> = args.iter().map(|a| expr_type_str(&a.inner)).collect();

        let Some(sigs) = self.machines.get(machine_name) else {
            // Unknown machine — shouldn't happen if we're checking its own body.
            return;
        };

        let matches = sigs.iter().any(|s| s.matches_arg_types(&arg_types));
        if !matches {
            let branch_list = sigs
                .iter()
                .map(|s| format!("`{}`", s.display()))
                .collect::<Vec<_>>()
                .join(", ");

            let call_sig = if arg_types.is_empty() {
                "_".to_string()
            } else {
                arg_types.join(" ")
            };

            self.errors.push(
                LakeError::with_label_msg(
                    format!("no branch of `{machine_name}` matches the call `self({call_sig})`"),
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
                ),
            );
        }
    }

    /// Check that an arithmetic operand has a known (non-`{}`) type.
    fn check_arith_operand(
        &mut self,
        machine_name: &'src str,
        scope: &HashMap<&'src str, &'src Type<'src>>,
        operand: &'src chumsky::span::Spanned<Expr<'src>>,
    ) {
        if expr_type_str(&operand.inner) == "{}" {
            self.errors.push(
                LakeError::new("arithmetic on a value of unknown type", operand.span)
                    .code("E002")
                    .note("arithmetic requires operands of type `i64`")
                    .help("declare the variable in the branch parameters with a concrete type"),
            );
        }
        // Recurse to also check sub-expressions.
        self.check_expr(machine_name, scope, &operand.inner, operand.span);
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build the signature of a branch from its non-default, non-wildcard patterns.
fn branch_sig(branch: &Branch<'_>) -> BranchSig {
    let param_types = branch
        .patterns
        .iter()
        .filter(|p| !p.inner.is_wildcard() && p.inner.default.is_none())
        .map(|p| p.inner.ty.inner.to_string())
        .collect();
    BranchSig { param_types }
}

/// Return the display type string for an expression used as a call argument.
/// `"{}"` means "unknown / unresolved".
fn expr_type_str(expr: &Expr<'_>) -> String {
    match expr {
        Expr::Var(_, ty) => ty.to_string(),
        Expr::Num(_, ty) => ty.to_string(),
        Expr::String(_, ty) => ty.to_string(),
        Expr::Bool(_) => "bool".to_string(),
        Expr::Add(l, _) | Expr::Sub(l, _) | Expr::Mul(l, _) | Expr::Div(l, _) => {
            expr_type_str(&l.inner)
        }
        _ => "{}".to_string(),
    }
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Type-check a **resolved** Lake program and return all diagnostics.
///
/// Run [`crate::resolver::resolve`] on the AST before calling this.
pub fn typecheck<'src>(program: &'src [chumsky::span::Spanned<Expr<'src>>]) -> LakeErrors {
    let mut checker = TypeChecker::new();
    checker.collect_signatures(program);
    checker.check_program(program);
    LakeErrors::new(checker.errors)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chumsky::{Parser, input::Input};

    use super::typecheck;
    use crate::{lexer::lexer, parser::program, resolver::resolve};

    fn check(src: &str) -> Vec<String> {
        let tokens = lexer().parse(src).into_result().expect("lex error");
        let ast = program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse error");
        let resolved = resolve(ast);
        typecheck(&resolved)
            .0
            .into_iter()
            .map(|e| e.message)
            .collect()
    }

    fn check_codes(src: &str) -> Vec<String> {
        let tokens = lexer().parse(src).into_result().expect("lex error");
        let ast = program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse error");
        let resolved = resolve(ast);
        typecheck(&resolved)
            .0
            .into_iter()
            .map(|e| e.code.unwrap_or_default())
            .collect()
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
}
