use module::*;
use cfgraph::*;
use super::llvm;
use opcode::Opcode;

fn generate_function_name(id: usize) -> String {
    format!("Wfunc_{}", id)
}

pub struct Compiler<'a> {
    _context: llvm::Context,
    source_module: &'a Module
}

struct CompilerIntrinsics {
    check_stack: llvm::Function
}

impl CompilerIntrinsics {
    pub fn new(ctx: &llvm::Context, m: &llvm::Module) -> CompilerIntrinsics {
        CompilerIntrinsics {
            check_stack: Self::build_check_stack(ctx, m)
        }
    }

    extern "C" fn stack_check_failed() {
        panic!("Stack check failed");
    }

    fn build_check_stack(ctx: &llvm::Context, m: &llvm::Module) -> llvm::Function {
        let f: llvm::Function = llvm::Function::new(
            ctx,
            m,
            "check_stack",
            llvm::Type::function(
                ctx,
                llvm::Type::void(ctx),
                &[
                    llvm::Type::int32(ctx), // current
                    llvm::Type::int32(ctx), // lower (inclusive)
                    llvm::Type::int32(ctx) // upper (exclusive)
                ]
            )
        );
        let initial_bb = llvm::BasicBlock::new(&f);
        let check_ok_bb = llvm::BasicBlock::new(&f);
        let check_failed_bb = llvm::BasicBlock::new(&f);

        unsafe {
            let builder = initial_bb.builder();
            let upper_cmp = builder.build_icmp(
                llvm::LLVMIntSLT,
                f.get_param(0),
                f.get_param(1)
            );
            let lower_cmp = builder.build_icmp(
                llvm::LLVMIntSGE,
                f.get_param(0),
                f.get_param(2)
            );
            let cmp_result = builder.build_or(upper_cmp, lower_cmp);
            builder.build_cond_br(cmp_result, &check_failed_bb, &check_ok_bb);
        }

        unsafe {
            let builder = check_ok_bb.builder();
            builder.build_ret_void();
        }

        unsafe {
            let builder = check_failed_bb.builder();
            let call_target = builder.build_cast(
                llvm::LLVMOpcode::LLVMIntToPtr,
                builder.build_const_int(
                    llvm::Type::int64(ctx),
                    (Self::stack_check_failed as usize) as u64,
                    false
                ),
                llvm::Type::pointer(
                    llvm::Type::function(
                        ctx,
                        llvm::Type::void(ctx),
                        &[]
                    )
                )
            );
            builder.build_call_raw(call_target, &[]);
            builder.build_ret_void();
        }

        f.verify();

        f
    }
}

struct FunctionId(usize);

impl<'a> Compiler<'a> {
    pub fn new(m: &'a Module, ctx: llvm::Context) -> OptimizeResult<Compiler<'a>> {
        Ok(Compiler {
            _context: ctx.clone(),
            source_module: m
        })
    }

    pub fn compile(&self) -> OptimizeResult<llvm::Module> {
        let target_module = llvm::Module::new(&self._context, "".into());
        let intrinsics = CompilerIntrinsics::new(&self._context, &target_module);

        let target_functions: Vec<llvm::Function> = Self::gen_function_defs(
            &self._context,
            self.source_module,
            &target_module
        )?;

        for i in 0..target_functions.len() {
            Self::gen_function_body(
                &self._context,
                &intrinsics,
                self.source_module,
                &target_module,
                &target_functions,
                FunctionId(i)
            )?;
            //println!("{}", target_functions[i].to_string_leaking());
            target_functions[i].verify();
        }

        Ok(target_module)
    }

    fn gen_function_defs(
        ctx: &llvm::Context,
        source_module: &Module,
        target_module: &llvm::Module
    ) -> OptimizeResult<Vec<llvm::Function>> {
        let mut result: Vec<llvm::Function> = Vec::with_capacity(source_module.functions.len());

        for (i, f) in source_module.functions.iter().enumerate() {
            let Type::Func(ref ty_args, ref ty_ret) = source_module.types[f.typeidx as usize];

            let mut target_ty_args: Vec<llvm::Type> = vec! [ 
                //llvm::Type::pointer(llvm::Type::void(ctx))
            ];

            target_ty_args.extend(
                ty_args.iter()
                    .map(|a| a.to_llvm_type(ctx))
            );

            let target_f: llvm::Function = llvm::Function::new(
                ctx,
                target_module,
                generate_function_name(i).as_str(),
                llvm::Type::function(
                    ctx,
                    if ty_ret.len() == 0 {
                        llvm::Type::void(ctx)
                    } else if ty_ret.len() == 1 {
                        ty_ret[0].to_llvm_type(ctx)
                    } else {
                        return Err(OptimizeError::Custom("Invalid number of return values".into()));
                    },
                    target_ty_args.as_slice()
                )
            );

            result.push(target_f);
        }

        Ok(result)
    }

    fn gen_function_body(
        ctx: &llvm::Context,
        intrinsics: &CompilerIntrinsics,
        source_module: &Module,
        target_module: &llvm::Module,
        target_functions: &[llvm::Function],
        this_function: FunctionId
    ) -> OptimizeResult<()> {
        const STACK_SIZE: usize = 32;
        const TAG_I32: i32 = 0x01;
        const TAG_I64: i32 = 0x02;

        let source_func = &source_module.functions[this_function.0];
        let Type::Func(ref source_func_args_ty, ref source_func_ret_ty) =
            source_module.types[source_func.typeidx as usize];
        let target_func = &target_functions[this_function.0];

        let source_cfg = CFGraph::from_function(&source_func.body.opcodes)?;
        source_cfg.validate()?;

        let get_stack_elem_type = || llvm::Type::int64(ctx);
        let get_stack_array_type = || llvm::Type::array(
            ctx,
            get_stack_elem_type(),
            STACK_SIZE
        );

        let initializer_bb = llvm::BasicBlock::new(target_func);
        let stack_base;
        let stack_index;

        unsafe {
            let builder = initializer_bb.builder();
            stack_base = builder.build_alloca(
                get_stack_array_type()
            );
            stack_index = builder.build_alloca(
                llvm::Type::int32(ctx)
            );
            builder.build_store(
                builder.build_const_int(
                    llvm::Type::int32(ctx),
                    0,
                    false
                ),
                stack_index
            );
        }

        let build_stack_pop = |builder: &llvm::Builder| -> llvm::LLVMValueRef {
            unsafe {
                let cur_stack_index = builder.build_load(
                    stack_index
                );
                builder.build_call(
                    &intrinsics.check_stack,
                    &[
                        cur_stack_index,
                        builder.build_const_int(
                            llvm::Type::int32(ctx),
                            1,
                            false
                        ),
                        builder.build_const_int(
                            llvm::Type::int32(ctx),
                            (STACK_SIZE + 1) as _,
                            false
                        )
                    ]
                );
                let new_stack_index = builder.build_sub(
                    cur_stack_index,
                    builder.build_const_int(
                        llvm::Type::int32(ctx),
                        1,
                        false
                    )
                );
                builder.build_store(
                    new_stack_index,
                    stack_index
                );
                let ret = builder.build_load(
                    builder.build_gep(
                        stack_base,
                        &[
                            builder.build_const_int(
                                llvm::Type::int32(ctx),
                                0,
                                false
                            ),
                            new_stack_index
                        ]
                    )
                );
                ret
            }
        };

        let build_stack_push = |builder: &llvm::Builder, v: llvm::LLVMValueRef| {
            unsafe {
                let cur_stack_index = builder.build_load(
                    stack_index
                );
                builder.build_call(
                    &intrinsics.check_stack,
                    &[
                        cur_stack_index,
                        builder.build_const_int(
                            llvm::Type::int32(ctx),
                            0,
                            false
                        ),
                        builder.build_const_int(
                            llvm::Type::int32(ctx),
                            STACK_SIZE as _,
                            false
                        )
                    ]
                );
                builder.build_store(
                    v,
                    builder.build_gep(
                        stack_base,
                        &[
                            builder.build_const_int(
                                llvm::Type::int32(ctx),
                                0,
                                false
                            ),
                            cur_stack_index
                        ]
                    )
                );
                builder.build_store(
                    builder.build_add(
                        cur_stack_index,
                        builder.build_const_int(
                            llvm::Type::int32(ctx),
                            1,
                            false
                        )
                    ),
                    stack_index
                );
            }
        };

        let target_basic_blocks: Vec<llvm::BasicBlock<'_>> = (0..source_cfg.blocks.len())
            .map(|_| llvm::BasicBlock::new(target_func))
            .collect();

        for (i, bb) in source_cfg.blocks.iter().enumerate() {
            let target_bb = &target_basic_blocks[i];

            for op in &bb.opcodes {
                unsafe {
                    match *op {
                        Opcode::I32Const(v) => {
                            let builder = target_bb.builder();
                            let v = builder.build_cast(
                                llvm::LLVMOpcode::LLVMZExt,
                                builder.build_const_int(
                                    llvm::Type::int32(ctx),
                                    v as _,
                                    false
                                ),
                                llvm::Type::int64(ctx)
                            );
                            build_stack_push(&builder, v);
                        },
                        _ => panic!("Opcode not implemented: {:?}", op)
                    }
                }
            }
        }

        for (i, bb) in source_cfg.blocks.iter().enumerate() {
            let target_bb = &target_basic_blocks[i];
            let builder = target_bb.builder();

            unsafe { 
                match *bb.br.as_ref().unwrap() {
                    Branch::Jmp(id) => {
                        builder.build_br(&target_basic_blocks[id.0]);
                    },
                    Branch::JmpEither(a, b) => {
                        let v = build_stack_pop(&builder);
                        let cond = builder.build_icmp(
                            llvm::LLVMIntNE,
                            v,
                            builder.build_const_int(
                                llvm::Type::int64(ctx),
                                0,
                                false
                            )
                        );
                        builder.build_cond_br(
                            cond,
                            &target_basic_blocks[a.0],
                            &target_basic_blocks[b.0]
                        );
                    },
                    Branch::JmpTable(ref targets, otherwise) => {
                        let v = build_stack_pop(&builder);

                        let targets: Vec<(llvm::LLVMValueRef, &llvm::BasicBlock)> = targets.iter()
                            .enumerate()
                            .map(|(i, t)| {
                                (
                                    builder.build_const_int(
                                        llvm::Type::int64(ctx),
                                        i as _,
                                        false
                                    ),
                                    &target_basic_blocks[t.0]
                                )
                            })
                            .collect();

                        builder.build_switch(
                            v,
                            &targets,
                            &target_basic_blocks[otherwise.0]
                        );
                    },
                    Branch::Return => {
                        if source_func_ret_ty.len() == 0 {
                            builder.build_ret_void();
                        } else {
                            let v = build_stack_pop(&builder);
                            builder.build_ret(v);
                        }
                    }
                }
            }
        }

        unsafe {
            if target_basic_blocks.len() == 0 {
                initializer_bb.builder().build_ret_void();
            } else {
                initializer_bb.builder().build_br(&target_basic_blocks[0]);
            }
        }

        Ok(())
    }
}

impl ValType {
    fn to_llvm_type(&self, ctx: &llvm::Context) -> llvm::Type {
        match *self {
            ValType::I32 | ValType::I64 => llvm::Type::int64(ctx),
            //ValType::I64 => llvm::Type::int64(ctx),
            ValType::F32 => llvm::Type::float32(ctx),
            ValType::F64 => llvm::Type::float64(ctx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_jit() {
        let mut m = Module::default();
        m.types.push(Type::Func(vec! [], vec! [ ValType::I32 ]));
        m.functions.push(Function {
            name: None,
            typeidx: 0,
            locals: Vec::new(),
            body: FunctionBody {
                opcodes: vec! [
                    Opcode::I32Const(42),
                    Opcode::Return
                ]
            }
        });
        let compiler = Compiler::new(&m, llvm::Context::new()).unwrap();
        let target_module = compiler.compile().unwrap();

        let ee = llvm::ExecutionEngine::new(target_module);

        let f: extern "C" fn () -> i64 = unsafe {
            ::std::mem::transmute(ee.get_function_address(
                generate_function_name(0).as_str()
            ).unwrap())
        };
        let ret = f();
        assert_eq!(ret, 42);
    }
}