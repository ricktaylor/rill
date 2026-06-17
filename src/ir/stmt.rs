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
                // An absent initializer (`let x;`) binds to Undefined.
                let value = match initializer {
                    Some(init) => self.lower_expression(init),
                    None => self.emit_undefined(),
                };
                // Point binding diagnostics (unused-variable) at the pattern,
                // not the initializer expression just lowered above.
                self.set_span(pattern.span);
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
        target: &ast::Expr,
        op: &ast::AssignmentOp,
        value: &ast::Expr,
    ) -> VarId {
        match &target.node {
            ast::Expression::Variable(name) => {
                let rhs = self.lower_expression(value);

                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    if let Some(lhs) = self.read_var(name) {
                        self.lower_guarded_expression(|s: &mut Self| {
                            s.lower_compound_op(lhs, op, rhs)
                        })
                    } else {
                        self.error_undefined_var(None, name, self.current_span);
                        return self.error_placeholder();
                    }
                };

                // If this variable is ref-backed, emit the appropriate
                // write-back instruction, then Reload the base for SSA.
                if let Some(origin) = self.lookup_ref(name).cloned() {
                    if let Some(key_var) = origin.key_var {
                        // Accessor (far ref): direct element write
                        self.emit(Instruction::WriteAccessor {
                            base: origin.base_var,
                            key: key_var,
                            value: final_value,
                        });
                    } else {
                        // Ref (near ref): write through the ref binding
                        self.emit(Instruction::WriteRef {
                            ref_var: origin.ref_var,
                            value: final_value,
                        });
                    }
                    // Reload the base so SSA models the mutation
                    if let Some(base_name) = &origin.base_name {
                        let reloaded = self.emit_reload(origin.base_var);
                        self.reassign(base_name, reloaded);
                    }
                }

                // Reassign the variable via its slot. mem2reg handles phis.
                self.reassign(name, final_value);
                final_value
            }

            ast::Expression::GlobalAccess(name) => {
                let slot = match self.resolve_global_slot(name) {
                    Some(s) => s,
                    None => {
                        self.error_undefined_var(None, name, self.current_span);
                        return self.error_placeholder();
                    }
                };
                let rhs = self.lower_expression(value);
                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    let lhs = self.emit_load_global(slot);
                    self.lower_guarded_expression(|s: &mut Self| s.lower_compound_op(lhs, op, rhs))
                };
                self.emit_store_global(slot, final_value);
                final_value
            }

            ast::Expression::ArrayAccess { array, index } => {
                let base_name = if let ast::Expression::Variable(name) = &array.node {
                    Some(name.clone())
                } else {
                    None
                };
                let base = self.lower_expression(array);
                let key = self.lower_expression(index);
                self.lower_indexed_assignment(base, key, op, value, base_name)
            }

            ast::Expression::MemberAccess { object, member } => {
                let base_name = if let ast::Expression::Variable(name) = &object.node {
                    Some(name.clone())
                } else {
                    None
                };
                let base = self.lower_expression(object);
                let key = self.lower_expression(member);
                self.lower_indexed_assignment(base, key, op, value, base_name)
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

                self.finish_block(Terminator::Match {
                    value: bit_check,
                    arms: vec![(MatchPattern::Type(types::BaseType::Undefined), undefined_bb)],
                    default: defined_bb,
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
                }); // BitSet is ternary — no helper for 3-arg intrinsics
                let defined_exit = self.current_block;
                self.finish_block(Terminator::Jump { target: join_bb });

                // Undefined path: skip rhs evaluation, return undefined
                self.current_block = undefined_bb;
                self.current_instructions = Vec::new();
                let undef_result = self.emit_undefined();
                self.finish_block(Terminator::Jump { target: join_bb });

                // Join with phi
                self.current_block = join_bb;
                self.current_instructions = Vec::new();
                self.emit_phi(vec![
                    (defined_exit, set_result),
                    (undefined_bb, undef_result),
                ])
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
    /// All collection mutations go through MakeAccessor + WriteRef + Reload.
    /// This makes every mutation visible to SSA. The peephole layer can
    /// fuse the pattern into a single write-back closure at code generation.
    ///
    /// For compound assignment (`+=`, `-=`, etc.): guards on the slot
    /// existing first (needs the current value for the operation).
    fn lower_indexed_assignment(
        &mut self,
        base: VarId,
        key: VarId,
        op: &ast::AssignmentOp,
        value: &ast::Expr,
        base_name: Option<ast::Identifier>,
    ) -> VarId {
        if matches!(op, ast::AssignmentOp::Assign) {
            // Plain assignment via Accessor.
            let rhs = self.lower_expression(value);
            let acc = self.new_temp(TypeSet::any());
            self.emit(Instruction::MakeAccessor {
                dest: acc,
                base,
                key,
            });
            self.emit(Instruction::WriteAccessor {
                base,
                key,
                value: rhs,
            });
            // Reload the base so SSA models the mutation
            if let Some(name) = &base_name {
                let reloaded = self.emit_reload(base);
                self.reassign(name, reloaded);
            }
            rhs
        } else {
            // Compound assignment: need the current value for the operation.
            // Guard on the slot existing first.
            let slot_check = self.emit_index(base, key);

            let defined_bb = self.fresh_block();
            let undefined_bb = self.fresh_block();
            let join_bb = self.fresh_block();

            self.finish_block(Terminator::Match {
                value: slot_check,
                arms: vec![(MatchPattern::Type(types::BaseType::Undefined), undefined_bb)],
                default: defined_bb,
                span: self.current_span,
            });

            // Defined path: evaluate rhs, apply compound op, write
            self.current_block = defined_bb;
            self.current_instructions = Vec::new();

            let rhs = self.lower_expression(value);
            let final_value = self.lower_compound_op(slot_check, op, rhs);

            let acc = self.new_temp(TypeSet::any());
            self.emit(Instruction::MakeAccessor {
                dest: acc,
                base,
                key,
            });
            self.emit(Instruction::WriteAccessor {
                base,
                key,
                value: final_value,
            });
            // Reload the base so SSA models the mutation
            if let Some(name) = &base_name {
                let reloaded = self.emit_reload(base);
                self.reassign(name, reloaded);
            }
            let defined_exit = self.current_block;
            self.finish_block(Terminator::Jump { target: join_bb });

            // Undefined path: skip, return undefined
            self.current_block = undefined_bb;
            self.current_instructions = Vec::new();
            let undef_result = self.emit_undefined();
            self.finish_block(Terminator::Jump { target: join_bb });

            // Join with phi
            self.current_block = join_bb;
            self.current_instructions = Vec::new();
            self.emit_phi(vec![
                (defined_exit, final_value),
                (undefined_bb, undef_result),
            ])
        }
    }
}
