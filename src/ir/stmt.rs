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
                // Extract ref origin from the value expression if it's indexed access.
                // This enables write-back: `with x = arr[i]; x = 10` → arr[i] = 10.
                let (value_var, ref_origin) = self.lower_ref_expression(value);
                self.lower_pattern_binding_ref(
                    &pattern.node,
                    value_var,
                    BindingMode::Reference,
                    ref_origin,
                );
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
                    let break_value = value.as_ref().map(|e| self.lower_expression(e));
                    if let Some(val) = break_value {
                        let from_block = self.current_block;
                        self.loop_stack
                            .last_mut()
                            .unwrap()
                            .break_values
                            .push((from_block, val));
                    }
                    self.finish_block(Terminator::Jump {
                        target: break_target,
                    });
                    self.start_block();
                } else {
                    self.error_invalid_loop_control("break", self.current_span);
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
                    self.error_invalid_loop_control("continue", self.current_span);
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
                    if let Some(lhs) = self.lookup(name) {
                        self.lower_compound_op(lhs, op, rhs)
                    } else {
                        self.error_undefined_var(None, name, self.current_span);
                        return self.error_placeholder();
                    }
                };

                // If this variable is ref-backed, emit WriteRef to write
                // the new value back to the source location.
                if let Some(origin) = self.lookup_ref(name).cloned() {
                    self.emit(Instruction::WriteRef {
                        ref_var: origin.ref_var,
                        value: final_value,
                    });
                }

                // SSA: create a new VarId for each assignment, rebind the name.
                // Loop-carried variables are handled by phi nodes constructed
                // in the while/loop lowering.
                let dest = self.new_temp(TypeSet::from_types(all_types()));
                self.emit(Instruction::Copy {
                    dest,
                    src: final_value,
                });
                self.bind(name, dest);
                final_value
            }

            ast::Expression::ArrayAccess { array, index } => {
                let base = self.lower_expression(array);
                let key = self.lower_expression(index);
                self.lower_indexed_assignment(base, key, op, value)
            }

            ast::Expression::MemberAccess { object, member } => {
                let base = self.lower_expression(object);
                let key = self.lower_expression(member);
                self.lower_indexed_assignment(base, key, op, value)
            }

            // Bit test as lvalue: x @ b = bool_value
            // Uses BitSet intrinsic which returns the new value or undefined
            ast::Expression::BinaryOp {
                left,
                op: ast::BinaryOperator::BitTest,
                right,
            } => {
                let base = self.lower_expression(left);
                let bit = self.lower_expression(right);

                // Check if the bit is accessible by testing first
                let bit_check = self.emit_binary_intrinsic(IntrinsicOp::BitTest, base, bit);

                // Short-circuit: only evaluate rhs if bit is accessible
                let defined_bb = self.fresh_block();
                let undefined_bb = self.fresh_block();
                let join_bb = self.fresh_block();

                self.finish_block(Terminator::Guard {
                    value: bit_check,
                    defined: defined_bb,
                    undefined: undefined_bb,
                    span: self.current_span,
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

                // Use BitSet intrinsic to set or clear the bit
                let set_result = self.new_temp(TypeSet::uint());
                self.emit(Instruction::Intrinsic {
                    dest: set_result,
                    op: IntrinsicOp::BitSet,
                    args: vec![base, bit, final_value],
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
        let intrinsic = op
            .intrinsic_op()
            .expect("plain Assign should not reach lower_compound_op");

        self.emit_binary_intrinsic(intrinsic, lhs, rhs)
    }

    /// Lower assignment to an indexed location (arr[i] or obj.field).
    ///
    /// Guards on the slot existing, evaluates rhs only if defined,
    /// performs SetIndex, and joins with a phi.
    fn lower_indexed_assignment(
        &mut self,
        base: VarId,
        key: VarId,
        op: &ast::AssignmentOp,
        value: &ast::Expression,
    ) -> VarId {
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
            span: self.current_span,
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
}
