use inkwell::{
    builder::Builder,
    context::Context,
    module::Module,
    types::{BasicType, BasicTypeEnum, FunctionType},
    values::{BasicValue, FunctionValue},
    FloatPredicate, IntPredicate,
};
use smallvec::SmallVec;
use wasmer_runtime_core::{
    module::ModuleInfo,
    structures::{Map, SliceMap, TypedIndex},
    types::{FuncIndex, FuncSig, LocalFuncIndex, LocalOrImport, SigIndex, Type},
};
use wasmparser::{BinaryReaderError, CodeSectionReader, LocalsReader, Operator, OperatorsReader};

use crate::intrinsics::Intrinsics;
use crate::read_info::type_to_type;
use crate::state::State;

fn func_sig_to_llvm(context: &Context, intrinsics: &Intrinsics, sig: &FuncSig) -> FunctionType {
    let user_param_types = sig.params().iter().map(|&ty| type_to_llvm(intrinsics, ty));

    let param_types: Vec<_> = user_param_types.collect();

    match sig.returns() {
        [] => intrinsics.void_ty.fn_type(&param_types, false),
        [single_value] => type_to_llvm(intrinsics, *single_value).fn_type(&param_types, false),
        returns @ _ => {
            let basic_types: Vec<_> = returns
                .iter()
                .map(|&ty| type_to_llvm(intrinsics, ty))
                .collect();

            context
                .struct_type(&basic_types, false)
                .fn_type(&param_types, false)
        }
    }
}

fn type_to_llvm(intrinsics: &Intrinsics, ty: Type) -> BasicTypeEnum {
    match ty {
        Type::I32 => intrinsics.i32_ty.as_basic_type_enum(),
        Type::I64 => intrinsics.i64_ty.as_basic_type_enum(),
        Type::F32 => intrinsics.f32_ty.as_basic_type_enum(),
        Type::F64 => intrinsics.f64_ty.as_basic_type_enum(),
    }
}

pub fn parse_function_bodies(
    info: &ModuleInfo,
    code_reader: CodeSectionReader,
) -> Result<(), BinaryReaderError> {
    let context = Context::create();
    let module = context.create_module("module");
    let builder = context.create_builder();

    let intrinsics = Intrinsics::declare(&module, &context);

    let signatures: Map<SigIndex, FunctionType> = info
        .signatures
        .iter()
        .map(|(_, sig)| func_sig_to_llvm(&context, &intrinsics, sig))
        .collect();
    let functions: Map<LocalFuncIndex, FunctionValue> = info
        .func_assoc
        .iter()
        .skip(info.imported_functions.len())
        .map(|(func_index, &sig_index)| {
            module.add_function(
                &format!("fn{}", func_index.index()),
                signatures[sig_index],
                None,
            )
        })
        .collect();

    for (local_func_index, body) in code_reader.into_iter().enumerate() {
        let body = body?;

        let locals_reader = body.get_locals_reader()?;
        let op_reader = body.get_operators_reader()?;

        parse_function(
            &context,
            &module,
            &builder,
            &intrinsics,
            info,
            &signatures,
            &functions,
            LocalFuncIndex::new(local_func_index),
            locals_reader,
            op_reader,
        )?;
    }

    Ok(())
}

fn parse_function(
    context: &Context,
    module: &Module,
    builder: &Builder,
    intrinsics: &Intrinsics,
    info: &ModuleInfo,
    signatures: &SliceMap<SigIndex, FunctionType>,
    functions: &SliceMap<LocalFuncIndex, FunctionValue>,
    func_index: LocalFuncIndex,
    locals_reader: LocalsReader,
    op_reader: OperatorsReader,
) -> Result<(), BinaryReaderError> {
    let sig_index = info.func_assoc[func_index.convert_up(info)];
    let func_sig = &info.signatures[sig_index];
    let llvm_sig = &signatures[sig_index];

    let function = functions[func_index];
    let entry_block = context.append_basic_block(&function, "entry");
    builder.position_at_end(&entry_block);

    let mut state = State::new();

    let mut locals = Vec::with_capacity(locals_reader.get_count() as usize);
    locals.extend(function.get_param_iter().enumerate().map(|(index, param)| {
        let ty = param.get_type();

        let alloca = builder.build_alloca(ty, &format!("local{}", index));
        builder.build_store(alloca, param);
        alloca
    }));

    for (index, local) in locals_reader.into_iter().enumerate().skip(locals.len()) {
        let (_, ty) = local?;

        let wasmer_ty = type_to_type(ty)?;

        let ty = type_to_llvm(intrinsics, wasmer_ty);

        let alloca = builder.build_alloca(ty, &format!("local{}", index));

        let default_value = match wasmer_ty {
            Type::I32 => intrinsics.i32_zero.as_basic_value_enum(),
            Type::I64 => intrinsics.i64_zero.as_basic_value_enum(),
            Type::F32 => intrinsics.f32_zero.as_basic_value_enum(),
            Type::F64 => intrinsics.f64_zero.as_basic_value_enum(),
        };

        builder.build_store(alloca, default_value);

        locals.push(alloca);
    }

    for op in op_reader {
        match op? {
            /***************************
             * Control Flow instructions.
             * https://github.com/sunfishcode/wasm-reference-manual/blob/master/WebAssembly.md#control-flow-instructions
             ***************************/
            Operator::Block { ty } => {
                let current_block = builder.get_insert_block().ok_or(BinaryReaderError {
                    message: "not currently in a block",
                    offset: -1isize as usize,
                })?;

                let end_block = context.append_basic_block(&function, &state.block_name());
                builder.position_at_end(&end_block);

                let phis = if let Ok(wasmer_ty) = type_to_type(ty) {
                    let llvm_ty = type_to_llvm(intrinsics, wasmer_ty);
                    [llvm_ty]
                        .iter()
                        .map(|&ty| builder.build_phi(ty, &state.var_name()))
                        .collect()
                } else {
                    SmallVec::new()
                };

                state.push_block(end_block, phis);
                builder.position_at_end(&current_block);
            }
            Operator::Loop { ty } => {

                // let loop_body = context.append_basic_block(&function, &state.block_name());
                // let next = context.append_basic_block(&function, &state.block_name());
                // builder.build_unconditional_branch(&body);
                // let num_return_values = if ty == wasmparser::Type::EmptyBlockType { 0 } else { 1 };
                // state.push_loop(loop_body, next, num_return_values);
            }
            Operator::Br { relative_depth } => {
                let frame = state.frame_at_depth(relative_depth)?;

                let current_block = builder.get_insert_block().ok_or(BinaryReaderError {
                    message: "not currently in a block",
                    offset: -1isize as usize,
                })?;

                let values = state.peekn(frame.phis().len())?;

                // For each result of the block we're branching to,
                // pop a value off the value stack and load it into
                // the corresponding phi.
                for (phi, value) in frame.phis().iter().zip(values.iter()) {
                    phi.add_incoming(&[(value, &current_block)]);
                }

                builder.build_unconditional_branch(frame.dest());

                state.popn(frame.phis().len())?;

                builder.build_unreachable();
            }
            Operator::BrIf { relative_depth } => {
                let cond = state.pop1()?;
                let frame = state.frame_at_depth(relative_depth)?;

                let current_block = builder.get_insert_block().ok_or(BinaryReaderError {
                    message: "not currently in a block",
                    offset: -1isize as usize,
                })?;

                let param_stack = state.peekn(frame.phis().len())?;

                for (phi, value) in frame.phis().iter().zip(param_stack.iter()) {
                    phi.add_incoming(&[(value, &current_block)]);
                }

                let false_block = context.append_basic_block(&function, &state.block_name());

                let cond_value = builder.build_int_compare(
                    IntPredicate::NE,
                    cond.into_int_value(),
                    intrinsics.i32_zero,
                    &state.var_name(),
                );
                builder.build_conditional_branch(cond_value, frame.dest(), &false_block);
                builder.position_at_end(&false_block);
            }
            Operator::BrTable { ref table } => {
                let current_block = builder.get_insert_block().ok_or(BinaryReaderError {
                    message: "not currently in a block",
                    offset: -1isize as usize,
                })?;

                let (label_depths, default_depth) = table.read_table()?;

                let index = state.pop1()?;

                let default_frame = state.frame_at_depth(default_depth)?;

                let res_len = default_frame.phis().len();

                let args = state.peekn(res_len)?;

                for (phi, value) in default_frame.phis().iter().zip(args.iter()) {
                    phi.add_incoming(&[(value, &current_block)]);
                }

                let cases: Vec<_> = label_depths
                    .iter()
                    .enumerate()
                    .map(|(case_index, &depth)| {
                        let frame = state.frame_at_depth(depth)?;
                        let case_index_literal =
                            context.i32_type().const_int(case_index as u64, false);

                        for (phi, value) in frame.phis().iter().zip(args.iter()) {
                            phi.add_incoming(&[(value, &current_block)]);
                        }

                        Ok((case_index_literal, frame.dest()))
                    })
                    .collect::<Result<_, _>>()?;

                builder.build_switch(index.into_int_value(), default_frame.dest(), &cases[..]);

                state.popn(res_len)?;
                builder.build_unreachable();
            }

            Operator::End => {
                let frame = state.pop_frame()?;

                // Push each phi value to the value stack.
                for phi in frame.phis() {
                    state.push1(phi.as_basic_value());
                }

                state.reset_stack(&frame);
            }

            Operator::Unreachable => {
                // Emit an unreachable instruction.
                // If llvm cannot prove that this is never touched,
                // it will emit a `ud2` instruction on x86_64 arches.
                builder.build_unreachable();
            }

            /***************************
             * Basic instructions.
             * https://github.com/sunfishcode/wasm-reference-manual/blob/master/WebAssembly.md#basic-instructions
             ***************************/
            Operator::Nop => {
                // Do nothing.
            }
            Operator::Drop => {
                state.pop1()?;
            }

            // Generate const values.
            Operator::I32Const { value } => {
                let i = intrinsics.i32_ty.const_int(value as u64, false);
                state.push1(i);
            }
            Operator::I64Const { value } => {
                let i = intrinsics.i64_ty.const_int(value as u64, false);
                state.push1(i);
            }
            Operator::F32Const { value } => {
                let f = intrinsics
                    .f32_ty
                    .const_float(f64::from_bits(value.bits() as u64));
                state.push1(f);
            }
            Operator::F64Const { value } => {
                let f = intrinsics.f64_ty.const_float(f64::from_bits(value.bits()));
                state.push1(f);
            }

            // Operate on locals.
            Operator::GetLocal { local_index } => {
                let pointer_value = locals[local_index as usize];
                let v = builder.build_load(pointer_value, &state.var_name());
                state.push1(v);
            }
            Operator::SetLocal { local_index } => {
                let pointer_value = locals[local_index as usize];
                let v = state.pop1()?;
                builder.build_store(pointer_value, v);
            }
            Operator::TeeLocal { local_index } => {
                let pointer_value = locals[local_index as usize];
                let v = state.peek1()?;
                builder.build_store(pointer_value, v);
            }

            Operator::GetGlobal { global_index } => unimplemented!(),
            Operator::SetGlobal { global_index } => unimplemented!(),

            Operator::Select => {
                let (v1, v2, cond) = state.pop3()?;
                let cond = cond.into_int_value();
                let res = builder.build_select(cond, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::Call { function_index } => {
                let func_index = FuncIndex::new(function_index as usize);
                let sigindex = info.func_assoc[func_index];
                let llvm_sig = signatures[sigindex];

                match func_index.local_or_import(info) {
                    LocalOrImport::Local(local_func_index) => {
                        let func_sig = &info.signatures[sigindex];
                        let func_value = functions[local_func_index];
                        let call_site = builder.build_call(
                            func_value,
                            &state.peekn(func_sig.params().len())?.to_vec(),
                            &state.var_name(),
                        );
                        if let Some(basic_value) = call_site.try_as_basic_value().left() {
                            match func_sig.returns().len() {
                                1 => state.push1(basic_value),
                                count @ _ => {
                                    // This is a multi-value return.
                                    let struct_value = basic_value.into_struct_value();
                                    for i in 0..(count as u32) {
                                        let value = builder.build_extract_value(
                                            struct_value,
                                            i,
                                            &state.var_name(),
                                        );
                                        state.push1(value);
                                    }
                                }
                            }
                        }
                    }
                    LocalOrImport::Import(import_func_index) => unimplemented!(),
                }
            }
            Operator::CallIndirect { index, table_index } => {
                unimplemented!("{}, {}", index, table_index);
            }

            /***************************
             * Integer Arithmetic instructions.
             * https://github.com/sunfishcode/wasm-reference-manual/blob/master/WebAssembly.md#integer-arithmetic-instructions
             ***************************/
            Operator::I32Add | Operator::I64Add => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_add(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32Sub | Operator::I64Sub => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_sub(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32Mul | Operator::I64Mul => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_mul(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32DivS | Operator::I64DivS => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_signed_div(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32DivU | Operator::I64DivU => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_unsigned_div(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32RemS | Operator::I64RemS => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_signed_rem(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32RemU | Operator::I64RemU => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_unsigned_rem(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32And | Operator::I64And => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_and(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32Or | Operator::I64Or => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_or(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32Xor | Operator::I64Xor => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_xor(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32Shl | Operator::I64Shl => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_left_shift(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32ShrS | Operator::I64ShrS => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_right_shift(v1, v2, true, &state.var_name());
                state.push1(res);
            }
            Operator::I32ShrU | Operator::I64ShrU => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_right_shift(v1, v2, false, &state.var_name());
                state.push1(res);
            }
            Operator::I32Rotl => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let lhs = builder.build_left_shift(v1, v2, &state.var_name());
                let rhs = {
                    let int_width = intrinsics.i32_ty.const_int(32 as u64, false);
                    let rhs = builder.build_int_sub(int_width, v2, &state.var_name());
                    builder.build_right_shift(v1, rhs, false, &state.var_name())
                };
                let res = builder.build_or(lhs, rhs, &state.var_name());
                state.push1(res);
            }
            Operator::I64Rotl => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let lhs = builder.build_left_shift(v1, v2, &state.var_name());
                let rhs = {
                    let int_width = intrinsics.i64_ty.const_int(64 as u64, false);
                    let rhs = builder.build_int_sub(int_width, v2, &state.var_name());
                    builder.build_right_shift(v1, rhs, false, &state.var_name())
                };
                let res = builder.build_or(lhs, rhs, &state.var_name());
                state.push1(res);
            }
            Operator::I32Rotr => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let lhs = builder.build_right_shift(v1, v2, false, &state.var_name());
                let rhs = {
                    let int_width = intrinsics.i32_ty.const_int(32 as u64, false);
                    let rhs = builder.build_int_sub(int_width, v2, &state.var_name());
                    builder.build_left_shift(v1, rhs, &state.var_name())
                };
                let res = builder.build_or(lhs, rhs, &state.var_name());
                state.push1(res);
            }
            Operator::I64Rotr => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let lhs = builder.build_right_shift(v1, v2, false, &state.var_name());
                let rhs = {
                    let int_width = intrinsics.i64_ty.const_int(64 as u64, false);
                    let rhs = builder.build_int_sub(int_width, v2, &state.var_name());
                    builder.build_left_shift(v1, rhs, &state.var_name())
                };
                let res = builder.build_or(lhs, rhs, &state.var_name());
                state.push1(res);
            }
            Operator::I32Clz => {
                let input = state.pop1()?;
                let ensure_defined_zero = intrinsics
                    .i1_ty
                    .const_int(1 as u64, false)
                    .as_basic_value_enum();
                let res = builder
                    .build_call(
                        intrinsics.ctlz_i32,
                        &[input, ensure_defined_zero],
                        &state.var_name(),
                    )
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::I64Clz => {
                let input = state.pop1()?;
                let ensure_defined_zero = intrinsics
                    .i1_ty
                    .const_int(1 as u64, false)
                    .as_basic_value_enum();
                let res = builder
                    .build_call(
                        intrinsics.ctlz_i64,
                        &[input, ensure_defined_zero],
                        &state.var_name(),
                    )
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::I32Ctz => {
                let input = state.pop1()?;
                let ensure_defined_zero = intrinsics
                    .i1_ty
                    .const_int(1 as u64, false)
                    .as_basic_value_enum();
                let res = builder
                    .build_call(
                        intrinsics.cttz_i32,
                        &[input, ensure_defined_zero],
                        &state.var_name(),
                    )
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::I64Ctz => {
                let input = state.pop1()?;
                let ensure_defined_zero = intrinsics
                    .i1_ty
                    .const_int(1 as u64, false)
                    .as_basic_value_enum();
                let res = builder
                    .build_call(
                        intrinsics.cttz_i64,
                        &[input, ensure_defined_zero],
                        &state.var_name(),
                    )
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::I32Popcnt => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.ctpop_i32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::I64Popcnt => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.ctpop_i64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::I32Eqz => {
                let input = state.pop1()?.into_int_value();
                let res = builder.build_int_compare(
                    IntPredicate::EQ,
                    input,
                    intrinsics.i32_zero,
                    &state.var_name(),
                );
                state.push1(res);
            }
            Operator::I64Eqz => {
                let input = state.pop1()?.into_int_value();
                let res = builder.build_int_compare(
                    IntPredicate::EQ,
                    input,
                    intrinsics.i64_zero,
                    &state.var_name(),
                );
                state.push1(res);
            }

            /***************************
             * Floating-Point Arithmetic instructions.
             * https://github.com/sunfishcode/wasm-reference-manual/blob/master/WebAssembly.md#floating-point-arithmetic-instructions
             ***************************/
            Operator::F32Add | Operator::F64Add => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res = builder.build_float_add(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Sub | Operator::F64Sub => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res = builder.build_float_sub(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Mul | Operator::F64Mul => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res = builder.build_float_mul(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Div | Operator::F64Div => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res = builder.build_float_div(v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Sqrt => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.sqrt_f32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Sqrt => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.sqrt_f64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Min => {
                let (v1, v2) = state.pop2()?;
                let res = builder
                    .build_call(intrinsics.minimum_f32, &[v1, v2], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Min => {
                let (v1, v2) = state.pop2()?;
                let res = builder
                    .build_call(intrinsics.minimum_f64, &[v1, v2], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Max => {
                let (v1, v2) = state.pop2()?;
                let res = builder
                    .build_call(intrinsics.maximum_f32, &[v1, v2], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Max => {
                let (v1, v2) = state.pop2()?;
                let res = builder
                    .build_call(intrinsics.maximum_f64, &[v1, v2], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Ceil => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.ceil_f32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Ceil => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.ceil_f64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Floor => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.floor_f32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Floor => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.floor_f64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Trunc => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.trunc_f32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Trunc => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.trunc_f64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Nearest => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.nearbyint_f32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Nearest => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.nearbyint_f64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Abs => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.fabs_f32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Abs => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.fabs_f64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F32Neg | Operator::F64Neg => {
                let input = state.pop1()?.into_float_value();
                let res = builder.build_float_neg(input, &state.var_name());
                state.push1(res);
            }
            Operator::F32Copysign => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.copysign_f32, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }
            Operator::F64Copysign => {
                let input = state.pop1()?;
                let res = builder
                    .build_call(intrinsics.copysign_f64, &[input], &state.var_name())
                    .try_as_basic_value()
                    .left()
                    .unwrap();
                state.push1(res);
            }

            /***************************
             * Integer Comparison instructions.
             * https://github.com/sunfishcode/wasm-reference-manual/blob/master/WebAssembly.md#integer-comparison-instructions
             ***************************/
            Operator::I32Eq | Operator::I64Eq => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::EQ, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32Ne | Operator::I64Ne => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::NE, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32LtS | Operator::I64LtS => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::SLT, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32LtU | Operator::I64LtU => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::ULT, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32LeS | Operator::I64LeS => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::SLE, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32LeU | Operator::I64LeU => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::ULE, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32GtS | Operator::I64GtS => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::SGT, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32GtU | Operator::I64GtU => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::UGT, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32GeS | Operator::I64GeS => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::SGE, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::I32GeU | Operator::I64GeU => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_int_value(), v2.into_int_value());
                let res = builder.build_int_compare(IntPredicate::UGE, v1, v2, &state.var_name());
                state.push1(res);
            }

            /***************************
             * Floating-Point Comparison instructions.
             * https://github.com/sunfishcode/wasm-reference-manual/blob/master/WebAssembly.md#floating-point-comparison-instructions
             ***************************/
            Operator::F32Eq | Operator::F64Eq => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res =
                    builder.build_float_compare(FloatPredicate::OEQ, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Ne | Operator::F64Ne => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res =
                    builder.build_float_compare(FloatPredicate::UNE, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Lt | Operator::F64Lt => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res =
                    builder.build_float_compare(FloatPredicate::OLT, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Le | Operator::F64Le => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res =
                    builder.build_float_compare(FloatPredicate::OLE, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Gt | Operator::F64Gt => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res =
                    builder.build_float_compare(FloatPredicate::OGT, v1, v2, &state.var_name());
                state.push1(res);
            }
            Operator::F32Ge | Operator::F64Ge => {
                let (v1, v2) = state.pop2()?;
                let (v1, v2) = (v1.into_float_value(), v2.into_float_value());
                let res =
                    builder.build_float_compare(FloatPredicate::OGE, v1, v2, &state.var_name());
                state.push1(res);
            }

            /***************************
             * Conversion instructions.
             * https://github.com/sunfishcode/wasm-reference-manual/blob/master/WebAssembly.md#conversion-instructions
             ***************************/
            Operator::I32WrapI64 => {
                let v1 = state.pop1()?.into_int_value();
                let res = builder.build_int_truncate(v1, intrinsics.i32_ty, &state.var_name());
                state.push1(res);
            }
            Operator::I64ExtendSI32 => {
                let v1 = state.pop1()?.into_int_value();
                let res = builder.build_int_s_extend(v1, intrinsics.i64_ty, &state.var_name());
                state.push1(res);
            }
            Operator::I64ExtendUI32 => {
                let v1 = state.pop1()?.into_int_value();
                let res = builder.build_int_z_extend(v1, intrinsics.i64_ty, &state.var_name());
                state.push1(res);
            }
            Operator::I32TruncSF32 | Operator::I32TruncSF64 => {
                let v1 = state.pop1()?.into_float_value();
                let res =
                    builder.build_float_to_signed_int(v1, intrinsics.i32_ty, &state.var_name());
                state.push1(res);
            }
            Operator::I64TruncSF32 | Operator::I64TruncSF64 => {
                let v1 = state.pop1()?.into_float_value();
                let res =
                    builder.build_float_to_signed_int(v1, intrinsics.i64_ty, &state.var_name());
                state.push1(res);
            }
            Operator::I32TruncUF32 | Operator::I32TruncUF64 => {
                let v1 = state.pop1()?.into_float_value();
                let res =
                    builder.build_float_to_unsigned_int(v1, intrinsics.i32_ty, &state.var_name());
                state.push1(res);
            }
            Operator::I64TruncUF32 | Operator::I64TruncUF64 => {
                let v1 = state.pop1()?.into_float_value();
                let res =
                    builder.build_float_to_unsigned_int(v1, intrinsics.i64_ty, &state.var_name());
                state.push1(res);
            }
            Operator::F32DemoteF64 => {
                let v1 = state.pop1()?.into_float_value();
                let res = builder.build_float_trunc(v1, intrinsics.f32_ty, &state.var_name());
                state.push1(res);
            }
            Operator::F64PromoteF32 => {
                let v1 = state.pop1()?.into_float_value();
                let res = builder.build_float_ext(v1, intrinsics.f64_ty, &state.var_name());
                state.push1(res);
            }
            Operator::F32ConvertSI32 | Operator::F32ConvertSI64 => {
                let v1 = state.pop1()?.into_int_value();
                let res =
                    builder.build_signed_int_to_float(v1, intrinsics.f32_ty, &state.var_name());
                state.push1(res);
            }
            Operator::F64ConvertSI32 | Operator::F64ConvertSI64 => {
                let v1 = state.pop1()?.into_int_value();
                let res =
                    builder.build_signed_int_to_float(v1, intrinsics.f64_ty, &state.var_name());
                state.push1(res);
            }
            Operator::F32ConvertUI32 | Operator::F32ConvertUI64 => {
                let v1 = state.pop1()?.into_int_value();
                let res =
                    builder.build_unsigned_int_to_float(v1, intrinsics.f32_ty, &state.var_name());
                state.push1(res);
            }
            Operator::F64ConvertUI32 | Operator::F64ConvertUI64 => {
                let v1 = state.pop1()?.into_int_value();
                let res =
                    builder.build_unsigned_int_to_float(v1, intrinsics.f64_ty, &state.var_name());
                state.push1(res);
            }
            Operator::I32ReinterpretF32
            | Operator::F32ReinterpretI32
            | Operator::I64ReinterpretF64
            | Operator::F64ReinterpretI64 => {
                unimplemented!("waiting on better bitcasting support in inkwell")
            }

            op @ _ => {
                println!("{}", module.print_to_string().to_string());
                unimplemented!("{:?}", op);
            }
        }
    }

    Ok(())
}
