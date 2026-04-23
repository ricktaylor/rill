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
                    self.bind(name, value);
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
                    let idx = self.emit_const(Literal::UInt(i as u64));

                    let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::any());
                        self.emit(Instruction::MakeAccessor {
                            dest,
                            base: value,
                            key: idx,
                        });
                        let origin = RefOrigin {
                            ref_var: dest,
                            base_var: value,
                            key_var: Some(idx),
                            base_name: None,
                        };
                        (dest, Some(origin))
                    } else {
                        (self.emit_index(value, idx), None)
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
                    let idx = self.emit_const(Literal::UInt(i as u64));

                    let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::any());
                        self.emit(Instruction::MakeAccessor {
                            dest,
                            base: value,
                            key: idx,
                        });
                        let origin = RefOrigin {
                            ref_var: dest,
                            base_var: value,
                            key_var: Some(idx),
                            base_name: None,
                        };
                        (dest, Some(origin))
                    } else {
                        (self.emit_index(value, idx), None)
                    };

                    self.lower_pattern_binding_ref(&pat.node, elem, mode, elem_origin);
                }

                // Compute length for rest and after patterns
                let length = self.emit_unary_intrinsic(IntrinsicOp::Len, value);

                // Bind rest variable as a zero-copy Sequence over the source array.
                // ArraySeq(mode, [array, start, end]) -> Sequence(ArraySlice)
                //
                // SliceMode follows the binding mode:
                //   let [a, ..rest] = arr   → ReadOnly, iteration is by-value
                //   with [a, ..rest] = arr  → Mutable, for-loop uses MakeRef
                //                             so mutations write back to arr
                // A start < end guard produces undefined for empty slices.
                if let Some(rest_name) = rest {
                    let start = self.emit_const(Literal::UInt(before.len() as u64));

                    let after_len = self.emit_const(Literal::UInt(after.len() as u64));
                    let end = self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len);

                    let slice_mode = if matches!(mode, BindingMode::Reference) {
                        types::SliceMode::Mutable
                    } else {
                        types::SliceMode::ReadOnly
                    };

                    // Guard: start < end — empty slices produce undefined
                    let valid = self.emit_binary_intrinsic(IntrinsicOp::Lt, start, end);
                    let seq_bb = self.fresh_block();
                    let undef_bb = self.fresh_block();
                    let join_bb = self.fresh_block();
                    self.finish_block(Terminator::If {
                        condition: valid,
                        then_target: seq_bb,
                        else_target: undef_bb,
                        span: self.current_span,
                    });

                    // Then: create the slice sequence
                    self.current_block = seq_bb;
                    self.current_instructions = Vec::new();
                    let seq_val = self.new_temp(TypeSet::single(types::BaseType::Sequence));
                    self.emit(Instruction::Intrinsic {
                        dest: seq_val,
                        op: IntrinsicOp::ArraySeq(slice_mode),
                        args: vec![value, start, end],
                    });
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // Else: undefined
                    self.current_block = undef_bb;
                    self.current_instructions = Vec::new();
                    let undef_val = self.emit_undefined();
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // Join: phi
                    self.current_block = join_bb;
                    self.current_instructions = Vec::new();
                    let rest_val = self.emit_phi(vec![(seq_bb, seq_val), (undef_bb, undef_val)]);

                    self.bind(rest_name, rest_val);
                }

                // Bind after elements (from end, using len - after.len() + i)
                if !after.is_empty() {
                    let after_len = self.emit_const(Literal::UInt(after.len() as u64));
                    let after_start =
                        self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len);

                    for (i, pat) in after.iter().enumerate() {
                        let offset = self.emit_const(Literal::UInt(i as u64));
                        let idx = self.emit_binary_intrinsic(IntrinsicOp::Add, after_start, offset);

                        let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                            let dest = self.new_temp(TypeSet::any());
                            self.emit(Instruction::MakeAccessor {
                                dest,
                                base: value,
                                key: idx,
                            });
                            let origin = RefOrigin {
                                ref_var: dest,
                                base_var: value,
                                key_var: Some(idx),
                                base_name: None,
                            };
                            (dest, Some(origin))
                        } else {
                            (self.emit_index(value, idx), None)
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
                            self.emit_const(Literal::Text(name.to_string()))
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
                        let dest = self.new_temp(TypeSet::any());
                        self.emit(Instruction::MakeAccessor {
                            dest,
                            base: value,
                            key: key_var,
                        });
                        let origin = RefOrigin {
                            ref_var: dest,
                            base_var: value,
                            key_var: Some(key_var),
                            base_name: None,
                        };
                        (dest, Some(origin))
                    } else {
                        (self.emit_index(value, key_var), None)
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
