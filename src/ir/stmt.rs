//! Statement Lowering

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Statement Lowering
    // ========================================================================

    /// Lower a statement
    ///
    /// Emits diagnostics on error and continues processing.
    pub fn lower_statement(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::VarDecl {
                pattern,
                initializer,
            } => {
                let value = self.lower_expression(initializer);
                self.lower_pattern_binding(&pattern.node, value, BindingMode::Value);
            }

            ast::Statement::With { pattern, value } => {
                let value_var = self.lower_expression(value);
                self.lower_pattern_binding(&pattern.node, value_var, BindingMode::Reference);
            }

            // Note: Assignment is now an Expression, not a Statement
            // It's handled in lower_expression via Expression::Assignment
            ast::Statement::Return { value } => {
                let var = value.as_ref().map(|e| self.lower_expression(e));
                self.finish_block(Terminator::Return { value: var });
                self.start_block();
            }

            ast::Statement::Expression(expr) => {
                self.lower_expression(expr);
            }

            ast::Statement::Break { value } => {
                if let Some(loop_ctx) = self.loop_stack.last() {
                    let break_target = loop_ctx.break_target;
                    let _break_value = value.as_ref().map(|e| self.lower_expression(e));
                    self.finish_block(Terminator::Jump {
                        target: break_target,
                    });
                    self.start_block();
                } else {
                    self.error_invalid_loop_control("break", dummy_span());
                    self.finish_block(Terminator::Return { value: None });
                    self.start_block();
                }
            }

            ast::Statement::Continue => {
                if let Some(loop_ctx) = self.loop_stack.last() {
                    let continue_target = loop_ctx.continue_target;
                    self.finish_block(Terminator::Jump {
                        target: continue_target,
                    });
                    self.start_block();
                } else {
                    self.error_invalid_loop_control("continue", dummy_span());
                    self.finish_block(Terminator::Return { value: None });
                    self.start_block();
                }
            }
        }
    }

    /// Lower an assignment expression
    /// Returns the VarId containing the assigned value (or undefined if lvalue invalid)
    pub fn lower_assignment(
        &mut self,
        target: &ast::Expression,
        op: &ast::AssignmentOp,
        value: &ast::Expression,
    ) -> VarId {
        match target {
            ast::Expression::Variable(name) => {
                let rhs = self.lower_expression(value);

                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    if let Some(lhs) = self.lookup(&name.0) {
                        self.lower_compound_op(lhs, op, rhs)
                    } else {
                        self.error_undefined_var(None, &name.0, dummy_span());
                        return self.error_placeholder();
                    }
                };

                let dest = self.new_temp(TypeSet::from_types(all_types()));
                self.emit(Instruction::Copy {
                    dest,
                    src: final_value,
                });
                self.bind(&name.0, dest);
                final_value
            }

            ast::Expression::ArrayAccess { array, index } => {
                let base = self.lower_expression(array);
                let idx = self.lower_expression(index);

                // Check if the slot exists by indexing first
                let slot_check = self.new_temp(TypeSet::all());
                self.emit(Instruction::Index {
                    dest: slot_check,
                    base,
                    key: idx,
                });

                // Short-circuit: only evaluate rhs if lvalue is defined
                let defined_bb = self.fresh_block();
                let undefined_bb = self.fresh_block();
                let join_bb = self.fresh_block();

                self.finish_block(Terminator::Guard {
                    value: slot_check,
                    defined: defined_bb,
                    undefined: undefined_bb,
                    span: dummy_span(),
                });

                // Defined path: evaluate rhs and perform assignment
                self.current_block = defined_bb;
                self.current_instructions = Vec::new();

                let rhs = self.lower_expression(value);
                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    self.lower_compound_op(slot_check, op, rhs)
                };

                self.emit(Instruction::SetIndex {
                    base,
                    key: idx,
                    value: final_value,
                });
                let defined_exit = self.current_block;
                self.finish_block(Terminator::Jump { target: join_bb });

                // Undefined path: skip rhs evaluation, return undefined
                self.current_block = undefined_bb;
                self.current_instructions = Vec::new();
                let undef_result = self.new_temp(TypeSet::empty());
                self.emit(Instruction::Undefined { dest: undef_result });
                self.finish_block(Terminator::Jump { target: join_bb });

                // Join with phi
                self.current_block = join_bb;
                self.current_instructions = Vec::new();
                let result = self.new_temp(TypeSet::all());
                self.emit(Instruction::Phi {
                    dest: result,
                    sources: vec![(defined_exit, final_value), (undefined_bb, undef_result)],
                });

                result
            }

            ast::Expression::MemberAccess { object, member } => {
                let base = self.lower_expression(object);
                let key = self.lower_expression(member);

                // Check if the slot exists by indexing first
                let slot_check = self.new_temp(TypeSet::all());
                self.emit(Instruction::Index {
                    dest: slot_check,
                    base,
                    key,
                });

                // Short-circuit: only evaluate rhs if lvalue is defined
                let defined_bb = self.fresh_block();
                let undefined_bb = self.fresh_block();
                let join_bb = self.fresh_block();

                self.finish_block(Terminator::Guard {
                    value: slot_check,
                    defined: defined_bb,
                    undefined: undefined_bb,
                    span: dummy_span(),
                });

                // Defined path: evaluate rhs and perform assignment
                self.current_block = defined_bb;
                self.current_instructions = Vec::new();

                let rhs = self.lower_expression(value);
                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    self.lower_compound_op(slot_check, op, rhs)
                };

                self.emit(Instruction::SetIndex {
                    base,
                    key,
                    value: final_value,
                });
                let defined_exit = self.current_block;
                self.finish_block(Terminator::Jump { target: join_bb });

                // Undefined path: skip rhs evaluation, return undefined
                self.current_block = undefined_bb;
                self.current_instructions = Vec::new();
                let undef_result = self.new_temp(TypeSet::empty());
                self.emit(Instruction::Undefined { dest: undef_result });
                self.finish_block(Terminator::Jump { target: join_bb });

                // Join with phi
                self.current_block = join_bb;
                self.current_instructions = Vec::new();
                let result = self.new_temp(TypeSet::all());
                self.emit(Instruction::Phi {
                    dest: result,
                    sources: vec![(defined_exit, final_value), (undefined_bb, undef_result)],
                });

                result
            }

            // Bit test as lvalue: x @ b = bool_value
            // Calls core.bit_set(x, b, value) which returns the new value or undefined
            ast::Expression::BinaryOp {
                left,
                op: ast::BinaryOperator::BitTest,
                right,
            } => {
                let base = self.lower_expression(left);
                let bit = self.lower_expression(right);

                // Check if the bit is accessible by testing first
                let bit_check = self.new_temp(TypeSet::all());
                self.emit(Instruction::Call {
                    dest: bit_check,
                    function: FunctionRef {
                        namespace: None,
                        name: ast::Identifier("core::bit_test".to_string()),
                    },
                    args: vec![
                        CallArg {
                            value: base,
                            by_ref: false,
                        },
                        CallArg {
                            value: bit,
                            by_ref: false,
                        },
                    ],
                });

                // Short-circuit: only evaluate rhs if bit is accessible
                let defined_bb = self.fresh_block();
                let undefined_bb = self.fresh_block();
                let join_bb = self.fresh_block();

                self.finish_block(Terminator::Guard {
                    value: bit_check,
                    defined: defined_bb,
                    undefined: undefined_bb,
                    span: dummy_span(),
                });

                // Defined path: evaluate rhs and perform bit set
                self.current_block = defined_bb;
                self.current_instructions = Vec::new();

                let rhs = self.lower_expression(value);
                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    // For compound assignment like x @ b ^= true
                    self.lower_compound_op(bit_check, op, rhs)
                };

                // Call core.bit_set to set or clear the bit
                let set_result = self.new_temp(TypeSet::all());
                self.emit(Instruction::Call {
                    dest: set_result,
                    function: FunctionRef {
                        namespace: None,
                        name: ast::Identifier("core::bit_set".to_string()),
                    },
                    args: vec![
                        CallArg {
                            value: base,
                            by_ref: true, // by-ref so we can modify the original
                        },
                        CallArg {
                            value: bit,
                            by_ref: false,
                        },
                        CallArg {
                            value: final_value,
                            by_ref: false,
                        },
                    ],
                });
                let defined_exit = self.current_block;
                self.finish_block(Terminator::Jump { target: join_bb });

                // Undefined path: skip rhs evaluation, return undefined
                self.current_block = undefined_bb;
                self.current_instructions = Vec::new();
                let undef_result = self.new_temp(TypeSet::empty());
                self.emit(Instruction::Undefined { dest: undef_result });
                self.finish_block(Terminator::Jump { target: join_bb });

                // Join with phi
                self.current_block = join_bb;
                self.current_instructions = Vec::new();
                let result = self.new_temp(TypeSet::all());
                self.emit(Instruction::Phi {
                    dest: result,
                    sources: vec![(defined_exit, set_result), (undefined_bb, undef_result)],
                });

                result
            }

            _ => {
                // Invalid lvalue - evaluate both sides but return undefined
                self.lower_expression(target);

                // TODO: Could emit a warning here for invalid lvalue
                self.lower_expression(value) // Return the value, though assignment didn't happen
            }
        }
    }

    /// Lower a compound assignment operator (+=, -=, etc.)
    fn lower_compound_op(&mut self, lhs: VarId, op: &ast::AssignmentOp, rhs: VarId) -> VarId {
        let builtin = match op {
            ast::AssignmentOp::Assign => unreachable!(),
            ast::AssignmentOp::AddAssign => "core::add",
            ast::AssignmentOp::SubAssign => "core::sub",
            ast::AssignmentOp::MulAssign => "core::mul",
            ast::AssignmentOp::DivAssign => "core::div",
            ast::AssignmentOp::ModAssign => "core::mod",
            ast::AssignmentOp::AndAssign => "core::bit_and",
            ast::AssignmentOp::OrAssign => "core::bit_or",
            ast::AssignmentOp::XorAssign => "core::bit_xor",
            ast::AssignmentOp::ShlAssign => "core::shl",
            ast::AssignmentOp::ShrAssign => "core::shr",
        };

        let dest = self.new_temp(TypeSet::from_types(all_types()));
        self.emit(Instruction::Call {
            dest,
            function: FunctionRef {
                namespace: None,
                name: ast::Identifier(builtin.to_string()),
            },
            args: vec![
                CallArg {
                    value: lhs,
                    by_ref: false,
                },
                CallArg {
                    value: rhs,
                    by_ref: false,
                },
            ],
        });
        dest
    }
}
