//! Expression Lowering

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Expression Lowering
    // ========================================================================

    /// Lower an expression, returning the VarId holding the result
    ///
    /// Always returns a VarId. On error, emits a diagnostic and returns
    /// a placeholder (undefined) value.
    pub fn lower_expression(&mut self, expr: &ast::Expression) -> VarId {
        match expr {
            ast::Expression::Literal(lit) => self.lower_literal(lit),

            ast::Expression::Variable(name) => {
                if let Some(var) = self.lookup(name) {
                    var
                } else if let Some(cv) = self.const_bindings.get(name).cloned() {
                    // Constant binding — emit inline literal
                    let lit = match &cv {
                        ConstValue::Bool(b) => Some(Literal::Bool(*b)),
                        ConstValue::UInt(n) => Some(Literal::UInt(*n)),
                        ConstValue::Int(n) => Some(Literal::Int(*n)),
                        ConstValue::Float(f) => Some(Literal::Float(*f)),
                        ConstValue::Text(s) => Some(Literal::Text(s.clone())),
                        ConstValue::Bytes(b) => Some(Literal::Bytes(b.clone())),
                        _ => None, // Array/Map constants can't be inlined as literals
                    };
                    if let Some(lit) = lit {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Const { dest, value: lit });
                        dest
                    } else {
                        self.error_placeholder()
                    }
                } else {
                    self.error_undefined_var(None, name, self.current_span);
                    self.error_placeholder()
                }
            }

            ast::Expression::QualifiedName { namespace, name } => {
                // QualifiedName is for accessing constants like `math::PI`
                // Currently not supported - emit error
                self.error_undefined_var(Some(namespace), name, self.current_span);
                self.error_placeholder()
            }

            ast::Expression::BinaryOp { left, op, right } => self.lower_binary_op(left, op, right),

            ast::Expression::UnaryOp { op, operand } => self.lower_unary_op(op, operand),

            ast::Expression::FunctionCall {
                namespace,
                name,
                arguments,
            } => self.lower_function_call(namespace.as_ref(), name, arguments),

            ast::Expression::ArrayAccess { array, index } => {
                let base = self.lower_expression(array);
                let key = self.lower_expression(index);
                let dest = self.new_temp(TypeSet::all());
                self.emit(Instruction::Index { dest, base, key });
                dest
            }

            ast::Expression::MemberAccess { object, member } => {
                let base = self.lower_expression(object);
                let key = self.lower_expression(member);
                let dest = self.new_temp(TypeSet::all());
                self.emit(Instruction::Index { dest, base, key });
                dest
            }

            ast::Expression::Block {
                statements,
                final_expr,
            } => {
                self.push_scope();
                for stmt in statements {
                    self.lower_stmt(stmt);
                }
                let result = if let Some(expr) = final_expr {
                    self.lower_expression(expr)
                } else {
                    let dest = self.new_temp(TypeSet::empty());
                    self.emit(Instruction::Undefined { dest });
                    dest
                };
                self.pop_scope();
                result
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

            ast::Expression::Assignment { target, op, value } => {
                self.lower_assignment(target, op, value)
            }
        }
    }

    /// Lower a literal value
    pub fn lower_literal(&mut self, lit: &ast::Literal) -> VarId {
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
        dest
    }

    fn lower_array_literal(&mut self, elements: &[ast::Expression]) -> VarId {
        let args: Vec<VarId> = elements.iter().map(|e| self.lower_expression(e)).collect();

        let dest = self.new_temp(TypeSet::single(types::BaseType::Array));
        self.emit(Instruction::Intrinsic {
            dest,
            op: IntrinsicOp::MakeArray,
            args,
        });
        dest
    }

    fn lower_map_literal(&mut self, entries: &[(ast::Expression, ast::Expression)]) -> VarId {
        let entry_vars: Vec<(VarId, VarId)> = entries
            .iter()
            .map(|(k, v)| (self.lower_expression(k), self.lower_expression(v)))
            .collect();

        let args: Vec<VarId> = entry_vars.into_iter().flat_map(|(k, v)| [k, v]).collect();

        let dest = self.new_temp(TypeSet::single(types::BaseType::Map));
        self.emit(Instruction::Intrinsic {
            dest,
            op: IntrinsicOp::MakeMap,
            args,
        });
        dest
    }

    fn lower_binary_op(
        &mut self,
        left: &ast::Expression,
        op: &ast::BinaryOperator,
        right: &ast::Expression,
    ) -> VarId {
        // Short-circuit operators need special control flow
        match op {
            ast::BinaryOperator::And => return self.lower_short_circuit_and(left, right),
            ast::BinaryOperator::Or => return self.lower_short_circuit_or(left, right),
            _ => {}
        }

        let lhs = self.lower_expression(left);
        let rhs = self.lower_expression(right);

        // Reflexive comparison operators expand to combinations of Eq/Lt/Not
        // This reduces the number of intrinsics and enables optimization
        match op {
            // a != b  →  Not(Eq(a, b))
            ast::BinaryOperator::NotEqual => {
                let eq_result = self.emit_binary_intrinsic(IntrinsicOp::Eq, lhs, rhs);
                return self.emit_unary_intrinsic(IntrinsicOp::Not, eq_result);
            }
            // a > b  →  Lt(b, a)  (swap operands)
            ast::BinaryOperator::Greater => {
                return self.emit_binary_intrinsic(IntrinsicOp::Lt, rhs, lhs);
            }
            // a <= b  →  Not(Lt(b, a))
            ast::BinaryOperator::LessEqual => {
                let lt_result = self.emit_binary_intrinsic(IntrinsicOp::Lt, rhs, lhs);
                return self.emit_unary_intrinsic(IntrinsicOp::Not, lt_result);
            }
            // a >= b  →  Not(Lt(a, b))
            ast::BinaryOperator::GreaterEqual => {
                let lt_result = self.emit_binary_intrinsic(IntrinsicOp::Lt, lhs, rhs);
                return self.emit_unary_intrinsic(IntrinsicOp::Not, lt_result);
            }
            _ => {}
        }

        // Direct intrinsic mapping for remaining operators
        let intrinsic = op
            .intrinsic_op()
            .expect("reflexive/short-circuit ops handled above");
        self.emit_binary_intrinsic(intrinsic, lhs, rhs)
    }

    /// Emit a binary intrinsic operation.
    pub(crate) fn emit_binary_intrinsic(
        &mut self,
        op: IntrinsicOp,
        lhs: VarId,
        rhs: VarId,
    ) -> VarId {
        let dest = self.new_temp(op.result_type());
        self.emit(Instruction::Intrinsic {
            dest,
            op,
            args: vec![lhs, rhs],
        });
        dest
    }

    /// Emit a unary intrinsic operation.
    pub(crate) fn emit_unary_intrinsic(&mut self, op: IntrinsicOp, arg: VarId) -> VarId {
        let dest = self.new_temp(op.result_type());
        self.emit(Instruction::Intrinsic {
            dest,
            op,
            args: vec![arg],
        });
        dest
    }

    fn lower_short_circuit_and(
        &mut self,
        left: &ast::Expression,
        right: &ast::Expression,
    ) -> VarId {
        let lhs = self.lower_expression(left);

        let right_block = self.fresh_block();
        let join_block = self.fresh_block();

        let from_left = self.current_block;
        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: right_block,
            else_target: join_block,
            span: self.current_span,
        });

        self.current_block = right_block;
        self.current_instructions = Vec::new();
        let rhs = self.lower_expression(right);
        let from_right = self.current_block;
        self.finish_block(Terminator::Jump { target: join_block });

        self.current_block = join_block;
        self.current_instructions = Vec::new();

        let false_var = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: false_var,
            value: Literal::Bool(false),
        });

        let result = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(from_left, false_var), (from_right, rhs)],
        });

        result
    }

    fn lower_short_circuit_or(&mut self, left: &ast::Expression, right: &ast::Expression) -> VarId {
        let lhs = self.lower_expression(left);

        let right_block = self.fresh_block();
        let join_block = self.fresh_block();

        let from_left = self.current_block;
        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: join_block,
            else_target: right_block,
            span: self.current_span,
        });

        self.current_block = right_block;
        self.current_instructions = Vec::new();
        let rhs = self.lower_expression(right);
        let from_right = self.current_block;
        self.finish_block(Terminator::Jump { target: join_block });

        self.current_block = join_block;
        self.current_instructions = Vec::new();

        let true_var = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: true_var,
            value: Literal::Bool(true),
        });

        let result = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(from_left, true_var), (from_right, rhs)],
        });

        result
    }

    fn lower_unary_op(&mut self, op: &ast::UnaryOperator, operand: &ast::Expression) -> VarId {
        let arg = self.lower_expression(operand);
        self.emit_unary_intrinsic(op.intrinsic_op(), arg)
    }

    pub fn lower_function_call(
        &mut self,
        namespace: Option<&ast::Identifier>,
        name: &ast::Identifier,
        arguments: &[ast::Expression],
    ) -> VarId {
        // Check for compiler intrinsics first (e.g. len).
        // These lower to Instruction::Intrinsic, not function calls.
        if namespace.is_none()
            && let Some(result) = self.try_lower_intrinsic(name, arguments)
        {
            return result;
        }

        // Build the lookup name for the builtin registry.
        // The registry now contains only user-callable extern functions
        // (no core:: prefix needed).
        let lookup_name = if let Some(ns) = namespace {
            format!("{ns}::{name}")
        } else {
            name.to_string()
        };

        // Check if the function exists in the registry
        let builtin_def = self.builtins.get(&lookup_name);

        let param_specs = builtin_def.map(|b| &b.meta.params);

        let args: Vec<CallArg> = arguments
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let by_ref = param_specs
                    .and_then(|specs| specs.get(i))
                    .map(|spec| spec.by_ref)
                    .unwrap_or(false);
                CallArg {
                    value: self.lower_expression(arg),
                    by_ref,
                }
            })
            .collect();

        let dest = self.new_temp(TypeSet::all());
        self.emit(Instruction::Call {
            dest,
            function: FunctionRef {
                namespace: namespace.cloned(),
                name: name.clone(),
            },
            args,
        });
        dest
    }

    /// Try to lower a call as a compiler intrinsic.
    /// Returns Some(result) if recognized, None to fall through to normal call resolution.
    fn try_lower_intrinsic(&mut self, name: &str, arguments: &[ast::Expression]) -> Option<VarId> {
        match name {
            "len" if arguments.len() == 1 => {
                let arg = self.lower_expression(&arguments[0]);
                Some(self.emit_unary_intrinsic(IntrinsicOp::Len, arg))
            }
            _ => None,
        }
    }
}
