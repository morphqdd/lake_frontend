//! Semantic analysis for LSP support.
//!
//! This module builds on top of the resolver and typechecker to provide
//! semantic information that can be used by language servers for:
//! - Semantic highlighting
//! - Go-to-definition
//! - Autocomplete
//! - Hover information

use std::collections::HashMap;

use chumsky::span::{SimpleSpan, Spanned};

use crate::api::{
    ast::{Branch, Directive, Import, Item, Machine, Type},
    expr::Expr,
};
use crate::visit::{Visit, walk_expr};

/// Kind of symbol in the Lake program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A machine definition (function)
    Machine,
    /// A parameter in a branch pattern
    Parameter,
    /// A local variable from a `let` binding
    LocalVariable,
    /// A runtime function imported via `@rt` directive
    RuntimeFunction,
    /// A method call (receiver@method)
    Method,
    /// A field access (receiver@field or receiver.field)
    Field,
    /// A namespace/module
    Namespace,
    /// A type name
    TypeName,
}

/// A symbol with its type and location information.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub typ: String,  // String representation of the type
    pub span: SimpleSpan,
}

/// Semantic information for a Lake program.
#[derive(Debug, Default)]
pub struct SemanticInfo {
    /// All symbols in the program with their locations
    pub symbols: Vec<Symbol>,
    /// Machine name → list of branch signatures
    pub machines: HashMap<String, Vec<String>>,
    /// Runtime function names from @rt directives
    pub runtime_functions: Vec<String>,
}

impl SemanticInfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Find the symbol at a specific position (byte offset)
    pub fn symbol_at(&self, offset: usize) -> Option<&Symbol> {
        self.symbols.iter().find(|sym| {
            let range = sym.span.into_range();
            range.contains(&offset)
        })
    }

    /// Get all symbols of a specific kind
    pub fn symbols_by_kind(&self, kind: SymbolKind) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| s.kind == kind)
            .collect()
    }

    /// Check if an identifier is a runtime function
    pub fn is_runtime_function(&self, name: &str) -> bool {
        self.runtime_functions.contains(&name.to_string())
    }
}

/// Semantic analyzer that walks the AST and collects symbol information.
pub struct SemanticAnalyzer {
    info: SemanticInfo,
    /// Current scope stack for tracking variable visibility
    scope_stack: Vec<HashMap<String, Symbol>>,
}

impl SemanticAnalyzer {
    pub fn new() -> Self {
        Self {
            info: SemanticInfo::new(),
            scope_stack: vec![HashMap::new()],
        }
    }

    fn add_symbol(&mut self, symbol: Symbol) {
        // Add to current scope
        if let Some(scope) = self.scope_stack.last_mut() {
            scope.insert(symbol.name.clone(), symbol.clone());
        }
        // Add to global symbol list
        self.info.symbols.push(symbol);
    }

    fn push_scope(&mut self) {
        self.scope_stack.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
    }

    fn analyze_type(&mut self, ty: &Spanned<Type>) {
        match &ty.inner {
            Type::Named(ident) => {
                // Add type name as a symbol (i64, pid, str, custom types)
                self.add_symbol(Symbol {
                    name: ident.inner.0.to_string(),
                    kind: SymbolKind::TypeName,
                    typ: "type".to_string(),
                    span: ident.span,
                });
            }
            Type::Generic(ident, args) => {
                // Add the base type
                self.add_symbol(Symbol {
                    name: ident.inner.0.to_string(),
                    kind: SymbolKind::TypeName,
                    typ: "type".to_string(),
                    span: ident.span,
                });
                // Recursively analyze type arguments
                for arg in args {
                    self.analyze_type(arg);
                }
            }
            Type::Path(ident, rest) => {
                // Add the namespace part
                self.add_symbol(Symbol {
                    name: ident.inner.0.to_string(),
                    kind: SymbolKind::Namespace,
                    typ: "namespace".to_string(),
                    span: ident.span,
                });
                // Recursively analyze the rest of the path
                let rest_type = Spanned {
                    inner: *rest.inner.clone(),
                    span: rest.span,
                };
                self.analyze_type(&rest_type);
            }
            Type::Struct(fields) => {
                // Analyze each field type
                for field in fields {
                    self.analyze_type(field);
                }
            }
            Type::Unit | Type::Unknown => {
                // Unit `{}` and unresolved placeholder — nothing to highlight.
            }
        }
    }

    fn lookup_variable_kind(&self, name: &str) -> SymbolKind {
        // First, search in scope stack (parameters and local variables)
        for scope in self.scope_stack.iter().rev() {
            if let Some(symbol) = scope.get(name) {
                return symbol.kind;
            }
        }

        // Check if it's a runtime function
        if self.info.runtime_functions.contains(&name.to_string()) {
            return SymbolKind::RuntimeFunction;
        }

        // Check if it's a machine
        if self.info.machines.contains_key(name) {
            return SymbolKind::Machine;
        }

        // Default to local variable (might be from outer scope we haven't tracked)
        SymbolKind::LocalVariable
    }

    fn analyze_directive(&mut self, directive: &Spanned<Directive>) {
        // @rt(func_name) declares a runtime function
        let dir_name = directive.inner.name.inner.0;

        if dir_name == "rt" {
            // args is Vec<Spanned<Type>>, for @rt it should have one Named type
            if let Some(arg_type) = directive.inner.args.first() {
                if let Type::Named(ident) = &arg_type.inner {
                    let func_name = ident.inner.0.to_string();
                    self.info.runtime_functions.push(func_name.clone());

                    self.add_symbol(Symbol {
                        name: func_name,
                        kind: SymbolKind::RuntimeFunction,
                        typ: "runtime_function".to_string(),
                        span: ident.span,
                    });
                }
            }
        }
    }

    fn analyze_import(&mut self, import_rc: &Spanned<std::rc::Rc<std::cell::RefCell<Import>>>) {
        let import = import_rc.inner.borrow();
        match &*import {
            Import::Import(ident, next) => {
                // Add each component of the import path as a namespace
                self.add_symbol(Symbol {
                    name: ident.inner.0.to_string(),
                    kind: SymbolKind::Namespace,
                    typ: "namespace".to_string(),
                    span: ident.span,
                });

                // Recursively analyze the rest of the path
                if let Some(next_import) = next {
                    self.analyze_import(next_import);
                }
            }
            Import::MultiImport(imports) => {
                // For multi-imports like +core.{ box.box result.result }
                for import_item in imports {
                    self.analyze_import(import_item);
                }
            }
        }
    }

    /// Build the displayable signature list for a machine (used for LSP
    /// hover/autocomplete).  Filters wildcard and literal-guard patterns the
    /// same way `typeck::branch_sig` does.
    fn build_branch_signatures(machine: &Machine<'_>) -> Vec<String> {
        let mut sigs = Vec::new();
        for item in &machine.items {
            if let crate::api::ast::MachineItem::Branch(b) = &item.inner {
                let sig = b
                    .patterns
                    .iter()
                    .filter(|p| !p.inner.is_wildcard() && !p.inner.is_literal_guard())
                    .map(|p| type_to_string(&p.inner.ty.inner))
                    .collect::<Vec<_>>()
                    .join(" ");
                sigs.push(if sig.is_empty() { "_".to_string() } else { sig });
            }
        }
        sigs
    }

    /// Analyze a complete Lake program and return semantic information.
    pub fn analyze(program: &[Spanned<Item>]) -> SemanticInfo {
        let mut analyzer = Self::new();

        // First pass: collect all machine and runtime function definitions
        // This allows forward references (calling a machine before it's defined)
        analyzer.collect_definitions(program);

        // Second pass: walk every item, building the symbol stream.
        analyzer.visit_program(program);

        analyzer.info
    }

    /// First pass: collect machine names and runtime functions without
    /// recursing into bodies.  Top-level constructs are flat `Item`s so the
    /// walk is a single linear scan.
    fn collect_definitions(&mut self, program: &[Spanned<Item>]) {
        for item in program {
            match &item.inner {
                Item::Directive(directive) => {
                    let dir_name = directive.inner.name.inner.0;
                    if dir_name == "rt" {
                        if let Some(arg_type) = directive.inner.args.first() {
                            if let Type::Named(ident) = &arg_type.inner {
                                let func_name = ident.inner.0.to_string();
                                self.info.runtime_functions.push(func_name);
                            }
                        }
                    }
                }
                Item::Machine(machine) => {
                    let machine_name = machine.inner.ident.inner.0.to_string();
                    if !self.info.machines.contains_key(&machine_name) {
                        self.info.machines.insert(machine_name, Vec::new());
                    }
                }
                Item::Import(_) => {}
            }
        }
    }
}

impl<'src> Visit<'src> for SemanticAnalyzer {
    fn visit_item(&mut self, item: &Spanned<Item<'src>>) {
        match &item.inner {
            Item::Import(import) => self.analyze_import(import),
            Item::Directive(directive) => self.analyze_directive(directive),
            Item::Machine(machine) => self.visit_machine(machine),
        }
    }

    fn visit_machine(&mut self, machine: &Spanned<Machine<'src>>) {
        let name = machine.inner.ident.inner.0.to_string();
        let span = machine.inner.ident.span;

        self.add_symbol(Symbol {
            name: name.clone(),
            kind: SymbolKind::Machine,
            typ: "machine".to_string(),
            span,
        });

        self.info
            .machines
            .insert(name, Self::build_branch_signatures(&machine.inner));

        // Recurse into branches via the default walk.
        crate::visit::walk_machine(self, machine);
    }

    fn visit_branch(&mut self, branch: &Branch<'src>) {
        // Each branch has its own scope.
        self.push_scope();

        for pattern in &branch.patterns {
            let ident = &pattern.inner.ident;
            if ident.inner.0 != "_" {
                self.add_symbol(Symbol {
                    name: ident.inner.0.to_string(),
                    kind: SymbolKind::Parameter,
                    typ: type_to_string(&pattern.inner.ty.inner),
                    span: ident.span,
                });
            }
            self.analyze_type(&pattern.inner.ty);
        }

        crate::visit::walk_branch(self, branch);
        self.pop_scope();
    }

    fn visit_expr(&mut self, expr: &Spanned<Expr<'src>>) {
        match &expr.inner {
            Expr::Let { ident, ty, default: _ } => {
                self.add_symbol(Symbol {
                    name: ident.inner.0.to_string(),
                    kind: SymbolKind::LocalVariable,
                    typ: type_to_string(&ty.inner),
                    span: ident.span,
                });
                self.analyze_type(ty);
            }
            Expr::MethodCall { method, .. } => {
                self.add_symbol(Symbol {
                    name: method.inner.0.to_string(),
                    kind: SymbolKind::Method,
                    typ: "method".to_string(),
                    span: method.span,
                });
            }
            Expr::AtAccess { field, .. } | Expr::DotAccess { field, .. } => {
                self.add_symbol(Symbol {
                    name: field.inner.0.to_string(),
                    kind: SymbolKind::Field,
                    typ: "field".to_string(),
                    span: field.span,
                });
            }
            Expr::Var(name, ty) => {
                if *name != "self" {
                    let kind = self.lookup_variable_kind(name);
                    self.add_symbol(Symbol {
                        name: name.to_string(),
                        kind,
                        typ: type_to_string(ty),
                        span: expr.span,
                    });
                }
            }
            _ => {}
        }
        // Recurse into children regardless of variant.
        walk_expr(self, expr);
    }
}

impl Default for SemanticAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a Type to its string representation.
fn type_to_string(ty: &Type) -> String {
    match ty {
        Type::Named(ident) => ident.inner.0.to_string(),
        Type::Generic(ident, args) => {
            let args_str = args
                .iter()
                .map(|a| type_to_string(&a.inner))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{}({})", ident.inner.0, args_str)
        }
        Type::Path(ident, rest) => {
            format!("{}:{}", ident.inner.0, type_to_string(&rest.inner))
        }
        Type::Unit => "unit".to_string(),
        Type::Unknown => "?".to_string(),
        Type::Struct(fields) => {
            let fields_str = fields
                .iter()
                .map(|f| type_to_string(&f.inner))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{{ {} }}", fields_str)
        }
    }
}

/// Convenience function: analyze a Lake program and return semantic information.
pub fn analyze<'src>(program: &[Spanned<Item<'src>>]) -> SemanticInfo {
    SemanticAnalyzer::analyze(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::{parse, resolve};

    #[test]
    fn test_semantic_analysis() {
        let src = r#"
@rt(rt_write)

counter is {
  n i64 -> {
    let x i64 = 5
  }
}
"#;

        let ast = parse("test.lake", src).expect("parse failed");
        let resolved = resolve(ast);
        let info = analyze(&resolved);

        // Should find rt_write as runtime function
        assert!(info.is_runtime_function("rt_write"));

        // Should find counter machine
        assert!(info.machines.contains_key("counter"));

        // Symbols: rt_write, counter, n, i64 (param type), x, i64 (let type)
        assert_eq!(info.symbols.len(), 6);

        // Check symbol kinds
        assert_eq!(
            info.symbols_by_kind(SymbolKind::RuntimeFunction).len(),
            1
        );
        assert_eq!(info.symbols_by_kind(SymbolKind::Machine).len(), 1);
        assert_eq!(info.symbols_by_kind(SymbolKind::Parameter).len(), 1);
        assert_eq!(info.symbols_by_kind(SymbolKind::LocalVariable).len(), 1);
        assert_eq!(info.symbols_by_kind(SymbolKind::TypeName).len(), 2);
    }
}
