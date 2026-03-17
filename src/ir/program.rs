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
        let mut imports = Vec::new();

        let errors_before = self.diagnostics.error_count();

        // Lower imports (imports can't fail currently)
        for import in &program.imports {
            imports.push(self.lower_import(&import.node));
        }

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
            imports,
        })
    }

    /// Lower an import declaration
    fn lower_import(&mut self, import: &ast::Import) -> Import {
        let namespace = match &import.alias {
            Some(alias) => alias.clone(),
            None => match &import.path {
                ast::ImportPath::Stdlib(parts) => parts
                    .last()
                    .cloned()
                    .unwrap_or(ast::Identifier("_".to_string())),
                ast::ImportPath::File(path) => {
                    let name = std::path::Path::new(path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("_");
                    ast::Identifier(name.to_string())
                }
            },
        };

        Import {
            namespace,
            path: import.path.clone(),
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
        self.loop_stack.clear();

        // Start with a fresh scope for parameters
        self.push_scope();

        // Create entry block
        let entry_block = self.start_block();

        // Lower parameters
        let mut params = Vec::new();
        for param in &func.params {
            let var = self.new_var(param.name.clone(), TypeSet::from_types(all_types()));
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

        Some(Function {
            name: func.name.clone(),
            params,
            rest_param,
            locals: std::mem::take(&mut self.vars),
            blocks: std::mem::take(&mut self.blocks),
            entry_block,
        })
    }
}
