//! Program and Function Lowering

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Program Lowering
    // ========================================================================

    /// Lower an entire program
    pub fn lower_program(&mut self, program: &ast::Program) -> Result<Program> {
        let mut functions = Vec::new();
        let mut constants = Vec::new();
        let mut imports = Vec::new();

        // Lower imports
        for import in &program.imports {
            imports.push(self.lower_import(&import.node)?);
        }

        // Lower constants
        for constant in &program.constants {
            let bindings = self.lower_constant(&constant.node)?;
            constants.extend(bindings);
        }

        // Lower functions
        for function in &program.functions {
            functions.push(self.lower_function(&function.node)?);
        }

        Ok(Program {
            functions,
            constants,
            imports,
        })
    }

    /// Lower an import declaration
    fn lower_import(&mut self, import: &ast::Import) -> Result<Import> {
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

        Ok(Import {
            namespace,
            path: import.path.clone(),
        })
    }

    // ========================================================================
    // Function Lowering
    // ========================================================================

    /// Lower a function definition
    pub(super) fn lower_function(&mut self, func: &ast::Function) -> Result<Function> {
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
            self.bind(&param.name.0, var);
            params.push(Param {
                var,
                by_ref: !param.is_value,
            });
        }

        // Lower rest parameter if present
        let rest_param = if let Some(ref rest) = func.rest_param {
            let var = self.new_var(rest.name.clone(), TypeSet::single(types::BaseType::Array));
            self.bind(&rest.name.0, var);
            Some(Param {
                var,
                by_ref: !rest.is_value,
            })
        } else {
            None
        };

        // Lower statements
        for stmt in &func.statements {
            self.lower_statement(&stmt.node)?;
        }

        // Lower final expression if present
        let final_value = if let Some(ref expr) = func.final_expr {
            Some(self.lower_expression(expr)?)
        } else {
            None
        };

        // Terminate with return
        self.finish_block(Terminator::Return { value: final_value });

        // Pop function scope
        self.pop_scope();

        // Extract attributes
        let attributes: Vec<ast::Attribute> =
            func.attributes.iter().map(|a| a.node.clone()).collect();

        Ok(Function {
            name: func.name.clone(),
            attributes,
            params,
            rest_param,
            locals: std::mem::take(&mut self.vars),
            blocks: std::mem::take(&mut self.blocks),
            entry_block,
        })
    }
}
