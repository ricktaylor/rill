//! Control Flow Lowering
//!
//! Handles if/while/loop/for/match expressions and pattern matching in conditions.

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Control Flow Lowering
    // ========================================================================

    /// Lower an if expression
    pub fn lower_if(
        &mut self,
        conditions: &[ast::IfCondition],
        then_block: &[ast::Stmt],
        then_expr: &Option<Box<ast::Expression>>,
        else_block: &Option<Vec<ast::Stmt>>,
        else_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
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
                    let cond_var = self.lower_expression(expr);
                    let next_bb = self.fresh_block();
                    self.finish_block(Terminator::If {
                        condition: cond_var,
                        then_target: next_bb,
                        else_target: else_bb,
                        span: self.current_span,
                    });
                    self.current_block = next_bb;
                    self.current_instructions = Vec::new();
                }

                ast::IfCondition::Let { pattern, value } => {
                    // Lower value and check if pattern matches
                    let value_var = self.lower_expression(value);
                    self.lower_if_pattern(pattern, value_var, BindingMode::Value, else_bb);
                }

                ast::IfCondition::With { pattern, value } => {
                    // Lower value and check if pattern matches (by-reference)
                    let value_var = self.lower_expression(value);
                    self.lower_if_pattern(pattern, value_var, BindingMode::Reference, else_bb);
                }
            }
        }

        // All conditions passed - execute then-block
        for stmt in then_block {
            self.lower_stmt(stmt);
        }
        let then_value = if let Some(expr) = then_expr {
            self.lower_expression(expr)
        } else {
            let dest = self.new_temp(TypeSet::empty());
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
                self.lower_stmt(stmt);
            }
        }
        let else_value = if let Some(expr) = else_expr {
            self.lower_expression(expr)
        } else {
            let dest = self.new_temp(TypeSet::empty());
            self.emit(Instruction::Undefined { dest });
            dest
        };
        let else_exit_block = self.current_block;
        self.pop_scope();
        self.finish_block(Terminator::Jump { target: join_bb });

        // Join block with phi
        self.current_block = join_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::all());
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(then_exit_block, then_value), (else_exit_block, else_value)],
        });

        result
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
    ) {
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
                        self.bind(name, dest);
                    }
                    BindingMode::Reference => {
                        self.bind(name, value);
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
                    let elem = self.new_temp(TypeSet::all());
                    self.emit(Instruction::Index {
                        dest: elem,
                        base: value,
                        key: idx,
                    });
                    // Recursively match element pattern
                    self.lower_if_pattern(elem_pat, elem, mode, else_bb);
                }
            }

            ast::Pattern::Literal(lit) => {
                // Match checks value AND rejects undefined (no Guard needed)
                let lit_pattern = self.ast_literal_to_ir_literal(lit);
                self.emit_match(value, MatchPattern::Literal(lit_pattern), else_bb);
            }

            ast::Pattern::Type { type_name, binding } => {
                // Match checks type AND rejects undefined (no Guard needed)
                if let Some(base_type) = self.type_name_to_base_type(type_name) {
                    self.emit_match(value, MatchPattern::Type(base_type), else_bb);
                } else {
                    // Unknown type - always fail to else
                    self.finish_block(Terminator::Jump { target: else_bb });
                    let unreachable_bb = self.fresh_block();
                    self.current_block = unreachable_bb;
                    self.current_instructions = Vec::new();
                    return;
                }

                // If there's a nested binding, process it
                if let Some(inner_pat) = binding {
                    self.lower_if_pattern(inner_pat.as_ref(), value, mode, else_bb);
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
                    let elem = self.new_temp(TypeSet::all());
                    self.emit(Instruction::Index {
                        dest: elem,
                        base: value,
                        key: idx,
                    });
                    self.lower_if_pattern(pat, elem, mode, else_bb);
                }

                // Compute length for rest and after patterns
                let length = self.emit_unary_call("len", value);

                // Bind rest variable as a zero-copy Sequence over the source array.
                // core::array_seq(array, start, end, mutable) -> Sequence(ArraySlice)
                // Mutability follows binding mode: with = mutable (write-back),
                // let = immutable (by-value iteration only).
                if let Some(rest_name) = rest {
                    let start = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: start,
                        value: Literal::UInt(before.len() as u64),
                    });

                    let after_len_val = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len_val,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let end = self.emit_binary_call("sub", length, after_len_val);

                    let is_mutable = self.new_temp(TypeSet::single(types::BaseType::Bool));
                    self.emit(Instruction::Const {
                        dest: is_mutable,
                        value: Literal::Bool(matches!(mode, BindingMode::Reference)),
                    });

                    let rest_val = self.new_temp(TypeSet::single(types::BaseType::Sequence));
                    self.emit(Instruction::Call {
                        dest: rest_val,
                        function: FunctionRef::core("array_seq"),
                        args: vec![
                            CallArg {
                                value,
                                by_ref: false,
                            },
                            CallArg {
                                value: start,
                                by_ref: false,
                            },
                            CallArg {
                                value: end,
                                by_ref: false,
                            },
                            CallArg {
                                value: is_mutable,
                                by_ref: false,
                            },
                        ],
                    });

                    let rest_var = self.new_var(
                        rest_name.clone(),
                        TypeSet::single(types::BaseType::Sequence),
                    );
                    self.emit(Instruction::Copy {
                        dest: rest_var,
                        src: rest_val,
                    });
                    self.bind(rest_name, rest_var);
                }

                // Bind after elements (from end, using len - after.len() + i)
                if !after.is_empty() {
                    let after_len_val = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len_val,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let after_start = self.emit_binary_call("sub", length, after_len_val);

                    for (i, pat) in after.iter().enumerate() {
                        let offset = self.new_temp(TypeSet::single(types::BaseType::UInt));
                        self.emit(Instruction::Const {
                            dest: offset,
                            value: Literal::UInt(i as u64),
                        });
                        let idx = self.emit_binary_call("add", after_start, offset);

                        let elem = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest: elem,
                            base: value,
                            key: idx,
                        });

                        self.lower_if_pattern(pat, elem, mode, else_bb);
                    }
                }
            }

            ast::Pattern::Map(entries) => {
                // Check it's a map, then destructure entries by key
                self.emit_match(value, MatchPattern::Type(types::BaseType::Map), else_bb);

                for (key_pat, val_pat) in entries {
                    let key_var = match &key_pat.node {
                        ast::Pattern::Literal(lit) => {
                            let lit_pattern = self.ast_literal_to_ir_literal(lit);
                            let k = self.new_temp(TypeSet::all());
                            self.emit(Instruction::Const {
                                dest: k,
                                value: lit_pattern,
                            });
                            k
                        }
                        ast::Pattern::Variable(name) => {
                            // Variable key: treat name as text key
                            let k = self.new_temp(TypeSet::single(types::BaseType::Text));
                            self.emit(Instruction::Const {
                                dest: k,
                                value: Literal::Text(name.to_string()),
                            });
                            k
                        }
                        _ => continue, // skip unsupported key patterns
                    };

                    let val = self.new_temp(TypeSet::all());
                    self.emit(Instruction::Index {
                        dest: val,
                        base: value,
                        key: key_var,
                    });

                    // Value must be present for the pattern to match
                    self.emit_guard(val, else_bb);
                    self.lower_if_pattern(val_pat, val, mode, else_bb);
                }
            }
        }
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
            span: self.current_span,
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
            span: self.current_span,
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
    /// Returns None and emits diagnostic for unknown types
    pub(crate) fn type_name_to_base_type(
        &mut self,
        name: &ast::Identifier,
    ) -> Option<types::BaseType> {
        match name.as_ref() {
            "Bool" => Some(types::BaseType::Bool),
            "UInt" => Some(types::BaseType::UInt),
            "Int" => Some(types::BaseType::Int),
            "Float" => Some(types::BaseType::Float),
            "Text" => Some(types::BaseType::Text),
            "Bytes" => Some(types::BaseType::Bytes),
            "Array" => Some(types::BaseType::Array),
            "Map" => Some(types::BaseType::Map),
            _ => {
                self.error_invalid_pattern(&format!("unknown type '{}'", name), self.current_span);
                None
            }
        }
    }

    /// Lower a while loop
    pub fn lower_while(
        &mut self,
        condition: &ast::Expression,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        // Jump to header
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: evaluate condition
        self.current_block = header_bb;
        self.current_instructions = Vec::new();
        let cond = self.lower_expression(condition);
        self.finish_block(Terminator::If {
            condition: cond,
            then_target: body_bb,
            else_target: exit_bb,
            span: self.current_span,
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
            self.lower_stmt(stmt);
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr);
        }

        self.loop_stack.pop();
        self.pop_scope();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Exit block
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        // While loops produce undefined (unless break with value)
        let result = self.new_temp(TypeSet::empty());
        self.emit(Instruction::Undefined { dest: result });
        result
    }

    /// Lower an infinite loop
    pub fn lower_loop(
        &mut self,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
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
            self.lower_stmt(stmt);
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr);
        }

        self.loop_stack.pop();
        self.pop_scope();

        self.finish_block(Terminator::Jump { target: body_bb });

        // Exit block (only reachable via break)
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::all());
        // TODO: Phi from break values
        self.emit(Instruction::Undefined { dest: result });
        result
    }

    /// Lower a for loop using index-based iteration.
    ///
    /// Lowers `for x in iterable { body }` to:
    /// ```text
    /// iter = iterable
    /// length = core::len(iter)
    /// i = 0
    /// header: if core::lt(i, length) -> body, exit
    /// body:   elem = iter[i]; bind x = elem; ... body ...; i = core::add(i, 1); jump header
    /// exit:   undefined
    /// ```
    pub fn lower_for(
        &mut self,
        binding_is_value: bool,
        binding: &ast::ForBinding,
        iterable: &ast::Expression,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
        let iter_var = self.lower_expression(iterable);

        // length = core::len(iter)
        let length = self.emit_unary_call("len", iter_var);

        // i = 0
        let i_init = self.new_temp(TypeSet::single(types::BaseType::UInt));
        self.emit(Instruction::Const {
            dest: i_init,
            value: Literal::UInt(0),
        });

        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        let pre_header_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: phi for index, then check i < length
        self.current_block = header_bb;
        self.current_instructions = Vec::new();

        // Phi for the loop index — sources filled in after body block is known
        let i_var = self.new_temp(TypeSet::single(types::BaseType::UInt));
        // Placeholder phi — we'll patch the body source after the body is lowered
        let i_phi_idx = self.current_instructions.len();
        self.emit(Instruction::Phi {
            dest: i_var,
            sources: vec![], // patched below
        });

        let has_more = self.emit_binary_call("lt", i_var, length);
        self.finish_block(Terminator::If {
            condition: has_more,
            then_target: body_bb,
            else_target: exit_bb,
            span: self.current_span,
        });

        // Body: index into iterable, bind variables, execute body, increment
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        // elem = iter[i]
        let elem = self.new_temp(TypeSet::all());
        self.emit(Instruction::Index {
            dest: elem,
            base: iter_var,
            key: i_var,
        });

        let mode = if binding_is_value {
            BindingMode::Value
        } else {
            BindingMode::Reference
        };

        // Bind loop variable(s)
        match binding {
            ast::ForBinding::Single(name) => match mode {
                BindingMode::Value => {
                    let var = self.new_var(name.clone(), TypeSet::all());
                    self.emit(Instruction::Copy {
                        dest: var,
                        src: elem,
                    });
                    self.bind(name, var);
                }
                BindingMode::Reference => {
                    self.bind(name, elem);
                }
            },
            ast::ForBinding::Pair(key_name, val_name) => {
                // First variable (key/index) is always by-value
                // For collections, i_var IS the index — bind it directly
                let key_var = self.new_var(key_name.clone(), TypeSet::all());
                self.emit(Instruction::Copy {
                    dest: key_var,
                    src: i_var,
                });
                self.bind(key_name, key_var);

                // Second variable (value/element) follows binding mode
                match mode {
                    BindingMode::Value => {
                        let var = self.new_var(val_name.clone(), TypeSet::all());
                        self.emit(Instruction::Copy {
                            dest: var,
                            src: elem,
                        });
                        self.bind(val_name, var);
                    }
                    BindingMode::Reference => {
                        self.bind(val_name, elem);
                    }
                }
            }
        }

        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: header_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_stmt(stmt);
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr);
        }

        self.loop_stack.pop();
        self.pop_scope();

        // i_next = core::add(i, 1)
        let one = self.new_temp(TypeSet::single(types::BaseType::UInt));
        self.emit(Instruction::Const {
            dest: one,
            value: Literal::UInt(1),
        });
        let i_next = self.emit_binary_call("add", i_var, one);

        let body_exit_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Patch the phi in the header block with both sources
        // Find the header block and update the phi instruction
        if let Some(header_block) = self.blocks.iter_mut().find(|b| b.id == header_bb)
            && let Some(phi_inst) = header_block.instructions.get_mut(i_phi_idx)
            && let Instruction::Phi { sources, .. } = &mut phi_inst.node
        {
            *sources = vec![(pre_header_bb, i_init), (body_exit_bb, i_next)];
        }

        // Exit
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::empty());
        self.emit(Instruction::Undefined { dest: result });
        result
    }

    /// Lower a match expression
    ///
    /// Uses a linear chain of pattern checks. Each arm tries its pattern
    /// against the scrutinee; on mismatch, control falls through to the next arm.
    /// This reuses `lower_if_pattern` which already handles all pattern types.
    pub fn lower_match(&mut self, value: &ast::Expression, arms: &[ast::MatchArm]) -> VarId {
        let scrutinee = self.lower_expression(value);
        let exit_bb = self.fresh_block();

        let mut arm_results: Vec<(BlockId, VarId)> = Vec::new();

        for arm in arms {
            let next_bb = self.fresh_block();

            // Determine binding mode from the arm
            let mode = if arm.binding_is_value {
                BindingMode::Value
            } else {
                BindingMode::Reference
            };

            // Push scope for pattern bindings
            self.push_scope();

            // Check pattern — on mismatch, jumps to next_bb
            self.lower_if_pattern(&arm.pattern, scrutinee, mode, next_bb);

            // Check guard if present
            if let Some(ref guard) = arm.guard {
                let guard_val = self.lower_expression(guard);
                let guard_pass_bb = self.fresh_block();
                self.finish_block(Terminator::If {
                    condition: guard_val,
                    then_target: guard_pass_bb,
                    else_target: next_bb,
                    span: self.current_span,
                });

                self.current_block = guard_pass_bb;
                self.current_instructions = Vec::new();
            }

            // Execute arm body
            for stmt in &arm.body {
                self.lower_stmt(stmt);
            }
            let arm_value = if let Some(ref expr) = arm.body_expr {
                self.lower_expression(expr)
            } else {
                let dest = self.new_temp(TypeSet::empty());
                self.emit(Instruction::Undefined { dest });
                dest
            };

            arm_results.push((self.current_block, arm_value));
            self.pop_scope();
            self.finish_block(Terminator::Jump { target: exit_bb });

            // Continue to next arm on pattern mismatch
            self.current_block = next_bb;
            self.current_instructions = Vec::new();
        }

        // Final fallthrough (unreachable if patterns are exhaustive)
        let fallback = self.new_temp(TypeSet::empty());
        self.emit(Instruction::Undefined { dest: fallback });
        arm_results.push((self.current_block, fallback));
        self.finish_block(Terminator::Jump { target: exit_bb });

        // Exit block with phi
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::all());
        self.emit(Instruction::Phi {
            dest: result,
            sources: arm_results,
        });
        result
    }

    /// Lower a `..` / `..=` expression as a call to `core::make_seq(start, end, inclusive)`.
    ///
    /// The builtin creates a Sequence value (lazy, O(1) memory). The `inclusive`
    /// flag is passed as a Bool argument so the builtin can handle both `..` and `..=`.
    pub fn lower_range(
        &mut self,
        start: &ast::Expression,
        end: &ast::Expression,
        inclusive: bool,
    ) -> VarId {
        let start_var = self.lower_expression(start);
        let end_var = self.lower_expression(end);
        let inclusive_var = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: inclusive_var,
            value: Literal::Bool(inclusive),
        });

        let dest = self.new_temp(TypeSet::single(types::BaseType::Sequence));
        self.emit(Instruction::Call {
            dest,
            function: FunctionRef::core("make_seq"),
            args: vec![
                CallArg {
                    value: start_var,
                    by_ref: false,
                },
                CallArg {
                    value: end_var,
                    by_ref: false,
                },
                CallArg {
                    value: inclusive_var,
                    by_ref: false,
                },
            ],
        });
        dest
    }
}
