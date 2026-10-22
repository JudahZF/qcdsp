//! Frame-kernel forms: Types 1a, 2a (multifunction), 3a, 12a, 16a, 18a, 21a/21c.
//!
//! Every vector is decoded field by field from the ADI PRM figure. Vectors marked
//! "stock" are byte-identical to words in the decrypted stock core-1 VISA blobs.

mod common;

use qc_asm::{
    AluOp, Compute, DataAccess, Direction, FloatMulAlu, IndexRegister, Instruction, LoopCount,
    LoopMode, MODE1_BDCST1, MODE1_BDCST9, MODE1_PEYEN, MemoryAccess, ModifyRegister, Register,
    UniversalRegister, Update, VisaError, VisaInstruction as V, VisaProgram,
};

fn r(index: u8) -> Register {
    Register::new(index).unwrap()
}

fn i(index: u8) -> IndexRegister {
    IndexRegister::new(index).unwrap()
}

fn m(index: u8) -> ModifyRegister {
    ModifyRegister::new(index).unwrap()
}

fn word(parcels: &[u16]) -> u64 {
    parcels
        .iter()
        .fold(0, |word, &parcel| (word << 16) | u64::from(parcel))
}

fn mul_alu(
    product: u8,
    mul: (u8, u8),
    op: AluOp,
    alu_dst: u8,
    alu: (u8, u8),
) -> Result<Compute, VisaError> {
    Compute::float_mul_alu(FloatMulAlu {
        product: r(product),
        mul_x: r(mul.0),
        mul_y: r(mul.1),
        op,
        alu_dst: r(alu_dst),
        alu_x: r(alu.0),
        alu_y: r(alu.1),
    })
}

fn data(direction: Direction, register: u8, index: u8, modifier: u8) -> DataAccess {
    DataAccess {
        direction,
        register: r(register),
        index: i(index),
        modifier: m(modifier),
    }
}

fn access(
    direction: Direction,
    register: impl Into<UniversalRegister>,
    index: u8,
    modifier: u8,
    update: Update,
) -> MemoryAccess {
    MemoryAccess {
        direction,
        register: register.into(),
        index: i(index),
        modifier: m(modifier),
        update,
    }
}

fn single(instruction: impl Into<V>) -> Compute {
    Compute::single(instruction).unwrap()
}

fn mac(product: u8, mul: (u8, u8), alu_dst: u8, alu: (u8, u8)) -> Compute {
    mul_alu(product, mul, AluOp::Add, alu_dst, alu).unwrap()
}

use Direction::{Load, Store};
use Update::{PostModify, PreModify};

/// (instruction, 48-bit word, PRM syntax / stock evidence).
fn vectors() -> Vec<(V, u64, &'static str)> {
    vec![
        // Type 1a.
        (
            V::DualMove {
                compute: Some(mac(12, (0, 5), 8, (8, 12))),
                dm: data(Load, 0, 4, 4),
                pm: data(Load, 4, 13, 12),
            },
            0x2901_6258_c810,
            "stock x73: F12=F0*F5, F8=F8+F12, R0=DM(I4,M4), R4=PM(I13,M12)",
        ),
        (
            V::DualMove {
                compute: Some(mul_alu(10, (1, 4), AluOp::Sub, 14, (10, 12)).unwrap()),
                dm: data(Store, 3, 4, 4),
                pm: data(Load, 0, 13, 12),
            },
            0x3907_6059_ae48,
            "stock x24: F10=F1*F4, F14=F10-F12, DM(I4,M4)=R3, R0=PM(I13,M12)",
        ),
        (
            V::DualMove {
                compute: Some(mac(13, (0, 4), 15, (11, 13))),
                dm: data(Store, 7, 4, 3),
                pm: data(Store, 2, 12, 12),
            },
            0x38ef_2158_df0d,
            "stock x18: F13=F0*F4, F15=F11+F13, DM(I4,M3)=R7, PM(I12,M12)=R2",
        ),
        (
            V::DualMove {
                compute: Some(single(Instruction::FloatMul {
                    dst: r(8),
                    lhs: r(2),
                    rhs: r(6),
                })),
                dm: data(Load, 12, 3, 6),
                pm: data(Load, 7, 12, 14),
            },
            0x2799_3393_0826,
            "stock x79: F8=F2*F6, R12=DM(I3,M6), R7=PM(I12,M14)",
        ),
        (
            V::DualMove {
                compute: None,
                dm: data(Load, 2, 0, 6),
                pm: data(Store, 2, 8, 12),
            },
            0x21a4_2100_0000,
            "stock x46: R2=DM(I0,M6), PM(I8,M12)=R2",
        ),
        // Type 2a with a multifunction compute.
        (
            V::Compute(mul_alu(8, (1, 7), AluOp::Sub, 12, (8, 12)).unwrap()),
            0x013e_0059_8c70,
            "stock x295: F8=F1*F7, F12=F8-F12",
        ),
        (
            V::Compute(mac(1, (2, 4), 2, (8, 12))),
            0x013e_0058_1280,
            "stock x94: F1=F2*F4, F2=F8+F12",
        ),
        (
            V::Compute(single(Instruction::IntegerSub {
                dst: r(12),
                lhs: r(12),
                rhs: r(12),
            })),
            0x013e_0000_2ccc,
            "R12=R12-R12 (the compute field of stock 553e86002ccc)",
        ),
        // Type 3a.
        (
            V::MemoryMove {
                compute: None,
                access: access(Load, r(2), 1, 5, PostModify),
            },
            0x537e_0100_0000,
            "stock 0x5ba SIMD loop body: R2=DM(I1,M5)",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Store, r(2), 5, 6, PostModify),
            },
            0x5bbe_8100_0000,
            "stock x561: DM(I5,M6)=R2",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Load, r(2), 4, 6, PostModify),
            },
            0x59be_0100_0000,
            "R2=DM(I4,M6); selache misreads this width (see README)",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Load, r(1), 12, 13, PostModify),
            },
            0x597f_0080_0000,
            "stock x30: R1=PM(I12,M13), G=1",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Store, r(12), 15, 12, PostModify),
            },
            0x5f3f_8600_0000,
            "stock x65: PM(I15,M12)=R12, G=1",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Load, i(12), 6, 7, PreModify),
            },
            0x4dfe_0e00_0000,
            "stock x217: I12=DM(M7,I6), pre-modify, DAG2 ureg",
        ),
        (
            V::MemoryMove {
                compute: Some(mac(8, (1, 4), 12, (8, 12))),
                access: access(Load, r(4), 0, 6, PostModify),
            },
            0x51be_0258_8c40,
            "stock x84: F8=F1*F4, F12=F8+F12, R4=DM(I0,M6)",
        ),
        (
            V::MemoryMove {
                compute: Some(single(V::IntegerSubOne {
                    dst: r(2),
                    src: r(2),
                })),
                access: access(Load, r(0), 3, 6, PostModify),
            },
            0x57be_0002_a220,
            "stock x256: R2=R2-1, R0=DM(I3,M6)",
        ),
        (
            V::MemoryMove {
                compute: Some(single(Instruction::IntegerSub {
                    dst: r(12),
                    lhs: r(12),
                    rhs: r(12),
                })),
                access: access(Store, r(12), 2, 4, PostModify),
            },
            0x553e_8600_2ccc,
            "stock 0x1a08: R12=R12-R12, DM(I2,M4)=R12 (store reads the old R12)",
        ),
        // Type 16a.
        (
            V::StoreImmediate {
                index: i(7),
                modifier: m(7),
                value: 0xf8_38ca,
            },
            0x9fc0_00f8_38ca,
            "stock x76117 (all I7/M7): DM(I7,M7)=0xF838CA, same as PushImmediate",
        ),
        (
            V::StoreImmediate {
                index: i(4),
                modifier: m(0),
                value: 19304,
            },
            0x9800_0000_4b68,
            "PRM example DM(I4,M0)=19304; selas assembles the same word",
        ),
        (
            V::StoreImmediate {
                index: i(14),
                modifier: m(11),
                value: 0x1234,
            },
            0x9ce0_0000_1234,
            "PRM example PM(I14,M11)=0x1234, G=1; selas assembles the same word",
        ),
        // Type 18a.
        (
            V::BitSetMode1 { mask: MODE1_PEYEN },
            0x1402_0020_0000,
            "stock x4538: BIT SET MODE1 0x200000",
        ),
        (
            V::BitClearMode1 { mask: MODE1_PEYEN },
            0x1422_0020_0000,
            "stock x4228: BIT CLR MODE1 0x200000",
        ),
        (
            V::BitSetMode1 {
                mask: MODE1_PEYEN | MODE1_BDCST9,
            },
            0x1402_0060_0000,
            "stock x56: BIT SET MODE1 0x600000",
        ),
        (
            V::BitClearMode1 {
                mask: MODE1_PEYEN | MODE1_BDCST9 | MODE1_BDCST1,
            },
            0x1422_00e0_0000,
            "stock x51: BIT CLR MODE1 0xE00000",
        ),
        // Type 21a.
        (V::WideNop, 0, "stock x2984: 48-bit NOP"),
        // DAG2 registers in the existing Type 17a / 19a forms.
        (
            V::LoadImmediate {
                dst: i(12).into(),
                value: 0x3f80_0000,
            },
            0x0f1c_3f80_0000,
            "stock x656: I12=0x3F800000",
        ),
        (
            V::ModifyIndex {
                index: i(9),
                offset: 0x18,
            },
            0x15c1_0000_0018,
            "stock x17: I9=MODIFY(I9,0x18)(NW)",
        ),
    ]
}

#[test]
fn documented_words_and_widths() {
    for (instruction, expected, evidence) in vectors() {
        let parcels = instruction.parcels().unwrap();
        assert_eq!(parcels.len(), 3, "{evidence}");
        assert_eq!(word(&parcels), expected, "{evidence}: {instruction:?}");
    }
    assert_eq!(V::Nop.parcels().unwrap(), [0x0001]);
    // Existing Type 5b transfers already take DAG2 registers (stock x222, x831, x34).
    for (dst, src, expected) in [
        (i(12).into(), r(4).into(), [0x707e, 0x0e3f]),
        (i(4).into(), i(15).into(), [0x71ff, 0x8a3f]),
        (m(12).into(), r(2).into(), [0x703f, 0x163f]),
    ] {
        assert_eq!(V::Transfer { dst, src }.parcels().unwrap(), expected);
    }
}

#[test]
fn compute_fields_and_operand_groups() {
    assert_eq!(mac(12, (0, 5), 8, (8, 12)).field(), 0x58_c810);
    assert_eq!(
        single(V::IntegerCompareUnsigned {
            lhs: r(12),
            rhs: r(0)
        })
        .field(),
        0x00_b0c0
    );
    assert_eq!(
        Compute::single(Instruction::Move {
            dst: r(0),
            src: r(1)
        }),
        Err(VisaError::NotACompute)
    );
    assert_eq!(
        Compute::single(V::Nop),
        Err(VisaError::NotACompute),
        "only compute forms have a compute field"
    );
    let groups = [
        ("multiplier X input", "F0-F3", (4, 4), (8, 12), 4),
        ("multiplier Y input", "F4-F7", (0, 8), (8, 12), 8),
        ("ALU X input", "F8-F11", (0, 4), (12, 12), 12),
        ("ALU Y input", "F12-F15", (0, 4), (8, 11), 11),
    ];
    for (operand, allowed, mul, alu, register) in groups {
        assert_eq!(
            mul_alu(1, mul, AluOp::Add, 2, alu),
            Err(VisaError::MultifunctionOperand {
                operand,
                register,
                allowed,
            })
        );
    }
    // PRM 05.48: parallel destinations must differ.
    assert_eq!(
        mul_alu(3, (0, 4), AluOp::Sub, 3, (8, 12)),
        Err(VisaError::DuplicateDestination(3))
    );
}

#[test]
fn dag_and_destination_rules() {
    let compute = Some(mac(8, (0, 4), 12, (8, 12)));
    let dual = |dm, pm| V::DualMove { compute, dm, pm }.parcels();
    assert_eq!(
        dual(data(Load, 0, 8, 4), data(Load, 1, 12, 12)),
        Err(VisaError::InvalidDmIndex(8))
    );
    assert_eq!(
        dual(data(Load, 0, 4, 12), data(Load, 1, 12, 12)),
        Err(VisaError::InvalidDmModifier(12))
    );
    assert_eq!(
        dual(data(Load, 0, 4, 4), data(Load, 1, 4, 12)),
        Err(VisaError::InvalidPmIndex(4))
    );
    assert_eq!(
        dual(data(Load, 0, 4, 4), data(Load, 1, 12, 4)),
        Err(VisaError::InvalidPmModifier(4))
    );
    assert_eq!(
        dual(data(Load, 5, 4, 4), data(Load, 5, 12, 12)),
        Err(VisaError::DuplicateDestination(5))
    );
    assert_eq!(
        dual(data(Load, 12, 4, 4), data(Load, 1, 12, 12)),
        Err(VisaError::DuplicateDestination(12))
    );
    // A store may read the register the compute writes.
    assert!(dual(data(Store, 8, 4, 4), data(Store, 12, 12, 12)).is_ok());

    let move_one = |compute, access| V::MemoryMove { compute, access }.parcels();
    assert_eq!(
        move_one(None, access(Load, r(0), 4, 12, PostModify)),
        Err(VisaError::MixedDag {
            index: 4,
            modifier: 12
        })
    );
    // UREGXDAG1 / UREGXDAG2: post-modify and stores avoid the addressing DAG.
    assert_eq!(
        move_one(None, access(Load, i(2), 6, 7, PostModify)),
        Err(VisaError::SameDagRegister(0x12))
    );
    assert_eq!(
        move_one(None, access(Store, m(9), 12, 12, PreModify)),
        Err(VisaError::SameDagRegister(0x29))
    );
    assert!(move_one(None, access(Load, i(2), 6, 7, PreModify)).is_ok());
    assert!(move_one(None, access(Load, i(2), 12, 12, PostModify)).is_ok());
    assert_eq!(
        move_one(compute, access(Load, r(12), 0, 6, PostModify)),
        Err(VisaError::DuplicateDestination(12))
    );
    assert_eq!(
        V::StoreImmediate {
            index: i(9),
            modifier: m(1),
            value: 0
        }
        .parcels(),
        Err(VisaError::MixedDag {
            index: 9,
            modifier: 1
        })
    );
}

/// Stock core-1 0x19de..0x1a0d: a SIMD broadcast MAC kernel with an F1-active
/// `LCNTR=0x10` loop around an F1-active `LCNTR=R15` loop, rebuilt word for word.
#[test]
fn stock_nested_mac_loop_is_reproduced() {
    let r12_clear = single(Instruction::IntegerSub {
        dst: r(12),
        lhs: r(12),
        rhs: r(12),
    });
    let r13_clear = single(Instruction::IntegerSub {
        dst: r(13),
        lhs: r(13),
        rhs: r(13),
    });
    let mac0 = mac(8, (0, 4), 12, (8, 12));
    let mac1 = mac(9, (0, 5), 13, (9, 13));
    let even = |compute| V::MemoryMove {
        compute: Some(compute),
        access: access(Load, r(4), 1, 6, PostModify),
    };
    let odd = |compute, pm| V::DualMove {
        compute: Some(compute),
        dm: data(Load, 5, 1, 6),
        pm,
    };
    let weights = data(Load, 0, 8, 8);
    let mut code = VisaProgram::new();
    code.push(V::DualMove {
        compute: Some(r12_clear),
        dm: data(Load, 4, 1, 6),
        pm: weights,
    });
    code.push(V::MemoryMove {
        compute: Some(r13_clear),
        access: access(Load, r(5), 1, 6, PostModify),
    });
    code.push(even(single(Instruction::FloatMul {
        dst: r(8),
        lhs: r(0),
        rhs: r(4),
    })));
    code.push(odd(
        single(Instruction::FloatMul {
            dst: r(9),
            lhs: r(0),
            rhs: r(5),
        }),
        weights,
    ));
    code.loop_begin(LoopCount::Immediate(0x10), LoopMode::F1Active);
    code.loop_begin(LoopCount::Register(r(15).into()), LoopMode::F1Active);
    code.push(even(mac0));
    code.push(odd(mac1, weights));
    code.loop_end();
    code.push(even(mac0));
    code.push(odd(mac1, weights));
    code.push(V::DualMove {
        compute: Some(mac0),
        dm: data(Load, 4, 1, 6),
        pm: data(Load, 1, 8, 12),
    });
    code.push(odd(mac1, weights));
    code.push(even(mac0));
    code.push(odd(mac1, weights));
    code.push(V::MemoryMove {
        compute: Some(r12_clear),
        access: access(Store, r(12), 2, 4, PostModify),
    });
    code.push(V::MemoryMove {
        compute: Some(r13_clear),
        access: access(Store, r(13), 3, 4, PostModify),
    });
    code.loop_end();
    let stock: [u64; 16] = [
        0x2388_0000_2ccc,
        0x53be_0280_2ddd,
        0x53be_0213_0804,
        0x238a_0013_0905,
        0x0c00_1080_0021,
        0x0d0f_0080_0006,
        0x53be_0258_8c00,
        0x238a_0058_9d15,
        0x53be_0258_8c00,
        0x238a_0058_9d15,
        0x2388_20d8_8c00,
        0x238a_0058_9d15,
        0x53be_0258_8c00,
        0x238a_0058_9d15,
        0x553e_8600_2ccc,
        0x573e_8680_2ddd,
    ];
    let parcels = code.parcels().unwrap();
    let words: Vec<u64> = parcels.chunks(3).map(word).collect();
    assert_eq!(words, stock);
    assert_eq!(code.instruction_count(), 16);
    assert_eq!(code.parcel_count().unwrap(), 48);
}

#[test]
fn loop_end_is_the_first_parcel_of_the_last_body_instruction() {
    // Stock 0x5b3..0x5c9 uses this frame: BIT SET; NOP; DO (PC,0xf) (F); 5 x 48-bit; BIT CLR.
    let mut code = VisaProgram::new();
    code.push(V::BitSetMode1 { mask: MODE1_PEYEN });
    code.push(V::Nop);
    code.loop_begin(LoopCount::Immediate(16), LoopMode::F1Active);
    for _ in 0..5 {
        code.push(V::WideNop);
    }
    code.loop_end();
    code.push(V::BitClearMode1 { mask: MODE1_PEYEN });
    let parcels = code.parcels().unwrap();
    assert_eq!(
        parcels[..7],
        [0x1402, 0x0020, 0x0000, 0x0001, 0x0c00, 0x1080, 0x000f]
    );
    assert_eq!(parcels[parcels.len() - 3..], [0x1422, 0x0020, 0x0000]);

    let mut e2 = VisaProgram::new();
    e2.loop_begin(LoopCount::Immediate(2), LoopMode::E2Active);
    for _ in 0..6 {
        e2.push(V::WideNop);
    }
    e2.loop_end();
    // Stock x209: LCNTR=2, DO (PC,0x12) UNTIL LCE (E2-active, six 48-bit instructions).
    assert_eq!(e2.parcels().unwrap()[..3], [0x0c00, 0x0200, 0x0012]);
}

#[test]
fn loop_rules() {
    let build = |body: &dyn Fn(&mut VisaProgram)| {
        let mut code = VisaProgram::new();
        code.loop_begin(LoopCount::Immediate(4), LoopMode::F1Active);
        body(&mut code);
        code.loop_end();
        code.parcels()
    };
    // The 12th instruction from the end may be short; the last 11 may not.
    assert!(
        build(&|code| {
            code.push(V::Nop);
            (0..11).for_each(|_| code.push(V::WideNop));
        })
        .is_ok()
    );
    assert_eq!(
        build(&|code| {
            code.push(V::Nop);
            (0..10).for_each(|_| code.push(V::WideNop));
        }),
        Err(VisaError::LoopTailNotFullWidth(3))
    );
    assert_eq!(
        build(&|code| {
            code.push(V::WideNop);
            code.push(V::Transfer {
                dst: i(4).into(),
                src: r(4).into(),
            });
            code.push(V::WideNop);
        }),
        Err(VisaError::LoopTailNotFullWidth(6))
    );
    assert_eq!(build(&|_| {}), Err(VisaError::EmptyLoop));
    assert_eq!(
        build(&|code| {
            code.push(V::WideNop);
            code.label("inside");
            code.push(V::WideNop);
        }),
        Err(VisaError::LabelInLoop("inside".into()))
    );
    assert_eq!(
        build(&|code| code.jump(qc_asm::Condition::Eq, "out")),
        Err(VisaError::BranchInLoop)
    );
    assert_eq!(
        build(&|code| code.push(V::CJump { target: 0x10 })),
        Err(VisaError::BranchInLoop)
    );
    // Nested loops may not share an end instruction.
    assert_eq!(
        build(&|code| {
            code.push(V::WideNop);
            code.loop_begin(LoopCount::Immediate(2), LoopMode::F1Active);
            code.push(V::WideNop);
            code.loop_end();
        }),
        Err(VisaError::SharedLoopEnd)
    );
    let mut unclosed = VisaProgram::new();
    unclosed.loop_begin(LoopCount::Immediate(1), LoopMode::E2Active);
    unclosed.push(V::WideNop);
    assert_eq!(unclosed.parcels(), Err(VisaError::UnclosedLoop));
    let mut unmatched = VisaProgram::new();
    unmatched.loop_end();
    assert_eq!(unmatched.parcels(), Err(VisaError::UnmatchedLoopEnd));
    let mut zero = VisaProgram::new();
    zero.loop_begin(LoopCount::Immediate(0), LoopMode::E2Active);
    zero.push(V::WideNop);
    zero.loop_end();
    assert_eq!(zero.parcels(), Err(VisaError::LoopCountZero));
}

#[test]
#[ignore = "set SELACHE to a directory containing standalone selas and seldump"]
fn standalone_selache_decodes_new_forms() {
    // Only forms selache decodes correctly: it drops G for Type 3a PM, prints `mrf+` for
    // the float multifunction, and misreads widths of Type 16a other than 0x9fc0 and of
    // Type 3a whose first parcel is 0x5980..0x59ff.
    let compute_sub = single(Instruction::IntegerSub {
        dst: r(12),
        lhs: r(12),
        rhs: r(12),
    });
    let expected: Vec<(V, &str)> = vec![
        (
            V::DualMove {
                compute: None,
                dm: data(Load, 2, 0, 6),
                pm: data(Store, 2, 8, 12),
            },
            "r2=dm (i0, m6), pm (i8, m12)=r2",
        ),
        (
            V::DualMove {
                compute: Some(single(Instruction::FloatMul {
                    dst: r(8),
                    lhs: r(2),
                    rhs: r(6),
                })),
                dm: data(Load, 12, 3, 6),
                pm: data(Load, 7, 12, 14),
            },
            "f8=f2*f6, r12=dm (i3, m6), r7=pm (i12, m14)",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Load, r(2), 1, 5, PostModify),
            },
            "r2=dm (i1,m5)",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Store, r(2), 5, 6, PostModify),
            },
            "dm (i5,m6)=r2",
        ),
        (
            V::MemoryMove {
                compute: None,
                access: access(Load, i(12), 6, 7, PreModify),
            },
            "i12=dm (m7,i6)",
        ),
        (
            V::MemoryMove {
                compute: Some(single(V::IntegerSubOne {
                    dst: r(2),
                    src: r(2),
                })),
                access: access(Load, r(0), 3, 6, PostModify),
            },
            "r2=r2-1 , r0=dm (i3,m6)",
        ),
        (
            V::MemoryMove {
                compute: Some(compute_sub),
                access: access(Store, r(12), 2, 4, PostModify),
            },
            "r12=r12-r12 , dm (i2,m4)=r12",
        ),
        (V::Compute(compute_sub), "r12=r12-r12"),
        (
            V::StoreImmediate {
                index: i(7),
                modifier: m(7),
                value: 0xf8_38ca,
            },
            "dm (i7,m7)=0xf838ca",
        ),
        (
            V::BitSetMode1 { mask: MODE1_PEYEN },
            "bit set mode1 0x200000",
        ),
        (
            V::BitClearMode1 { mask: MODE1_PEYEN },
            "bit clr mode1 0x200000",
        ),
        (
            V::BitSetMode1 {
                mask: MODE1_PEYEN | MODE1_BDCST1,
            },
            "bit set mode1 0xa00000",
        ),
        (V::Nop, "nop"),
        (V::WideNop, "nop"),
        (
            V::LoadImmediate {
                dst: i(12).into(),
                value: 0x3f80_0000,
            },
            "i12=0x3f800000",
        ),
        (
            V::ModifyIndex {
                index: i(9),
                offset: 0x18,
            },
            "i9=modify (i9,0x18)(nw)",
        ),
        (
            V::Transfer {
                dst: m(12).into(),
                src: r(2).into(),
            },
            "m12=r2",
        ),
    ];
    let mut program = VisaProgram::new();
    let mut lines = Vec::new();
    for (instruction, mnemonic) in expected {
        lines.push((instruction.parcels().unwrap(), mnemonic));
        program.push(instruction);
    }
    // An E2-active immediate loop (selache prints the F1 mode bit as part of reladdr).
    program.loop_begin(LoopCount::Immediate(0x10), LoopMode::E2Active);
    lines.push((
        vec![0x0c00, 0x1000, 0x0006],
        "lcntr=0x10 , do (pc,0x6)until lce",
    ));
    for _ in 0..2 {
        program.push(V::WideNop);
        lines.push((vec![0, 0, 0], "nop"));
    }
    program.loop_end();
    common::assert_selache_decodes(&program.encode().unwrap(), &lines);
}
