//! Type checker for Lake.
//!
//! Runs on the **resolved** AST (after [`crate::resolver::resolve`]) and emits
//! [`LakeError`]s. For the full list of error codes see `ERROR_CODES.md`.

use std::collections::HashMap;

use chumsky::span::SimpleSpan;

use crate::{
    api::{
        ast::{Branch, Directive, Machine, MachineItem, Type},
        expr::Expr,
    },
    error_handle::{LakeError, LakeErrors},
    resolver::is_unknown,
};

/// The type-string of each non-default, non-wildcard parameter in a branch.
#[derive(Debug, Clone)]
struct BranchSig {
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

/// Single-pass type checker.
pub struct TypeChecker<'src> {
    /// All machine signatures collected in pass 1.
    machines: HashMap<&'src str, Vec<BranchSig>>,
    directives: HashMap<&'src str, Directive<'src>>,
    errors: Vec<LakeError>,
}

impl<'src> TypeChecker<'src> {
    fn new() -> Self {
        Self {
            machines: HashMap::new(),
            directives: HashMap::new(),
            errors: vec![],
        }
    }

    fn collect_signatures(&mut self, program: Vec<chumsky::span::Spanned<Expr<'src>>>) {
        for item in program {
            match item.inner {
                Expr::Machine(m) => {
                    self.collect_machine_sig(m.inner.ident.inner.0, m.inner);
                }
                Expr::Directive(d) => {
                    self.directives.insert(d.name.0, d.inner.clone());
                }
                _ => continue,
            }
        }
    }

    fn collect_machine_sig(&mut self, name: &'src str, machine: Machine<'src>) {
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

    fn check_program(&mut self, program: Vec<chumsky::span::Spanned<Expr<'src>>>) {
        for item in program {
            if let Expr::Machine(m) = item.inner {
                let machine_name = m.inner.ident.inner.0;
                for machine_item in m.inner.items {
                    if let MachineItem::Branch(b) = machine_item.inner {
                        self.check_branch(machine_name, b);
                    }
                }
            }
        }
    }

    fn check_branch(&mut self, machine_name: &'src str, branch: Branch<'src>) {
        let mut scope: HashMap<&'src str, Type<'src>> = HashMap::new();
        for pat in branch.patterns {
            if !pat.inner.is_wildcard() {
                scope.insert(pat.inner.ident.inner.0, pat.inner.ty.inner);
            }
        }

        for expr in branch.body {
            self.check_expr(machine_name, &scope, expr.inner, expr.span);
        }
    }

    fn check_expr(
        &mut self,
        machine_name: &'src str,
        scope: &HashMap<&'src str, Type<'src>>,
        expr: Expr<'src>,
        span: SimpleSpan,
    ) {
        match expr {
            Expr::Var(name, ty) => {
                if name == "self" {
                    return;
                }
                if is_unknown(&ty) && !scope.contains_key(name) {
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
                self.check_expr(machine_name, scope, default.inner, default.span);
            }

            Expr::Jump { ident, args } => {
                for arg in args.clone() {
                    self.check_expr(machine_name, scope, arg.inner, arg.span);
                }

                if let Expr::Var("self", _) = ident.inner.clone() {
                    self.check_call(machine_name, args.clone(), span, true);
                }

                if let Expr::Var(ident, _) = ident.inner {
                    if self.directives.contains_key(ident) {
                        return;
                    }
                    self.check_call(ident, args, span, false);
                }
            }

            Expr::Add(l, r) | Expr::Sub(l, r) | Expr::Mul(l, r) | Expr::Div(l, r) => {
                self.check_arith_operand(machine_name, scope, *l);
                self.check_arith_operand(machine_name, scope, *r);
            }

            Expr::When { cond, branches } => {
                self.check_expr(machine_name, scope, cond.inner, cond.span);
                for (pat, body) in branches {
                    self.check_expr(machine_name, scope, pat.inner, pat.span);
                    for e in body {
                        self.check_expr(machine_name, scope, e.inner, e.span);
                    }
                }
            }

            _ => {}
        }
    }

    fn check_call(
        &mut self,
        machine_name: &'src str,
        args: Vec<chumsky::span::Spanned<Expr<'src>>>,
        call_span: SimpleSpan,
        is_self: bool,
    ) {
        let arg_types: Vec<String> = args.iter().map(|a| expr_type_str(&a.inner)).collect();

        let Some(sigs) = self.machines.get(machine_name) else {
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

            let callee = if is_self { "self" } else { machine_name };

            self.errors.push(
                LakeError::with_label_msg(
                    format!(
                        "no branch of `{machine_name}` matches the call `{callee}({call_sig})`"
                    ),
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
        scope: &HashMap<&'src str, Type<'src>>,
        operand: chumsky::span::Spanned<Expr<'src>>,
    ) {
        if expr_type_str(&operand.inner) == "{}" {
            self.errors.push(
                LakeError::new("arithmetic on a value of unknown type", operand.span)
                    .code("E002")
                    .note("arithmetic requires operands of type `i64`")
                    .help("declare the variable in the branch parameters with a concrete type"),
            );
        }

        self.check_expr(machine_name, scope, operand.inner, operand.span);
    }
}

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

/// Type-check a **resolved** Lake program and return all diagnostics.
///
/// Run [`crate::resolver::resolve`] on the AST before calling this.
pub fn typecheck<'src>(program: Vec<chumsky::span::Spanned<Expr<'src>>>) -> LakeErrors {
    let mut checker = TypeChecker::new();
    checker.collect_signatures(program.clone());
    checker.check_program(program);
    LakeErrors::new(checker.errors)
}

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
        typecheck(resolved)
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
        typecheck(resolved)
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
