//! Expression Lowering

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Expression Lowering
    // ========================================================================

    /// Lower an expression, returning the VarId holding the result
    pub(super) fn lower_expression(&mut self, expr: &ast::Expression) -> Result<VarId> {
        match expr {
            ast::Expression::Literal(lit) => self.lower_literal(lit),

            ast::Expression::Variable(name) => {
                self.lookup(&name.0)
                    .ok_or_else(|| LowerError::UndefinedVariable {
                        name: name.0.clone(),
                        span: dummy_span(),
                    })
            }

            ast::Expression::QualifiedName { namespace, name } => {
                let dest = self.new_temp(TypeSet::from_types(all_types()).as_optional());
                self.emit(Instruction::Undefined { dest });
                let _ = (namespace, name);
                Ok(dest)
            }

            ast::Expression::BinaryOp { left, op, right } => self.lower_binary_op(left, op, right),

            ast::Expression::UnaryOp { op, operand } => self.lower_unary_op(op, operand),

            ast::Expression::FunctionCall {
                namespace,
                name,
                arguments,
            } => self.lower_function_call(namespace.as_ref(), name, arguments),

            ast::Expression::ArrayAccess { array, index } => {
                let base = self.lower_expression(array)?;
                let key = self.lower_expression(index)?;
                let dest = self.new_temp(TypeSet::from_types(all_types()).as_optional());
                self.emit(Instruction::Index { dest, base, key });
                Ok(dest)
            }

            ast::Expression::MemberAccess { object, member } => {
                let base = self.lower_expression(object)?;
                let key = self.lower_expression(member)?;
                let dest = self.new_temp(TypeSet::from_types(all_types()).as_optional());
                self.emit(Instruction::Index { dest, base, key });
                Ok(dest)
            }

            ast::Expression::Block {
                statements,
                final_expr,
            } => {
                self.push_scope();
                for stmt in statements {
                    self.lower_statement(&stmt.node)?;
                }
                let result = if let Some(expr) = final_expr {
                    self.lower_expression(expr)?
                } else {
                    let dest = self.new_temp(TypeSet::undefined());
                    self.emit(Instruction::Undefined { dest });
                    dest
                };
                self.pop_scope();
                Ok(result)
            }

            ast::Expression::If {
                conditions,
                then_block,
                then_expr,
                else_block,
                else_expr,
            } => self.lower_if(conditions, then_block, then_expr, else_block, else_expr),

            ast::Expression::While {
                condition,
                body,
                body_expr,
            } => self.lower_while(condition, body, body_expr),

            ast::Expression::Loop { body, body_expr } => self.lower_loop(body, body_expr),

            ast::Expression::For {
                binding_is_value,
                binding,
                iterable,
                body,
                body_expr,
            } => self.lower_for(*binding_is_value, binding, iterable, body, body_expr),

            ast::Expression::Match { value, arms } => self.lower_match(value, arms),

            ast::Expression::Range {
                start,
                end,
                inclusive,
            } => self.lower_range(start, end, *inclusive),
        }
    }

    /// Lower a literal value
    pub(super) fn lower_literal(&mut self, lit: &ast::Literal) -> Result<VarId> {
        let (dest, instruction) = match lit {
            ast::Literal::Bool(b) => {
                let dest = self.new_temp(TypeSet::single(types::BaseType::Bool));
                (
                    dest,
                    Instruction::Const {
                        dest,
                        value: Literal::Bool(*b),
                    },
                )
            }
            ast::Literal::UInt(n) => {
                let dest = self.new_temp(TypeSet::single(types::BaseType::UInt));
                (
                    dest,
                    Instruction::Const {
                        dest,
                        value: Literal::UInt(*n),
                    },
                )
            }
            ast::Literal::Int(n) => {
                let dest = self.new_temp(TypeSet::single(types::BaseType::Int));
                (
                    dest,
                    Instruction::Const {
                        dest,
                        value: Literal::Int(*n),
                    },
                )
            }
            ast::Literal::Float(f) => {
                let dest = self.new_temp(TypeSet::single(types::BaseType::Float));
                (
                    dest,
                    Instruction::Const {
                        dest,
                        value: Literal::Float(*f),
                    },
                )
            }
            ast::Literal::Text(s) => {
                let dest = self.new_temp(TypeSet::single(types::BaseType::Text));
                (
                    dest,
                    Instruction::Const {
                        dest,
                        value: Literal::Text(s.clone()),
                    },
                )
            }
            ast::Literal::Bytes(b) => {
                let dest = self.new_temp(TypeSet::single(types::BaseType::Bytes));
                (
                    dest,
                    Instruction::Const {
                        dest,
                        value: Literal::Bytes(b.clone()),
                    },
                )
            }
            ast::Literal::Array(elements) => {
                return self.lower_array_literal(elements);
            }
            ast::Literal::Map(entries) => {
                return self.lower_map_literal(entries);
            }
        };

        self.emit(instruction);
        Ok(dest)
    }

    fn lower_array_literal(&mut self, elements: &[ast::Expression]) -> Result<VarId> {
        let element_vars: Vec<VarId> = elements
            .iter()
            .map(|e| self.lower_expression(e))
            .collect::<Result<_>>()?;

        let dest = self.new_temp(TypeSet::single(types::BaseType::Array));
        let args: Vec<CallArg> = element_vars
            .into_iter()
            .map(|v| CallArg {
                value: v,
                by_ref: false,
            })
            .collect();

        self.emit(Instruction::Call {
            dest,
            function: FunctionRef {
                namespace: None,
                name: ast::Identifier("core.make_array".to_string()),
            },
            args,
        });

        Ok(dest)
    }

    fn lower_map_literal(
        &mut self,
        entries: &[(ast::Expression, ast::Expression)],
    ) -> Result<VarId> {
        let entry_vars: Vec<(VarId, VarId)> = entries
            .iter()
            .map(|(k, v)| Ok((self.lower_expression(k)?, self.lower_expression(v)?)))
            .collect::<Result<_>>()?;

        let dest = self.new_temp(TypeSet::single(types::BaseType::Map));
        let args: Vec<CallArg> = entry_vars
            .into_iter()
            .flat_map(|(k, v)| {
                [
                    CallArg {
                        value: k,
                        by_ref: false,
                    },
                    CallArg {
                        value: v,
                        by_ref: false,
                    },
                ]
            })
            .collect();

        self.emit(Instruction::Call {
            dest,
            function: FunctionRef {
                namespace: None,
                name: ast::Identifier("core.make_map".to_string()),
            },
            args,
        });

        Ok(dest)
    }

    fn lower_binary_op(
        &mut self,
        left: &ast::Expression,
        op: &ast::BinaryOperator,
        right: &ast::Expression,
    ) -> Result<VarId> {
        match op {
            ast::BinaryOperator::And => return self.lower_short_circuit_and(left, right),
            ast::BinaryOperator::Or => return self.lower_short_circuit_or(left, right),
            _ => {}
        }

        let lhs = self.lower_expression(left)?;
        let rhs = self.lower_expression(right)?;

        let builtin = match op {
            ast::BinaryOperator::Add => "core.add",
            ast::BinaryOperator::Subtract => "core.sub",
            ast::BinaryOperator::Multiply => "core.mul",
            ast::BinaryOperator::Divide => "core.div",
            ast::BinaryOperator::Modulo => "core.mod",
            ast::BinaryOperator::Equal => "core.eq",
            ast::BinaryOperator::NotEqual => "core.neq",
            ast::BinaryOperator::Less => "core.lt",
            ast::BinaryOperator::LessEqual => "core.le",
            ast::BinaryOperator::Greater => "core.gt",
            ast::BinaryOperator::GreaterEqual => "core.ge",
            ast::BinaryOperator::BitwiseAnd => "core.bit_and",
            ast::BinaryOperator::BitwiseOr => "core.bit_or",
            ast::BinaryOperator::BitwiseXor => "core.bit_xor",
            ast::BinaryOperator::ShiftLeft => "core.shl",
            ast::BinaryOperator::ShiftRight => "core.shr",
            ast::BinaryOperator::And | ast::BinaryOperator::Or => unreachable!(),
        };

        let dest = self.new_temp(TypeSet::from_types(all_types()).as_optional());
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

    fn lower_short_circuit_and(
        &mut self,
        left: &ast::Expression,
        right: &ast::Expression,
    ) -> Result<VarId> {
        let lhs = self.lower_expression(left)?;

        let right_block = self.fresh_block();
        let join_block = self.fresh_block();

        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: right_block,
            else_target: join_block,
        });

        self.current_block = right_block;
        self.current_instructions = Vec::new();
        let rhs = self.lower_expression(right)?;
        let from_right = self.current_block;
        self.finish_block(Terminator::Jump { target: join_block });

        self.current_block = join_block;
        self.current_instructions = Vec::new();

        let false_var = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: false_var,
            value: Literal::Bool(false),
        });

        let result = self.new_temp(TypeSet::single(types::BaseType::Bool).as_optional());
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![
                (BlockId(right_block.0.wrapping_sub(1)), false_var),
                (from_right, rhs),
            ],
        });

        Ok(result)
    }

    fn lower_short_circuit_or(
        &mut self,
        left: &ast::Expression,
        right: &ast::Expression,
    ) -> Result<VarId> {
        let lhs = self.lower_expression(left)?;

        let right_block = self.fresh_block();
        let join_block = self.fresh_block();

        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: join_block,
            else_target: right_block,
        });

        self.current_block = right_block;
        self.current_instructions = Vec::new();
        let rhs = self.lower_expression(right)?;
        let from_right = self.current_block;
        self.finish_block(Terminator::Jump { target: join_block });

        self.current_block = join_block;
        self.current_instructions = Vec::new();

        let true_var = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: true_var,
            value: Literal::Bool(true),
        });

        let result = self.new_temp(TypeSet::single(types::BaseType::Bool).as_optional());
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![
                (BlockId(right_block.0.wrapping_sub(1)), true_var),
                (from_right, rhs),
            ],
        });

        Ok(result)
    }

    fn lower_unary_op(
        &mut self,
        op: &ast::UnaryOperator,
        operand: &ast::Expression,
    ) -> Result<VarId> {
        let arg = self.lower_expression(operand)?;

        let builtin = match op {
            ast::UnaryOperator::Negate => "core.neg",
            ast::UnaryOperator::Not => "core.not",
            ast::UnaryOperator::BitwiseNot => "core.bit_not",
        };

        let dest = self.new_temp(TypeSet::from_types(all_types()).as_optional());
        self.emit(Instruction::Call {
            dest,
            function: FunctionRef {
                namespace: None,
                name: ast::Identifier(builtin.to_string()),
            },
            args: vec![CallArg {
                value: arg,
                by_ref: false,
            }],
        });
        Ok(dest)
    }

    pub(super) fn lower_function_call(
        &mut self,
        namespace: Option<&ast::Identifier>,
        name: &ast::Identifier,
        arguments: &[ast::Expression],
    ) -> Result<VarId> {
        let full_name = match namespace {
            Some(ns) => format!("{}::{}", ns.0, name.0),
            None => name.0.clone(),
        };
        let param_specs = self.builtins.get(&full_name).map(|b| &b.meta.params);

        let args: Vec<CallArg> = arguments
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let by_ref = param_specs
                    .and_then(|specs| specs.get(i))
                    .map(|spec| spec.by_ref)
                    .unwrap_or(false);
                Ok(CallArg {
                    value: self.lower_expression(arg)?,
                    by_ref,
                })
            })
            .collect::<Result<_>>()?;

        let dest = self.new_temp(TypeSet::from_types(all_types()).as_optional());
        self.emit(Instruction::Call {
            dest,
            function: FunctionRef {
                namespace: namespace.cloned(),
                name: name.clone(),
            },
            args,
        });
        Ok(dest)
    }
}
