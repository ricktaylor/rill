//! Program and Function Lowering

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Program Lowering
    // ========================================================================

    /// Lower an entire program
    ///
    /// Returns `Some(IrProgram)` if lowering succeeded, `None` if there were errors.
    /// Errors are emitted to the diagnostics accumulator.
    pub fn lower_program(&mut self, program: &ast::AstProgram) -> Option<IrProgram> {
        let mut functions = Vec::new();
        let mut constants = Vec::new();

        let errors_before = self.diagnostics.error_count();

        // Validate require declarations against the extern registry
        for req in &program.requires {
            self.set_span(req.span);
            self.validate_require(&req.node);
        }

        // Build the namespace alias table from require declarations
        self.build_require_aliases(&program.requires);

        // Note: imports (source files) are not yet implemented — they are
        // collected but not loaded. Phase 2 of the module system will add
        // SourceLoader support.

        // Validate function and constant names for clashes
        let function_names = self.check_function_names(&program.functions);
        self.check_constant_names(&program.constants, &function_names);

        // Lower constants (may emit errors but we continue)
        for constant in &program.constants {
            self.set_span(constant.span);
            if let Some(bindings) = self.lower_constant(&constant.node) {
                constants.extend(bindings);
            }
        }

        // Lower functions (may emit errors but we continue)
        for function in &program.functions {
            self.set_span(function.span);
            if let Some(func) = self.lower_function(&function.node) {
                functions.push(func);
            }
        }

        // If any errors were emitted, return None
        if self.diagnostics.error_count() > errors_before {
            return None;
        }

        Some(IrProgram {
            functions,
            constants,
        })
    }

    /// Check all function names for clashes with intrinsics, global externs,
    /// merged externs (`as _`), and duplicate definitions.
    ///
    /// Returns the set of valid function names (for cross-checking with constants).
    fn check_function_names(
        &mut self,
        functions: &[ast::Spanned<ast::Function>],
    ) -> HashMap<ast::Identifier, ast::Span> {
        let mut seen: HashMap<ast::Identifier, ast::Span> = HashMap::new();

        for func in functions {
            let name = &func.node.name;
            let span = func.span;

            if let Some(msg) = self.check_name_clash(name, "function") {
                self.diagnostics
                    .error(DiagnosticCode::E400_DuplicateDefinition, span, msg);
                continue;
            }

            if let Some(prev_span) = seen.get(name) {
                self.diagnostics
                    .error(
                        DiagnosticCode::E400_DuplicateDefinition,
                        span,
                        format!("duplicate function `{}`", name),
                    )
                    .note(*prev_span, "previously defined here");
                continue;
            }

            seen.insert(name.clone(), span);
        }

        seen
    }

    /// Check all constant names for clashes with intrinsics, global externs,
    /// merged externs, function names, and duplicate definitions.
    fn check_constant_names(
        &mut self,
        constants: &[ast::Spanned<ast::Constant>],
        function_names: &HashMap<ast::Identifier, ast::Span>,
    ) {
        for constant in constants {
            self.check_constant_pattern_names(
                &constant.node.pattern.node,
                constant.span,
                function_names,
            );
        }
    }

    /// Check a single constant pattern for name clashes.
    fn check_constant_pattern_names(
        &mut self,
        pattern: &ast::Pattern,
        span: ast::Span,
        function_names: &HashMap<ast::Identifier, ast::Span>,
    ) {
        match pattern {
            ast::Pattern::Variable(name) => {
                if let Some(msg) = self.check_name_clash(name, "constant") {
                    self.diagnostics
                        .error(DiagnosticCode::E400_DuplicateDefinition, span, msg);
                } else if let Some(fn_span) = function_names.get(name) {
                    self.diagnostics
                        .error(
                            DiagnosticCode::E400_DuplicateDefinition,
                            span,
                            format!("constant `{}` clashes with function of the same name", name),
                        )
                        .note(*fn_span, "function defined here");
                }
            }
            ast::Pattern::Array(pats) => {
                for pat in pats {
                    self.check_constant_pattern_names(&pat.node, span, function_names);
                }
            }
            ast::Pattern::ArrayRest {
                before,
                rest,
                after,
            } => {
                for pat in before.iter().chain(after.iter()) {
                    self.check_constant_pattern_names(&pat.node, span, function_names);
                }
                if let Some(name) = rest {
                    self.check_constant_pattern_names(
                        &ast::Pattern::Variable(name.clone()),
                        span,
                        function_names,
                    );
                }
            }
            ast::Pattern::Map(entries) => {
                for (_, val_pat) in entries {
                    self.check_constant_pattern_names(&val_pat.node, span, function_names);
                }
            }
            // Wildcards, literals, type patterns — no names to check
            _ => {}
        }
    }

    /// Check a name against intrinsics, global externs, and merged externs.
    ///
    /// Returns `Some(message)` if there's a clash, `None` if clean.
    fn check_name_clash(&self, name: &ast::Identifier, kind: &str) -> Option<String> {
        if is_reserved_name(name) {
            return Some(format!(
                "{} `{}` clashes with built-in intrinsic",
                kind, name
            ));
        }
        if self.externs.contains(name) {
            return Some(format!("{} `{}` clashes with global extern", kind, name));
        }
        if let Some(ns) = self.merged_externs.get(name) {
            return Some(format!(
                "{} `{}` clashes with extern from namespace `{}`",
                kind, name, ns
            ));
        }
        None
    }

    /// Validate a `require` declaration against the extern registry.
    fn validate_require(&mut self, req: &ast::Require) {
        if !self.externs.has_namespace(&req.namespace) {
            self.diagnostics.error(
                DiagnosticCode::E500_UndefinedExternal,
                self.current_span,
                format!(
                    "extern namespace `{}` not provided by embedder",
                    req.namespace
                ),
            );
        }
    }

    /// Build the alias table from require declarations.
    ///
    /// Maps call-site alias → extern namespace name. Detects duplicate aliases.
    fn build_require_aliases(&mut self, requires: &[ast::Spanned<ast::Require>]) {
        for req in requires {
            let alias = match &req.node.alias {
                Some(a) if a == "_" => {
                    // `as _` — merge all functions into root scope (no namespace)
                    // Register each function in the namespace as a "global-like" lookup
                    self.require_merge_to_root(&req.node.namespace, req.span);
                    continue;
                }
                Some(a) => a.clone(),
                None => req.node.namespace.clone(),
            };

            if self.require_aliases.contains_key(&alias) {
                self.diagnostics.error(
                    DiagnosticCode::E400_DuplicateDefinition,
                    req.span,
                    format!("duplicate namespace alias `{}`", alias),
                );
                continue;
            }

            self.require_aliases
                .insert(alias, req.node.namespace.clone());
        }
    }

    /// Merge all functions from a required namespace into the root scope.
    ///
    /// Called when `require ns as _;` is used. Functions become available
    /// unqualified.
    fn require_merge_to_root(&mut self, namespace: &ast::Identifier, span: ast::Span) {
        for (name, _) in self.externs.namespace_iter(namespace) {
            let name = ast::Identifier(name.clone());
            use std::collections::hash_map::Entry;
            match self.merged_externs.entry(name) {
                Entry::Occupied(e) => {
                    self.diagnostics.error(
                        DiagnosticCode::E400_DuplicateDefinition,
                        span,
                        format!(
                            "extern function `{}` (from namespace `{}`) clashes with another definition",
                            e.key(), namespace
                        ),
                    );
                }
                Entry::Vacant(e) => {
                    e.insert(namespace.clone());
                }
            }
        }
    }

    // ========================================================================
    // Function Lowering
    // ========================================================================

    /// Lower a function definition
    ///
    /// Returns `Some(Function)` if lowering succeeded, `None` if there were errors.
    pub fn lower_function(&mut self, func: &ast::Function) -> Option<Function> {
        let errors_before = self.diagnostics.error_count();

        // Reset per-function state
        self.vars.clear();
        self.blocks.clear();
        self.next_var_id = 0;
        self.next_block_id = 0;
        self.next_slot_id = 0;
        self.loop_stack.clear();

        // Start with a fresh scope for parameters
        self.push_scope();

        // Create entry block
        let entry_block = self.start_block();

        // Lower parameters
        let mut params = Vec::new();
        for param in &func.params {
            let var = self.new_var(param.name.clone(), TypeSet::all());
            self.bind(&param.name, var);
            params.push(Param {
                var,
                by_ref: !param.is_value,
            });
        }

        // Lower rest parameter if present
        let rest_param = if let Some(ref rest) = func.rest_param {
            let var = self.new_var(rest.name.clone(), TypeSet::single(types::BaseType::Array));
            self.bind(&rest.name, var);
            Some(Param {
                var,
                by_ref: !rest.is_value,
            })
        } else {
            None
        };

        // Lower statements (continue even on errors to report multiple issues)
        for stmt in &func.statements {
            self.lower_stmt(stmt);
        }

        // Lower final expression if present
        let final_value = func
            .final_expr
            .as_ref()
            .map(|expr| self.lower_expression(expr));

        // Terminate with return
        self.finish_block(Terminator::Return { value: final_value });

        // Pop function scope
        self.pop_scope();

        // If any errors were emitted, return None
        if self.diagnostics.error_count() > errors_before {
            return None;
        }

        let mut function = Function {
            name: func.name.clone(),
            params,
            rest_param,
            locals: std::mem::take(&mut self.vars),
            blocks: std::mem::take(&mut self.blocks),
            entry_block,
        };

        // Convert pre-SSA Assign/Read instructions to proper SSA with Phi nodes
        crate::ssa::promote(&mut function);

        Some(function)
    }
}
