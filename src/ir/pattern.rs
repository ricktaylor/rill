//! Pattern Lowering
//!
//! Handles pattern binding for let/with statements (unconditional binding).

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Pattern Lowering
    // ========================================================================

    /// Lower a pattern binding (for let/with statements — unconditional)
    ///
    /// Unlike conditional patterns (if-let, match arms), unconditional bindings
    /// don't branch on mismatch. If a type/structure doesn't match, the bound
    /// variables are simply undefined (duck-typing: no error, undefined propagation).
    pub fn lower_pattern_binding(
        &mut self,
        pattern: &ast::Pattern,
        value: VarId,
        mode: BindingMode,
    ) {
        self.lower_pattern_binding_ref(pattern, value, mode, None);
    }

    /// Lower a pattern binding with optional ref origin tracking.
    ///
    /// When `ref_origin` is `Some`, the value came from a `with` binding and
    /// the ref origin is recorded so that subsequent assignments emit `WriteRef`.
    pub fn lower_pattern_binding_ref(
        &mut self,
        pattern: &ast::Pattern,
        value: VarId,
        mode: BindingMode,
        ref_origin: Option<RefOrigin>,
    ) {
        match pattern {
            ast::Pattern::Wildcard => {
                // Ignore the value
            }

            ast::Pattern::Variable(name) => match mode {
                BindingMode::Value => {
                    let dest = self.new_var(name.clone(), TypeSet::from_types(all_types()));
                    self.emit(Instruction::Copy { dest, src: value });
                    self.bind(name, dest);
                }
                BindingMode::Reference => {
                    self.bind(name, value);
                    // Record the ref origin so assignments to this name emit WriteRef
                    if let Some(origin) = ref_origin {
                        self.bind_ref(name, origin);
                    }
                }
            },

            ast::Pattern::Literal(_lit) => {
                // Literal patterns in let/with don't bind anything.
                // They're only meaningful in conditional contexts (match, if-let).
            }

            ast::Pattern::Array(patterns) => {
                for (i, pat) in patterns.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });

                    let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::MakeRef {
                            dest,
                            base: value,
                            key: Some(idx),
                        });
                        let origin = RefOrigin {
                            ref_var: dest,
                            base: value,
                            key: Some(idx),
                        };
                        (dest, Some(origin))
                    } else {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest,
                            base: value,
                            key: idx,
                        });
                        (dest, None)
                    };

                    self.lower_pattern_binding_ref(&pat.node, elem, mode, elem_origin);
                }
            }

            ast::Pattern::ArrayRest {
                before,
                rest,
                after,
            } => {
                // Bind before elements (from start)
                for (i, pat) in before.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });

                    let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::MakeRef {
                            dest,
                            base: value,
                            key: Some(idx),
                        });
                        let origin = RefOrigin {
                            ref_var: dest,
                            base: value,
                            key: Some(idx),
                        };
                        (dest, Some(origin))
                    } else {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest,
                            base: value,
                            key: idx,
                        });
                        (dest, None)
                    };

                    self.lower_pattern_binding_ref(&pat.node, elem, mode, elem_origin);
                }

                // Compute length for rest and after patterns
                let length = self.emit_unary_intrinsic(IntrinsicOp::Len, value);

                // Bind rest variable as a zero-copy Sequence over the source array.
                // ArraySeq(array, start, end, mutable) -> Sequence(ArraySlice)
                //
                // Mutability follows the binding mode:
                //   let [a, ..rest] = arr   → mutable=false, iteration is by-value
                //   with [a, ..rest] = arr  → mutable=true, for-loop uses MakeRef
                //                             so mutations write back to arr
                if let Some(rest_name) = rest {
                    let start = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: start,
                        value: Literal::UInt(before.len() as u64),
                    });

                    let after_len = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let end = self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len);

                    let is_mutable = self.new_temp(TypeSet::single(types::BaseType::Bool));
                    self.emit(Instruction::Const {
                        dest: is_mutable,
                        value: Literal::Bool(matches!(mode, BindingMode::Reference)),
                    });

                    let rest_val = self.new_temp(TypeSet::single(types::BaseType::Sequence));
                    self.emit(Instruction::Intrinsic {
                        dest: rest_val,
                        op: IntrinsicOp::ArraySeq,
                        args: vec![value, start, end, is_mutable],
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
                    let after_len = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let after_start =
                        self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len);

                    for (i, pat) in after.iter().enumerate() {
                        let offset = self.new_temp(TypeSet::single(types::BaseType::UInt));
                        self.emit(Instruction::Const {
                            dest: offset,
                            value: Literal::UInt(i as u64),
                        });
                        let idx = self.emit_binary_intrinsic(IntrinsicOp::Add, after_start, offset);

                        let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                            let dest = self.new_temp(TypeSet::all());
                            self.emit(Instruction::MakeRef {
                                dest,
                                base: value,
                                key: Some(idx),
                            });
                            let origin = RefOrigin {
                                ref_var: dest,
                                base: value,
                                key: Some(idx),
                            };
                            (dest, Some(origin))
                        } else {
                            let dest = self.new_temp(TypeSet::all());
                            self.emit(Instruction::Index {
                                dest,
                                base: value,
                                key: idx,
                            });
                            (dest, None)
                        };

                        self.lower_pattern_binding_ref(&pat.node, elem, mode, elem_origin);
                    }
                }
            }

            ast::Pattern::Map(entries) => {
                // Destructure map: each entry has a key pattern (must be literal)
                // and a value pattern. Index into the map by key and bind the value.
                for (key_pat, val_pat) in entries {
                    // Key must be a literal for map destructuring
                    let key_var = match &key_pat.node {
                        ast::Pattern::Literal(lit) => self.lower_literal(lit),
                        ast::Pattern::Variable(name) => {
                            // Variable key: use the variable name as a text key
                            let key = self.new_temp(TypeSet::single(types::BaseType::Text));
                            self.emit(Instruction::Const {
                                dest: key,
                                value: Literal::Text(name.to_string()),
                            });
                            key
                        }
                        _ => {
                            self.diagnostics.error(
                                diagnostics::DiagnosticCode::E105_InvalidPattern,
                                self.current_span,
                                "map destructuring key must be a literal or identifier",
                            );
                            continue;
                        }
                    };

                    let (val, val_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::MakeRef {
                            dest,
                            base: value,
                            key: Some(key_var),
                        });
                        let origin = RefOrigin {
                            ref_var: dest,
                            base: value,
                            key: Some(key_var),
                        };
                        (dest, Some(origin))
                    } else {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest,
                            base: value,
                            key: key_var,
                        });
                        (dest, None)
                    };

                    self.lower_pattern_binding_ref(&val_pat.node, val, mode, val_origin);
                }
            }

            ast::Pattern::Type { type_name, binding } => {
                // Type pattern in unconditional binding: check type, bind if matches.
                // If type doesn't match, the binding is undefined (duck-typing).
                // We emit a Match terminator with a join block — if the type matches
                // we bind, otherwise variables get undefined.
                if let Some(base_type) = self.type_name_to_base_type(type_name) {
                    let match_bb = self.fresh_block();
                    let nomatch_bb = self.fresh_block();
                    let join_bb = self.fresh_block();

                    self.finish_block(Terminator::Match {
                        value,
                        arms: vec![(MatchPattern::Type(base_type), match_bb)],
                        default: nomatch_bb,
                        span: self.current_span,
                    });

                    // Match path: bind the inner pattern
                    self.current_block = match_bb;
                    self.current_instructions = Vec::new();
                    if let Some(inner) = binding {
                        self.lower_pattern_binding_ref(
                            &inner.node,
                            value,
                            mode,
                            ref_origin.clone(),
                        );
                    }
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // No-match path: skip (variables remain unbound/undefined)
                    self.current_block = nomatch_bb;
                    self.current_instructions = Vec::new();
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // Continue in join block
                    self.current_block = join_bb;
                    self.current_instructions = Vec::new();
                } else {
                    // Unknown type name — error already emitted by type_name_to_base_type
                }
            }
        }
    }
}
