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

        let errors_before = self.diagnostics.error_count();

        // Validate require declarations against the extern registry
        for req in &program.requires {
            self.set_span(req.span);
            self.validate_require(&req.node);
        }

        // Build the namespace alias table from require declarations
        self.build_require_aliases(&program.requires);

        // Import resolution is handled by the Compiler builder (BFS queue
        // in process_source). The lowerer processes each file independently;
        // cross-file calls are resolved by the linker after IR merging.

        // Note: import vs require namespace clash detection is handled by
        // the Compiler builder (parse_source_tree), which has the loader's
        // namespace for each imported file.

        // Assign global slots (source order → 0..N) before name checks, so
        // function-vs-global clashes are caught via check_name_clash.
        self.collect_globals(&program.globals);

        // Validate function names for clashes (with intrinsics, externs, globals)
        self.check_function_names(&program.functions);

        // Collect user function param by-ref modes before lowering bodies,
        // so the lowerer can emit Reload after calls for by-ref args.
        for function in &program.functions {
            let modes: Vec<bool> = function.node.params.iter().map(|p| !p.is_value).collect();
            self.user_fn_params
                .insert(function.node.name.clone(), modes);
        }

        // Lower functions (may emit errors but we continue)
        for function in &program.functions {
            self.set_span(function.span);
            if let Some(func) = self.lower_function(&function.node) {
                functions.push(func);
            }
        }

        // Synthesize the `__init__` function that evaluates global initializers
        // in source order. Only emitted when the file declares globals.
        if !program.globals.is_empty()
            && let Some(init) = self.lower_init_function(&program.globals)
        {
            functions.push(init);
        }

        // If any errors were emitted, return None
        if self.diagnostics.error_count() > errors_before {
            return None;
        }

        Some(IrProgram {
            functions,
            global_count: program.globals.len(),
        })
    }

    /// Assign each file-scope global a slot (source order → 0..N) and check for
    /// name clashes (discard `_`, duplicates, intrinsics, merged externs).
    fn collect_globals(&mut self, globals: &[ast::Spanned<ast::GlobalVar>]) {
        for (i, g) in globals.iter().enumerate() {
            let name = &g.node.name;
            if name.0 == "_" {
                self.diagnostics.error(
                    DiagnosticCode::E400_DuplicateDefinition,
                    g.span,
                    "file-scope `let _` is not allowed; a global must be named".to_string(),
                );
                continue;
            }
            if self.global_slots.contains_key(name) {
                self.diagnostics.error(
                    DiagnosticCode::E400_DuplicateDefinition,
                    g.span,
                    format!("duplicate global `{}`", name),
                );
                continue;
            }
            if let Some(msg) = self.check_name_clash(name, "global") {
                self.diagnostics
                    .error(DiagnosticCode::E400_DuplicateDefinition, g.span, msg);
                continue;
            }
            self.global_slots.insert(name.clone(), i as u32);
        }
    }

    /// Lower the synthetic `__init__` function: evaluate each global's
    /// initializer in source order and store it into the global's slot.
    /// Absent initializers leave the slot Undefined (the reserved default).
    fn lower_init_function(&mut self, globals: &[ast::Spanned<ast::GlobalVar>]) -> Option<Function> {
        let errors_before = self.diagnostics.error_count();

        // Reset per-function state (same as lower_function)
        self.vars.clear();
        self.blocks.clear();
        self.next_var_id = 0;
        self.next_block_id = 0;
        self.next_slot_id = 0;
        self.loop_stack.clear();
        self.byref_param_vars.clear();

        self.push_scope();
        let entry_block = self.start_block();

        // Lower each initializer in source order. `init_slot_limit = i` makes
        // bare names resolve to globals and restricts visibility to globals
        // declared before slot `i` (forward/self references error).
        for (i, g) in globals.iter().enumerate() {
            self.set_span(g.span);
            self.init_slot_limit = Some(i as u32);
            let slot = self.global_slots.get(&g.node.name).copied();
            if let (Some(init), Some(slot)) = (&g.node.initializer, slot) {
                let value = self.lower_expression(init);
                self.emit_store_global(slot, value);
            }
        }
        self.init_slot_limit = None;

        self.finish_block(Terminator::Return { value: None });
        self.pop_scope();

        if self.diagnostics.error_count() > errors_before {
            return None;
        }

        let mut function = Function {
            name: ast::Identifier("__init__".to_string()),
            params: Vec::new(),
            rest_param: None,
            locals: std::mem::take(&mut self.vars),
            blocks: std::mem::take(&mut self.blocks),
            entry_block,
        };
        crate::ssa::promote(&mut function);
        Some(function)
    }

    /// Check all function names for clashes with intrinsics, global externs,
    /// merged externs (`as _`), file-scope globals, and duplicate definitions.
    fn check_function_names(&mut self, functions: &[ast::Spanned<ast::Function>]) {
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
    }

    /// Check a name against intrinsics and merged externs (`require ns as _`).
    ///
    /// Returns `Some(message)` if there's a clash, `None` if clean.
    fn check_name_clash(&self, name: &ast::Identifier, kind: &str) -> Option<String> {
        if is_reserved_name(name) {
            return Some(format!(
                "{} `{}` clashes with built-in intrinsic",
                kind, name
            ));
        }
        if let Some(ns) = self.merged_externs.get(name) {
            return Some(format!(
                "{} `{}` clashes with extern from namespace `{}`",
                kind, name, ns
            ));
        }
        // Globals are assigned before function/constant checks, so this catches
        // function-vs-global and constant-vs-global clashes. (For a global being
        // collected, its own name is not yet in the map — no false positive.)
        if self.global_slots.contains_key(name) {
            return Some(format!(
                "{} `{}` clashes with file-scope global of the same name",
                kind, name
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

            // Check against other merged externs
            use std::collections::hash_map::Entry;
            match self.merged_externs.entry(name) {
                Entry::Occupied(e) => {
                    self.diagnostics.error(
                        DiagnosticCode::E400_DuplicateDefinition,
                        span,
                        format!(
                            "extern function `{}` (from namespace `{}`) \
                             clashes with extern from namespace `{}`",
                            e.key(),
                            namespace,
                            e.get()
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
        self.byref_param_vars.clear();
        self.slot_decls.clear();

        // Start with a fresh scope for parameters
        self.push_scope();

        // Create entry block
        let entry_block = self.start_block();

        // Lower parameters
        let mut params = Vec::new();
        for param in &func.params {
            let var = self.new_var(param.name.clone(), TypeSet::any());
            self.bind(&param.name, var);
            // By-ref params are ref-backed: assignments emit WriteRef
            // (the caller created a MakeRef at the call site, so the
            // param's compiled slot is a Slot::Ref). The WriteRef is
            // side-effecting and survives mem2reg/DCE.
            if !param.is_value {
                let ref_origin = RefOrigin {
                    ref_var: var,
                    base_var: var,
                    key_var: None,
                    base_name: Some(param.name.clone()),
                };
                self.bind_ref(&param.name, ref_origin);
                self.byref_param_vars.insert(param.name.clone(), var);
            }
            params.push(var);
        }

        // Lower rest parameter if present
        let rest_param = if let Some(ref rest) = func.rest_param {
            let var = self.new_var(rest.name.clone(), TypeSet::single(types::BaseType::Array));
            self.bind(&rest.name, var);
            Some(var)
        } else {
            None
        };

        // Slots allocated from here on belong to the function body; slots below
        // this are parameters, which the unused-variable lint does not flag.
        self.body_slot_start = self.next_slot_id;

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

        // Unused-variable lint runs on the pre-SSA function (slot↔name map and
        // Read instructions still present), before SSA construction.
        self.check_unused_bindings(&function);

        // Convert pre-SSA Assign/Read instructions to proper SSA with Phi nodes
        crate::ssa::promote(&mut function);

        Some(function)
    }
}
