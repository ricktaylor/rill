//! Statement Lowering

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Statement Lowering
    // ========================================================================

    /// Lower a statement
    pub(super) fn lower_statement(&mut self, stmt: &ast::Statement) -> Result<()> {
        match stmt {
            ast::Statement::VarDecl {
                pattern,
                initializer,
            } => {
                let value = self.lower_expression(initializer)?;
                self.lower_pattern_binding(&pattern.node, value, BindingMode::Value)?;
            }

            ast::Statement::With { pattern, value } => {
                let value_var = self.lower_expression(value)?;
                self.lower_pattern_binding(&pattern.node, value_var, BindingMode::Reference)?;
            }

            ast::Statement::Assignment { target, op, value } => {
                self.lower_assignment(target, op, value)?;
            }

            ast::Statement::Return { value } => {
                let var = value
                    .as_ref()
                    .map(|e| self.lower_expression(e))
                    .transpose()?;
                self.finish_block(Terminator::Return { value: var });
                self.start_block();
            }

            ast::Statement::Expression(expr) => {
                self.lower_expression(expr)?;
            }

            ast::Statement::Break { value } => {
                if let Some(loop_ctx) = self.loop_stack.last() {
                    let break_target = loop_ctx.break_target;
                    let _break_value = value
                        .as_ref()
                        .map(|e| self.lower_expression(e))
                        .transpose()?;
                    self.finish_block(Terminator::Jump {
                        target: break_target,
                    });
                    self.start_block();
                } else {
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
                    self.finish_block(Terminator::Return { value: None });
                    self.start_block();
                }
            }
        }
        Ok(())
    }

    /// Lower an assignment statement
    fn lower_assignment(
        &mut self,
        target: &ast::Expression,
        op: &ast::AssignmentOp,
        value: &ast::Expression,
    ) -> Result<()> {
        match target {
            ast::Expression::Variable(name) => {
                let rhs = self.lower_expression(value)?;

                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    let lhs =
                        self.lookup(&name.0)
                            .ok_or_else(|| LowerError::UndefinedVariable {
                                name: name.0.clone(),
                                span: dummy_span(),
                            })?;
                    self.lower_compound_op(lhs, op, rhs)?
                };

                let dest = self.new_temp(TypeSet::from_types(all_types()));
                self.emit(Instruction::Copy {
                    dest,
                    src: final_value,
                });
                self.bind(&name.0, dest);
            }

            ast::Expression::ArrayAccess { array, index } => {
                let base = self.lower_expression(array)?;
                let idx = self.lower_expression(index)?;
                let rhs = self.lower_expression(value)?;

                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    let current = self.new_temp(TypeSet::from_types(all_types()));
                    self.emit(Instruction::Index {
                        dest: current,
                        base,
                        key: idx,
                    });
                    self.lower_compound_op(current, op, rhs)?
                };

                self.emit(Instruction::SetIndex {
                    base,
                    key: idx,
                    value: final_value,
                });
            }

            ast::Expression::MemberAccess { object, member } => {
                let base = self.lower_expression(object)?;
                let key = self.lower_expression(member)?;
                let rhs = self.lower_expression(value)?;

                let final_value = if matches!(op, ast::AssignmentOp::Assign) {
                    rhs
                } else {
                    let current = self.new_temp(TypeSet::from_types(all_types()));
                    self.emit(Instruction::Index {
                        dest: current,
                        base,
                        key,
                    });
                    self.lower_compound_op(current, op, rhs)?
                };

                self.emit(Instruction::SetIndex {
                    base,
                    key,
                    value: final_value,
                });
            }

            _ => {
                self.lower_expression(target)?;
                self.lower_expression(value)?;
            }
        }
        Ok(())
    }

    /// Lower a compound assignment operator (+=, -=, etc.)
    fn lower_compound_op(
        &mut self,
        lhs: VarId,
        op: &ast::AssignmentOp,
        rhs: VarId,
    ) -> Result<VarId> {
        let builtin = match op {
            ast::AssignmentOp::Assign => unreachable!(),
            ast::AssignmentOp::AddAssign => "core.add",
            ast::AssignmentOp::SubAssign => "core.sub",
            ast::AssignmentOp::MulAssign => "core.mul",
            ast::AssignmentOp::DivAssign => "core.div",
            ast::AssignmentOp::ModAssign => "core.mod",
            ast::AssignmentOp::AndAssign => "core.bit_and",
            ast::AssignmentOp::OrAssign => "core.bit_or",
            ast::AssignmentOp::XorAssign => "core.bit_xor",
            ast::AssignmentOp::ShlAssign => "core.shl",
            ast::AssignmentOp::ShrAssign => "core.shr",
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
        Ok(dest)
    }
}
