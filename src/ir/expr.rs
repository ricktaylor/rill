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
                if let Some(var) = self.lookup(&name.0) {
                    var
                } else {
                    self.error_undefined_var(None, &name.0, dummy_span());
                    self.error_placeholder()
                }
            }

            ast::Expression::QualifiedName { namespace, name } => {
                // QualifiedName is for accessing constants like `math::PI`
                // Currently not supported - emit error
                self.error_undefined_var(Some(&namespace.0), &name.0, dummy_span());
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
                    self.lower_statement(&stmt.node);
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
        let element_vars: Vec<VarId> = elements.iter().map(|e| self.lower_expression(e)).collect();

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
                name: ast::Identifier("core::make_array".to_string()),
            },
            args,
        });

        dest
    }

    fn lower_map_literal(&mut self, entries: &[(ast::Expression, ast::Expression)]) -> VarId {
        let entry_vars: Vec<(VarId, VarId)> = entries
            .iter()
            .map(|(k, v)| (self.lower_expression(k), self.lower_expression(v)))
            .collect();

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
                name: ast::Identifier("core::make_map".to_string()),
            },
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

        // Reflexive comparison operators expand to combinations of eq/lt/not
        // This reduces the number of builtins and enables optimization
        match op {
            // a != b  →  not(eq(a, b))
            ast::BinaryOperator::NotEqual => {
                let eq_result = self.emit_binary_call("core::eq", lhs, rhs);
                return self.emit_unary_call("core::not", eq_result);
            }
            // a > b  →  lt(b, a)  (swap operands)
            ast::BinaryOperator::Greater => {
                return self.emit_binary_call("core::lt", rhs, lhs);
            }
            // a <= b  →  not(lt(b, a))
            ast::BinaryOperator::LessEqual => {
                let lt_result = self.emit_binary_call("core::lt", rhs, lhs);
                return self.emit_unary_call("core::not", lt_result);
            }
            // a >= b  →  not(lt(a, b))
            ast::BinaryOperator::GreaterEqual => {
                let lt_result = self.emit_binary_call("core::lt", lhs, rhs);
                return self.emit_unary_call("core::not", lt_result);
            }
            _ => {}
        }

        // Direct builtin mapping for remaining operators
        let builtin = match op {
            ast::BinaryOperator::Add => "core::add",
            ast::BinaryOperator::Subtract => "core::sub",
            ast::BinaryOperator::Multiply => "core::mul",
            ast::BinaryOperator::Divide => "core::div",
            ast::BinaryOperator::Modulo => "core::mod",
            ast::BinaryOperator::Equal => "core::eq",
            ast::BinaryOperator::Less => "core::lt",
            ast::BinaryOperator::BitwiseAnd => "core::bit_and",
            ast::BinaryOperator::BitwiseOr => "core::bit_or",
            ast::BinaryOperator::BitwiseXor => "core::bit_xor",
            ast::BinaryOperator::ShiftLeft => "core::shl",
            ast::BinaryOperator::ShiftRight => "core::shr",
            ast::BinaryOperator::BitTest => "core::bit_test",
            // Already handled above
            ast::BinaryOperator::NotEqual
            | ast::BinaryOperator::Greater
            | ast::BinaryOperator::LessEqual
            | ast::BinaryOperator::GreaterEqual
            | ast::BinaryOperator::And
            | ast::BinaryOperator::Or => unreachable!(),
        };

        let dest = self.new_temp(TypeSet::all());
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

    /// Helper to emit a binary builtin call
    fn emit_binary_call(&mut self, builtin: &str, lhs: VarId, rhs: VarId) -> VarId {
        let dest = self.new_temp(TypeSet::all());
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

    /// Helper to emit a unary builtin call
    fn emit_unary_call(&mut self, builtin: &str, arg: VarId) -> VarId {
        let dest = self.new_temp(TypeSet::all());
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

        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: right_block,
            else_target: join_block,
            span: dummy_span(),
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
            sources: vec![
                (BlockId(right_block.0.wrapping_sub(1)), false_var),
                (from_right, rhs),
            ],
        });

        result
    }

    fn lower_short_circuit_or(&mut self, left: &ast::Expression, right: &ast::Expression) -> VarId {
        let lhs = self.lower_expression(left);

        let right_block = self.fresh_block();
        let join_block = self.fresh_block();

        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: join_block,
            else_target: right_block,
            span: dummy_span(),
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
            sources: vec![
                (BlockId(right_block.0.wrapping_sub(1)), true_var),
                (from_right, rhs),
            ],
        });

        result
    }

    fn lower_unary_op(&mut self, op: &ast::UnaryOperator, operand: &ast::Expression) -> VarId {
        let arg = self.lower_expression(operand);

        let builtin = match op {
            ast::UnaryOperator::Negate => "core::neg",
            ast::UnaryOperator::Not => "core::not",
            ast::UnaryOperator::BitwiseNot => "core::bit_not",
        };

        let dest = self.new_temp(TypeSet::all());
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
        dest
    }

    pub fn lower_function_call(
        &mut self,
        namespace: Option<&ast::Identifier>,
        name: &ast::Identifier,
        arguments: &[ast::Expression],
    ) -> VarId {
        // Determine the effective namespace and check for intrinsics
        let is_core_namespace = namespace.as_ref().map(|ns| ns.0 == "core").unwrap_or(false);

        // For qualified core:: calls or unqualified calls, check intrinsics first
        // (Unqualified calls implicitly look in core:: after local/imported)
        if (is_core_namespace || namespace.is_none())
            && let Some(result) = self.try_lower_intrinsic(&name.0, arguments)
        {
            return result;
        }

        // Resolve the full name for builtin lookup
        let full_name = if is_core_namespace {
            // Explicit core:: qualification
            format!("core::{}", name.0)
        } else if namespace.is_some() {
            // Other namespace qualification
            format!("{}::{}", namespace.as_ref().unwrap().0, name.0)
        } else {
            // Unqualified: try core:: prefix for builtin lookup
            // (In future: check local/imported functions first)
            format!("core::{}", name.0)
        };

        // Check if the function exists in the registry
        let builtin_def = self.builtins.get(&full_name);

        // If function not found in builtins and not in core namespace,
        // check if it might be a user-defined function (future: function table lookup)
        // For now, require all functions to be in the builtins registry
        if builtin_def.is_none() {
            self.error_undefined_fn(namespace.map(|ns| ns.0.as_str()), &name.0, dummy_span());
            return self.error_placeholder();
        }

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

        // Emit the call with the resolved namespace
        let resolved_namespace = if is_core_namespace || namespace.is_none() {
            Some(ast::Identifier("core".to_string()))
        } else {
            namespace.cloned()
        };

        let dest = self.new_temp(TypeSet::all());
        self.emit(Instruction::Call {
            dest,
            function: FunctionRef {
                namespace: resolved_namespace,
                name: name.clone(),
            },
            args,
        });
        dest
    }

    /// Try to lower a call as a core intrinsic. Returns Some(result) if it's an intrinsic
    /// with matching arity, None if it should be handled as a regular builtin call.
    fn try_lower_intrinsic(&mut self, name: &str, arguments: &[ast::Expression]) -> Option<VarId> {
        match name {
            "is_some" => self.lower_intrinsic_is_some(arguments),
            "is_uint" => self.lower_intrinsic_is_type(types::BaseType::UInt, arguments),
            "is_int" => self.lower_intrinsic_is_type(types::BaseType::Int, arguments),
            "is_float" => self.lower_intrinsic_is_type(types::BaseType::Float, arguments),
            "is_bool" => self.lower_intrinsic_is_type(types::BaseType::Bool, arguments),
            "is_text" => self.lower_intrinsic_is_type(types::BaseType::Text, arguments),
            "is_bytes" => self.lower_intrinsic_is_type(types::BaseType::Bytes, arguments),
            "is_array" => self.lower_intrinsic_is_type(types::BaseType::Array, arguments),
            "is_map" => self.lower_intrinsic_is_type(types::BaseType::Map, arguments),
            _ => None,
        }
    }

    /// Lower is_some(x) intrinsic: Guard x → (true), (false) + Phi
    /// Returns None if arity doesn't match, allowing fallthrough to normal lookup
    fn lower_intrinsic_is_some(&mut self, arguments: &[ast::Expression]) -> Option<VarId> {
        if arguments.len() != 1 {
            return None; // Fall through to normal function lookup
        }

        let value = self.lower_expression(&arguments[0]);

        // Create blocks for defined and undefined paths
        let defined_block = self.fresh_block();
        let undefined_block = self.fresh_block();
        let join_block = self.fresh_block();

        // Guard on the value
        self.finish_block(Terminator::Guard {
            value,
            defined: defined_block,
            undefined: undefined_block,
            span: dummy_span(),
        });

        // Defined path: result = true
        self.current_block = defined_block;
        self.current_instructions = Vec::new();
        let true_val = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: true_val,
            value: Literal::Bool(true),
        });
        self.finish_block(Terminator::Jump { target: join_block });

        // Undefined path: result = false
        self.current_block = undefined_block;
        self.current_instructions = Vec::new();
        let false_val = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: false_val,
            value: Literal::Bool(false),
        });
        self.finish_block(Terminator::Jump { target: join_block });

        // Join with Phi
        self.current_block = join_block;
        self.current_instructions = Vec::new();
        let result = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(defined_block, true_val), (undefined_block, false_val)],
        });

        Some(result)
    }

    /// Lower is_uint(x), is_int(x), etc. intrinsic: Match on type → (true), (false) + Phi
    /// Returns None if arity doesn't match, allowing fallthrough to normal lookup
    fn lower_intrinsic_is_type(
        &mut self,
        expected_type: types::BaseType,
        arguments: &[ast::Expression],
    ) -> Option<VarId> {
        if arguments.len() != 1 {
            return None; // Fall through to normal function lookup
        }

        let value = self.lower_expression(&arguments[0]);

        // Create blocks for match and default paths
        let match_block = self.fresh_block();
        let default_block = self.fresh_block();
        let join_block = self.fresh_block();

        // Match on type
        self.finish_block(Terminator::Match {
            value,
            arms: vec![(MatchPattern::Type(expected_type), match_block)],
            default: default_block,
            span: dummy_span(),
        });

        // Match path: result = true
        self.current_block = match_block;
        self.current_instructions = Vec::new();
        let true_val = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: true_val,
            value: Literal::Bool(true),
        });
        self.finish_block(Terminator::Jump { target: join_block });

        // Default path: result = false
        self.current_block = default_block;
        self.current_instructions = Vec::new();
        let false_val = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: false_val,
            value: Literal::Bool(false),
        });
        self.finish_block(Terminator::Jump { target: join_block });

        // Join with Phi
        self.current_block = join_block;
        self.current_instructions = Vec::new();
        let result = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(match_block, true_val), (default_block, false_val)],
        });

        Some(result)
    }
}
