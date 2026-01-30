//! Control Flow Lowering
//!
//! Handles if/while/loop/for/match expressions and pattern matching in conditions.

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Control Flow Lowering
    // ========================================================================

    /// Lower an if expression
    pub(super) fn lower_if(
        &mut self,
        conditions: &[ast::IfCondition],
        then_block: &[ast::Stmt],
        then_expr: &Option<Box<ast::Expression>>,
        else_block: &Option<Vec<ast::Stmt>>,
        else_expr: &Option<Box<ast::Expression>>,
    ) -> Result<VarId> {
        let else_bb = self.fresh_block();
        let join_bb = self.fresh_block();

        // Push scope for condition bindings (visible in then-block)
        self.push_scope();

        // Process each condition in the chain
        // Each condition either continues to the next or jumps to else
        for condition in conditions {
            match condition {
                ast::IfCondition::Bool(expr) => {
                    // Lower boolean expression and branch
                    let cond_var = self.lower_expression(expr)?;
                    let next_bb = self.fresh_block();
                    self.finish_block(Terminator::If {
                        condition: cond_var,
                        then_target: next_bb,
                        else_target: else_bb,
                    });
                    self.current_block = next_bb;
                    self.current_instructions = Vec::new();
                }

                ast::IfCondition::Let { pattern, value } => {
                    // Lower value and check if pattern matches
                    let value_var = self.lower_expression(value)?;
                    self.lower_if_pattern(pattern, value_var, BindingMode::Value, else_bb)?;
                }

                ast::IfCondition::With { pattern, value } => {
                    // Lower value and check if pattern matches (by-reference)
                    let value_var = self.lower_expression(value)?;
                    self.lower_if_pattern(pattern, value_var, BindingMode::Reference, else_bb)?;
                }
            }
        }

        // All conditions passed - execute then-block
        for stmt in then_block {
            self.lower_statement(&stmt.node)?;
        }
        let then_value = if let Some(expr) = then_expr {
            self.lower_expression(expr)?
        } else {
            let dest = self.new_temp(TypeSet::undefined());
            self.emit(Instruction::Undefined { dest });
            dest
        };
        let then_exit_block = self.current_block;
        self.pop_scope(); // End condition bindings scope
        self.finish_block(Terminator::Jump { target: join_bb });

        // Else block
        self.current_block = else_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        if let Some(stmts) = else_block {
            for stmt in stmts {
                self.lower_statement(&stmt.node)?;
            }
        }
        let else_value = if let Some(expr) = else_expr {
            self.lower_expression(expr)?
        } else {
            let dest = self.new_temp(TypeSet::undefined());
            self.emit(Instruction::Undefined { dest });
            dest
        };
        let else_exit_block = self.current_block;
        self.pop_scope();
        self.finish_block(Terminator::Jump { target: join_bb });

        // Join block with phi
        self.current_block = join_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::from_types(all_types()).as_optional());
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(then_exit_block, then_value), (else_exit_block, else_value)],
        });

        Ok(result)
    }

    /// Lower a pattern match for if-let/if-with conditions
    /// On match: binds variables and continues to next instruction
    /// On mismatch: jumps to else_bb
    ///
    /// Optimization: Match terminators implicitly reject undefined values
    /// (they won't match any type pattern), so we only emit Guard when
    /// there's no subsequent Match (i.e., simple variable patterns).
    fn lower_if_pattern(
        &mut self,
        pattern: &ast::Pat,
        value: VarId,
        mode: BindingMode,
        else_bb: BlockId,
    ) -> Result<()> {
        match &pattern.node {
            ast::Pattern::Wildcard => {
                // Always matches, binds nothing
            }

            ast::Pattern::Variable(name) => {
                // Only presence check needed - no type constraint
                // Guard checks defined vs undefined
                self.emit_guard(value, else_bb);

                // Bind the variable
                match mode {
                    BindingMode::Value => {
                        let dest = self.new_var(name.clone(), TypeSet::from_types(all_types()));
                        self.emit(Instruction::Copy { dest, src: value });
                        self.bind(&name.0, dest);
                    }
                    BindingMode::Reference => {
                        self.bind(&name.0, value);
                    }
                }
            }

            ast::Pattern::Array(patterns) => {
                // Match checks type AND rejects undefined (no Guard needed)
                self.emit_match(value, MatchPattern::Array(patterns.len()), else_bb);

                // Bind each element
                for (i, elem_pat) in patterns.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });
                    let elem = self.new_temp(TypeSet::from_types(all_types()).as_optional());
                    self.emit(Instruction::Index {
                        dest: elem,
                        base: value,
                        key: idx,
                    });
                    // Recursively match element pattern
                    self.lower_if_pattern(elem_pat, elem, mode, else_bb)?;
                }
            }

            ast::Pattern::Literal(lit) => {
                // Match checks value AND rejects undefined (no Guard needed)
                let lit_pattern = self.ast_literal_to_ir_literal(lit);
                self.emit_match(value, MatchPattern::Literal(lit_pattern), else_bb);
            }

            ast::Pattern::Type { type_name, binding } => {
                // Match checks type AND rejects undefined (no Guard needed)
                let base_type = self.type_name_to_base_type(type_name)?;
                self.emit_match(value, MatchPattern::Type(base_type), else_bb);

                // If there's a nested binding, process it
                if let Some(inner_pat) = binding {
                    self.lower_if_pattern(inner_pat.as_ref(), value, mode, else_bb)?;
                }
            }

            ast::Pattern::ArrayRest {
                before,
                rest,
                after,
            } => {
                // Match checks min length AND rejects undefined (no Guard needed)
                let min_len = before.len() + after.len();
                self.emit_match(value, MatchPattern::ArrayMin(min_len), else_bb);

                // Bind before elements
                for (i, pat) in before.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });
                    let elem = self.new_temp(TypeSet::from_types(all_types()).as_optional());
                    self.emit(Instruction::Index {
                        dest: elem,
                        base: value,
                        key: idx,
                    });
                    self.lower_if_pattern(pat, elem, mode, else_bb)?;
                }

                // Bind rest if present (requires slice builtin - TODO)
                if let Some(rest_name) = rest {
                    // For now, bind undefined - proper impl needs array slice
                    let rest_var =
                        self.new_var(rest_name.clone(), TypeSet::single(types::BaseType::Array));
                    self.emit(Instruction::Undefined { dest: rest_var });
                    self.bind(&rest_name.0, rest_var);
                }

                // Bind after elements (from end) - TODO: needs len() builtin
                let _ = after;
            }

            ast::Pattern::Map(_entries) => {
                // TODO: Full map pattern matching
                // For now, just check it's a map
                self.emit_match(value, MatchPattern::Type(types::BaseType::Map), else_bb);
            }
        }
        Ok(())
    }

    /// Emit Guard terminator: check value is defined
    /// On defined: continues in new block
    /// On undefined: jumps to fail_bb
    fn emit_guard(&mut self, value: VarId, fail_bb: BlockId) {
        let ok_bb = self.fresh_block();
        self.finish_block(Terminator::Guard {
            value,
            defined: ok_bb,
            undefined: fail_bb,
        });
        self.current_block = ok_bb;
        self.current_instructions = Vec::new();
    }

    /// Emit Match terminator: check value matches pattern
    /// Match implicitly rejects undefined (won't match any pattern)
    /// On match: continues in new block
    /// On no match: jumps to fail_bb
    fn emit_match(&mut self, value: VarId, pattern: MatchPattern, fail_bb: BlockId) {
        let ok_bb = self.fresh_block();
        self.finish_block(Terminator::Match {
            value,
            arms: vec![(pattern, ok_bb)],
            default: fail_bb,
        });
        self.current_block = ok_bb;
        self.current_instructions = Vec::new();
    }

    /// Convert AST literal to IR literal for pattern matching
    fn ast_literal_to_ir_literal(&self, lit: &ast::Literal) -> Literal {
        match lit {
            ast::Literal::Bool(b) => Literal::Bool(*b),
            ast::Literal::UInt(n) => Literal::UInt(*n),
            ast::Literal::Int(n) => Literal::Int(*n),
            ast::Literal::Float(f) => Literal::Float(*f),
            ast::Literal::Text(s) => Literal::Text(s.clone()),
            ast::Literal::Bytes(b) => Literal::Bytes(b.clone()),
            // Array/Map literals can't be used in patterns directly
            ast::Literal::Array(_) | ast::Literal::Map(_) => Literal::Bool(false),
        }
    }

    /// Convert type name to BaseType
    fn type_name_to_base_type(&self, name: &ast::Identifier) -> Result<types::BaseType> {
        match name.0.as_str() {
            "Bool" => Ok(types::BaseType::Bool),
            "UInt" => Ok(types::BaseType::UInt),
            "Int" => Ok(types::BaseType::Int),
            "Float" => Ok(types::BaseType::Float),
            "Text" => Ok(types::BaseType::Text),
            "Bytes" => Ok(types::BaseType::Bytes),
            "Array" => Ok(types::BaseType::Array),
            "Map" => Ok(types::BaseType::Map),
            _ => Err(LowerError::SemanticError {
                message: format!("unknown type '{}'", name.0),
                span: dummy_span(),
            }),
        }
    }

    /// Lower a while loop
    pub(super) fn lower_while(
        &mut self,
        condition: &ast::Expression,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> Result<VarId> {
        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        // Jump to header
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: evaluate condition
        self.current_block = header_bb;
        self.current_instructions = Vec::new();
        let cond = self.lower_expression(condition)?;
        self.finish_block(Terminator::If {
            condition: cond,
            then_target: body_bb,
            else_target: exit_bb,
        });

        // Body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: header_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_statement(&stmt.node)?;
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr)?;
        }

        self.loop_stack.pop();
        self.pop_scope();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Exit block
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        // While loops produce undefined (unless break with value)
        let result = self.new_temp(TypeSet::undefined());
        self.emit(Instruction::Undefined { dest: result });
        Ok(result)
    }

    /// Lower an infinite loop
    pub(super) fn lower_loop(
        &mut self,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> Result<VarId> {
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        self.finish_block(Terminator::Jump { target: body_bb });

        // Body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: body_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_statement(&stmt.node)?;
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr)?;
        }

        self.loop_stack.pop();
        self.pop_scope();

        self.finish_block(Terminator::Jump { target: body_bb });

        // Exit block (only reachable via break)
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::from_types(all_types()).as_optional());
        // TODO: Phi from break values
        self.emit(Instruction::Undefined { dest: result });
        Ok(result)
    }

    /// Lower a for loop
    pub(super) fn lower_for(
        &mut self,
        _binding_is_value: bool,
        binding: &ast::ForBinding,
        iterable: &ast::Expression,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> Result<VarId> {
        // TODO: Proper iterator protocol
        // For now, just a placeholder that evaluates the iterable

        let _iter = self.lower_expression(iterable)?;

        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: check if more elements
        self.current_block = header_bb;
        self.current_instructions = Vec::new();
        // TODO: Actual iteration check
        let has_more = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: has_more,
            value: Literal::Bool(false), // Exit immediately for now
        });
        self.finish_block(Terminator::If {
            condition: has_more,
            then_target: body_bb,
            else_target: exit_bb,
        });

        // Body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        // Bind loop variable
        match binding {
            ast::ForBinding::Variable(name) => {
                let var = self.new_temp(TypeSet::from_types(all_types()).as_optional());
                self.emit(Instruction::Undefined { dest: var });
                self.bind(&name.0, var);
            }
            ast::ForBinding::Array(names) => {
                for name in names {
                    let var = self.new_temp(TypeSet::from_types(all_types()).as_optional());
                    self.emit(Instruction::Undefined { dest: var });
                    self.bind(&name.0, var);
                }
            }
        }

        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: header_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_statement(&stmt.node)?;
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr)?;
        }

        self.loop_stack.pop();
        self.pop_scope();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Exit
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::undefined());
        self.emit(Instruction::Undefined { dest: result });
        Ok(result)
    }

    /// Lower a match expression
    pub(super) fn lower_match(
        &mut self,
        value: &ast::Expression,
        arms: &[ast::MatchArm],
    ) -> Result<VarId> {
        let _scrutinee = self.lower_expression(value)?;
        let exit_bb = self.fresh_block();

        // TODO: Proper match compilation with decision trees
        // For now, just a linear chain of checks

        let mut arm_results: Vec<(BlockId, VarId)> = Vec::new();

        for arm in arms {
            let arm_bb = self.fresh_block();
            let next_bb = self.fresh_block();

            // Check pattern (simplified - just jump to arm for now)
            // TODO: Actual pattern matching
            self.finish_block(Terminator::Jump { target: arm_bb });

            // Arm body
            self.current_block = arm_bb;
            self.current_instructions = Vec::new();
            self.push_scope();

            // TODO: Bind pattern variables

            // Check guard if present
            if let Some(ref guard) = arm.guard {
                let guard_val = self.lower_expression(guard)?;
                let guard_pass_bb = self.fresh_block();
                self.finish_block(Terminator::If {
                    condition: guard_val,
                    then_target: guard_pass_bb,
                    else_target: next_bb,
                });

                self.current_block = guard_pass_bb;
                self.current_instructions = Vec::new();
            }

            for stmt in &arm.body {
                self.lower_statement(&stmt.node)?;
            }
            let arm_value = if let Some(ref expr) = arm.body_expr {
                self.lower_expression(expr)?
            } else {
                let dest = self.new_temp(TypeSet::undefined());
                self.emit(Instruction::Undefined { dest });
                dest
            };

            arm_results.push((self.current_block, arm_value));
            self.pop_scope();
            self.finish_block(Terminator::Jump { target: exit_bb });

            self.current_block = next_bb;
            self.current_instructions = Vec::new();
        }

        // Final fallthrough (should be unreachable if patterns are exhaustive)
        let fallback = self.new_temp(TypeSet::undefined());
        self.emit(Instruction::Undefined { dest: fallback });
        arm_results.push((self.current_block, fallback));
        self.finish_block(Terminator::Jump { target: exit_bb });

        // Exit block with phi
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::from_types(all_types()).as_optional());
        self.emit(Instruction::Phi {
            dest: result,
            sources: arm_results,
        });
        Ok(result)
    }

    /// Lower a range expression
    pub(super) fn lower_range(
        &mut self,
        start: &ast::Expression,
        end: &ast::Expression,
        _inclusive: bool,
    ) -> Result<VarId> {
        // TODO: Ranges should produce lazy iterators or arrays
        // For now, just evaluate bounds and return undefined
        let _start = self.lower_expression(start)?;
        let _end = self.lower_expression(end)?;

        let result = self.new_temp(TypeSet::single(types::BaseType::Array));
        self.emit(Instruction::Undefined { dest: result });
        Ok(result)
    }
}
