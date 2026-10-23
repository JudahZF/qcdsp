use qc_asm::{
    Condition, IndexRegister, Instruction, LEAF_RETURN_PARCELS, ModifyRegister, Register,
    UniversalRegister, VisaError, VisaInstruction, VisaProgram,
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

fn u(code: u8) -> UniversalRegister {
    UniversalRegister::new(code).unwrap()
}

// Independently derived from ADI's diagrams, not from the encoder under test.
// Short transfer/stack vectors and the return sequence are stock-identical.
fn vectors() -> Vec<(VisaInstruction, Vec<u16>, &'static str)> {
    vec![
        (
            VisaInstruction::IntegerCompareUnsigned {
                lhs: r(12),
                rhs: r(0),
            },
            vec![0x013e, 0x0000, 0xb0c0],
            "COMPU(R12,R0)",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::UnsignedGt,
                offset: 8,
                delayed: false,
            },
            vec![0x0724, 0, 8],
            "IF GT JUMP(PC,0x8)",
        ),
        (
            Instruction::FloatAdd {
                dst: r(0),
                lhs: r(1),
                rhs: r(2),
            }
            .into(),
            vec![0x013e, 0x0008, 0x1012],
            "F0=F1+F2",
        ),
        (
            Instruction::FloatSub {
                dst: r(3),
                lhs: r(4),
                rhs: r(5),
            }
            .into(),
            vec![0x013e, 0x0008, 0x2345],
            "F3=F4-F5",
        ),
        (
            Instruction::FloatMul {
                dst: r(1),
                lhs: r(1),
                rhs: r(8),
            }
            .into(),
            vec![0x013e, 0x0013, 0x0118],
            "F1=F1*F8",
        ),
        (
            Instruction::FloatMin {
                dst: r(9),
                lhs: r(10),
                rhs: r(11),
            }
            .into(),
            vec![0x013e, 0x000e, 0x19ab],
            "F9=MIN(F10,F11)",
        ),
        (
            Instruction::FloatMax {
                dst: r(12),
                lhs: r(13),
                rhs: r(14),
            }
            .into(),
            vec![0x013e, 0x000e, 0x2cde],
            "F12=MAX(F13,F14)",
        ),
        (
            Instruction::FloatNeg {
                dst: r(15),
                src: r(1),
            }
            .into(),
            vec![0x013e, 0x000a, 0x2f10],
            "F15=-F1",
        ),
        (
            Instruction::FloatAbs {
                dst: r(2),
                src: r(15),
            }
            .into(),
            vec![0x013e, 0x000b, 0x02f0],
            "F2=ABS F15",
        ),
        (
            Instruction::FloatPass {
                dst: r(1),
                src: r(8),
            }
            .into(),
            vec![0x013e, 0x000a, 0x1180],
            "F1=PASS F8",
        ),
        (
            Instruction::Move {
                dst: r(4),
                src: r(15),
            }
            .into(),
            vec![0x70ff, 0x8200, 0x0000],
            "R4=R15",
        ),
        (
            Instruction::LoadImmediate {
                dst: r(1),
                value: 0x3d4c_cccd,
            }
            .into(),
            vec![0x0f01, 0x3d4c, 0xcccd],
            "R1=0x3D4CCCCD",
        ),
        (
            Instruction::LoadDm {
                dst: r(3),
                address: 0x1234_5678,
            }
            .into(),
            vec![0x1003, 0x1234, 0x5678],
            "R3=DM(0x12345678)",
        ),
        (
            Instruction::StoreDm {
                src: r(12),
                address: 0x8765_4321,
            }
            .into(),
            vec![0x110c, 0x8765, 0x4321],
            "DM(0x87654321)=R12",
        ),
        (
            VisaInstruction::Transfer {
                dst: i(4).into(),
                src: r(4).into(),
            },
            vec![0x707e, 0x0a3f],
            "I4=R4",
        ),
        (
            VisaInstruction::Transfer {
                dst: r(2).into(),
                src: i(5).into(),
            },
            vec![0x717e, 0x813f],
            "R2=I5",
        ),
        (
            VisaInstruction::Transfer {
                dst: i(4).into(),
                src: i(3).into(),
            },
            vec![0x713f, 0x8a3f],
            "I4=I3",
        ),
        (
            VisaInstruction::Transfer {
                dst: r(15).into(),
                src: r(15).into(),
            },
            vec![0x70ff, 0x87bf],
            "R15=R15",
        ),
        (
            VisaInstruction::Transfer {
                dst: m(5).into(),
                src: i(15).into(),
            },
            vec![0x71ff, 0x92bf],
            "M5=I15",
        ),
        (
            VisaInstruction::LoadImmediate {
                dst: i(5).into(),
                value: 0x1234_5678,
            },
            vec![0x0f15, 0x1234, 0x5678],
            "I5=0x12345678",
        ),
        (
            VisaInstruction::ModifyIndex {
                index: i(7),
                offset: -10,
            },
            vec![0x1587, 0xffff, 0xfff6],
            "I7=MODIFY(I7,-0xA)(NW)",
        ),
        (
            VisaInstruction::ModifyIndex {
                index: i(15),
                offset: 1,
            },
            vec![0x15c7, 0x0000, 0x0001],
            "I15=MODIFY(I15,0x1)(NW)",
        ),
        (
            VisaInstruction::StoreDmOffset {
                src: r(15).into(),
                index: i(6),
                offset: -3,
            },
            vec![0x9d08, 0x07fd],
            "DM(-0x3,I6)=R15",
        ),
        (
            VisaInstruction::StoreDmOffset {
                src: r(2).into(),
                index: i(6),
                offset: -2,
            },
            vec![0x9d08, 0x017e],
            "DM(-0x2,I6)=R2",
        ),
        (
            VisaInstruction::StoreDmOffset {
                src: r(13).into(),
                index: i(6),
                offset: -5,
            },
            vec![0x9d08, 0x06fb],
            "DM(-0x5,I6)=R13",
        ),
        (
            VisaInstruction::StoreDmOffset {
                src: r(2).into(),
                index: i(6),
                offset: -10,
            },
            vec![0x9d08, 0x0176],
            "DM(-0xA,I6)=R2",
        ),
        (
            VisaInstruction::LoadDmOffset {
                dst: r(15).into(),
                index: i(6),
                offset: -11,
            },
            vec![0x9c08, 0x07f5],
            "R15=DM(-0xB,I6)",
        ),
        (
            VisaInstruction::LoadDmOffset {
                dst: i(0).into(),
                index: i(6),
                offset: -10,
            },
            vec![0x9c08, 0x0876],
            "I0=DM(-0xA,I6)",
        ),
        (
            VisaInstruction::StoreDmOffset {
                src: r(2).into(),
                index: i(5),
                offset: 11,
            },
            vec![0x9b08, 0x010b],
            "DM(0xB,I5)=R2",
        ),
        (
            VisaInstruction::LoadDmOffset {
                dst: r(2).into(),
                index: i(5),
                offset: 0x1fff,
            },
            vec![0xaa02, 0x0000, 0x1fff],
            "R2=DM(0x1FFF,I5)",
        ),
        (
            VisaInstruction::StoreDmOffset {
                src: r(2).into(),
                index: i(5),
                offset: 0x1fff,
            },
            vec![0xab02, 0x0000, 0x1fff],
            "DM(0x1FFF,I5)=R2",
        ),
        (
            VisaInstruction::LoadDmOffset {
                dst: i(0).into(),
                index: i(6),
                offset: -65,
            },
            vec![0xac10, 0xffff, 0xffbf],
            "I0=DM(-0x41,I6)",
        ),
        (
            VisaInstruction::LoadDmPostModify {
                dst: r(2).into(),
                index: i(4),
                modifier: m(6),
            },
            vec![0x9992],
            "R2=DM(I4,M6)",
        ),
        (
            VisaInstruction::StoreDmPostModify {
                src: r(2).into(),
                index: i(4),
                modifier: m(6),
            },
            vec![0x99b2],
            "DM(I4,M6)=R2",
        ),
        (
            VisaInstruction::LoadDmPostModify {
                dst: r(15).into(),
                index: i(7),
                modifier: m(7),
            },
            vec![0x9fdf],
            "R15=DM(I7,M7)",
        ),
        (
            VisaInstruction::LoadDmPreModify {
                dst: i(3).into(),
                index: i(5),
                modifier: m(5),
            },
            vec![0x4b7e, 0x09bf],
            "I3=DM(M5,I5)",
        ),
        (
            VisaInstruction::StoreDmPreModify {
                src: r(2).into(),
                index: i(4),
                modifier: m(6),
            },
            vec![0x49be, 0x813f],
            "DM(M6,I4)=R2",
        ),
        (
            VisaInstruction::LoadDmPreModify {
                dst: r(2).into(),
                index: i(4),
                modifier: m(6),
            },
            vec![0x49be, 0x013f],
            "R2=DM(M6,I4)",
        ),
        (
            VisaInstruction::LoadDmPostModify {
                dst: r(1).into(),
                index: i(4),
                modifier: m(5),
            },
            vec![0x9951],
            "R1=DM(I4,M5)",
        ),
        (
            VisaInstruction::StoreDmPostModify {
                src: r(12).into(),
                index: i(4),
                modifier: m(5),
            },
            vec![0x997c],
            "DM(I4,M5)=R12",
        ),
        (
            VisaInstruction::IntegerSubOne {
                dst: r(2),
                src: r(2),
            },
            vec![0x013e, 0x0002, 0xa220],
            "R2=R2-1",
        ),
        (
            VisaInstruction::IntegerPass {
                dst: r(2),
                src: r(0),
            },
            vec![0x013e, 0x0002, 0x1200],
            "R2=PASS R0",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::Eq,
                offset: 0x2c,
                delayed: false,
            },
            vec![0x0700, 0x0000, 0x002c],
            "IF EQ JUMP(PC,0x2C)",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::Sv,
                offset: 0x13,
                delayed: false,
            },
            vec![0x070e, 0x0000, 0x0013],
            "IF SV JUMP(PC,0x13)",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::Sz,
                offset: 8,
                delayed: false,
            },
            vec![0x0710, 0x0000, 0x0008],
            "IF SZ JUMP(PC,0x8)",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::NotSv,
                offset: 8,
                delayed: false,
            },
            vec![0x072e, 0x0000, 0x0008],
            "IF NOT SV JUMP(PC,0x8)",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::Ne,
                offset: -3,
                delayed: false,
            },
            vec![0x0720, 0x00ff, 0xfffd],
            "IF NE JUMP(PC,-0x3)",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::Always,
                offset: -63,
                delayed: true,
            },
            vec![0x073e, 0x04ff, 0xffc1],
            "JUMP(PC,-0x3F)(DB)",
        ),
        (
            VisaInstruction::JumpRelative {
                condition: Condition::Always,
                offset: 3,
                delayed: false,
            },
            vec![0x073e, 0x0000, 0x0003],
            "JUMP(PC,0x3)",
        ),
        // The stock C call to heap_malloc, digital_delay 0x38c4..0x38c8: CJUMP, push R2,
        // push the return literal (before its kind-7 relocation).
        (
            VisaInstruction::CJump { target: 0x16_adfa },
            vec![0x1804, 0x0016, 0xadfa],
            "CJUMP 0x16ADFA (DB)",
        ),
        (
            VisaInstruction::StoreDmPostModify {
                src: r(2).into(),
                index: i(7),
                modifier: m(7),
            },
            vec![0x9ff2],
            "DM(I7,M7)=R2",
        ),
        (
            VisaInstruction::PushImmediate { value: 0xf8_38ca },
            vec![0x9fc0, 0x00f8, 0x38ca],
            "DM(I7,M7)=0xF838CA",
        ),
    ]
}

#[test]
fn documented_parcels_and_section_two_serialization() {
    for (instruction, expected, _) in vectors() {
        assert_eq!(instruction.parcels().unwrap(), expected, "{instruction:?}");
        let bytes: Vec<_> = expected.into_iter().flat_map(u16::to_le_bytes).collect();
        assert_eq!(instruction.encode().unwrap(), bytes, "{instruction:?}");
    }
    assert_eq!(
        Instruction::LoadImmediate {
            dst: r(1),
            value: 0x3d4c_cccd
        }
        .encode_visa()
        .unwrap(),
        [0x01, 0x0f, 0x4c, 0x3d, 0xcd, 0xcc]
    );
    assert_eq!(
        VisaInstruction::LeafReturn.parcels().unwrap(),
        LEAF_RETURN_PARCELS
    );
    assert_eq!(
        VisaInstruction::LeafReturn.encode().unwrap(),
        [
            0xfe, 0x4d, 0x3f, 0x0e, 0x3f, 0x08, 0x3f, 0x34, 0x01, 0x19, 0x01, 0x00,
        ]
    );
}

#[test]
fn mixed_width_labels_use_branch_start_and_parcel_units() {
    let mut program = VisaProgram::new();
    program.label("loop"); // parcel 0
    program.push(VisaInstruction::Transfer {
        dst: i(4).into(),
        src: r(4).into(),
    }); // 2
    program.jump(Condition::Eq, "done"); // parcel 2; target 11: +9
    program.push(Instruction::FloatNeg {
        dst: r(0),
        src: r(0),
    }); // 3
    program.jump(Condition::Ne, "loop"); // parcel 8; target 0: -8
    // "done" is still undefined: the count does not resolve labels.
    assert_eq!(program.parcel_count().unwrap(), 11);
    program.label("done"); // parcel 11
    program.push(VisaInstruction::LeafReturn); // 6 parcels (not 4 instructions)
    let expected = [
        0x707e, 0x0a3f, 0x0700, 0x0000, 0x0009, 0x013e, 0x000a, 0x2000, 0x0720, 0x00ff, 0xfff8,
        0x4dfe, 0x0e3f, 0x083f, 0x343f, 0x1901, 0x0001,
    ];
    assert_eq!(program.parcels().unwrap(), expected);
    assert_eq!(program.instruction_count(), 8);
    assert_eq!(
        program.encode().unwrap(),
        expected
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );
    assert!(VisaProgram::new().encode().unwrap().is_empty());
}

#[test]
fn register_offset_branch_and_label_validation() {
    assert_eq!(
        UniversalRegister::new(0x60),
        Err(VisaError::InvalidUniversalRegister(0x60))
    );
    assert!(IndexRegister::new(16).is_err());
    assert!(ModifyRegister::new(16).is_err());
    assert_eq!(UniversalRegister::from(i(15)).code(), 0x1f);
    assert_eq!(UniversalRegister::from(m(15)).code(), 0x2f);
    assert_eq!(UniversalRegister::from(r(15)).code(), 0x0f);
    for index in 0..8 {
        for code in [0, 15, 16, 31, 32, 47, 48, 63, 64, 79, 80, 95] {
            for offset in [-65, -64, -1, 0, 63, 64, 0x1fff, i32::MIN, i32::MAX] {
                let instruction = VisaInstruction::LoadDmOffset {
                    dst: u(code),
                    index: i(index),
                    offset,
                };
                let p = instruction.parcels().unwrap();
                if (-64..=63).contains(&offset) {
                    assert_eq!(p.len(), 2);
                    assert_eq!((p[1] >> 7) & 0x7f, u16::from(code));
                    assert_eq!(p[1] & 0x7f, (offset as u16) & 0x7f);
                } else {
                    assert_eq!(p.len(), 3);
                    assert_eq!(p[0] & 0x7f, u16::from(code));
                    assert_eq!((u32::from(p[1]) << 16) | u32::from(p[2]), offset as u32);
                }
            }
        }
    }
    assert_eq!(
        VisaInstruction::LoadDmOffset {
            dst: u(0),
            index: i(8),
            offset: 0
        }
        .encode(),
        Err(VisaError::InvalidDmIndex(8))
    );
    assert_eq!(
        VisaInstruction::LoadDmPostModify {
            dst: u(0),
            index: i(4),
            modifier: m(8)
        }
        .encode(),
        Err(VisaError::InvalidDmModifier(8))
    );
    assert_eq!(
        VisaInstruction::LoadDmPostModify {
            dst: i(12).into(),
            index: i(5),
            modifier: m(5)
        }
        .encode(),
        Err(VisaError::UnsupportedPostModifyRegister(0x1c))
    );
    for offset in [-0x80_0000, 0x7f_ffff] {
        assert!(
            VisaInstruction::JumpRelative {
                condition: Condition::Always,
                offset,
                delayed: false
            }
            .encode()
            .is_ok()
        );
    }
    for offset in [-0x80_0001, 0x80_0000] {
        assert_eq!(
            VisaInstruction::JumpRelative {
                condition: Condition::Always,
                offset,
                delayed: false
            }
            .encode(),
            Err(VisaError::BranchOutOfRange(i64::from(offset)))
        );
    }
    assert!(
        VisaInstruction::CJump { target: 0xff_ffff }
            .encode()
            .is_ok()
    );
    assert_eq!(
        VisaInstruction::CJump { target: 0x100_0000 }.encode(),
        Err(VisaError::CallTargetOutOfRange(0x100_0000))
    );
    let mut missing = VisaProgram::new();
    missing.jump(Condition::Always, "missing");
    assert_eq!(
        missing.encode(),
        Err(VisaError::UndefinedLabel("missing".into()))
    );
    let mut duplicate = VisaProgram::new();
    duplicate.label("duplicate");
    duplicate.label("duplicate");
    assert_eq!(
        duplicate.encode(),
        Err(VisaError::DuplicateLabel("duplicate".into()))
    );
}

#[test]
#[ignore = "set SELACHE to a directory containing standalone selas and seldump"]
fn standalone_selache_decodes_our_visa_stream() {
    use std::{
        fs,
        path::PathBuf,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    let tools = PathBuf::from(std::env::var_os("SELACHE").expect("set SELACHE"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("qc-asm-visa-oracle-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let source = directory.join("visa.asm");
    let object = directory.join("visa.doj");
    let mut expected = Vec::new();
    let mut program = VisaProgram::new();
    for (instruction, parcels, mnemonic) in vectors() {
        expected.push((parcels, mnemonic));
        program.push(instruction);
    }
    program.push(VisaInstruction::LeafReturn);
    expected.extend([
        (vec![0x4dfe, 0x0e3f], "I12=DM(M7,I6)"),
        (vec![0x083f, 0x343f], "JUMP(M14,I12)(DB)"),
        (vec![0x1901], "RFRAME"),
        (vec![0x0001], "NOP"),
    ]);
    let bytes = program.encode().unwrap();
    fs::write(
        &source,
        format!(
            ".section/pm qc_asm;\n{}",
            "nop;\n".repeat(bytes.len().div_ceil(6))
        ),
    )
    .unwrap();
    let output = Command::new(tools.join("selas"))
        .args(["-proc", "ADSP-SC589", "-o"])
        .arg(&object)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "selas: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut elf = fs::read(&object).unwrap();
    assert_eq!(&elf[..6], b"\x7fELF\x01\x01");
    let read_u32 = |at: usize| u32::from_le_bytes(elf[at..at + 4].try_into().unwrap()) as usize;
    let read_u16 = |at: usize| u16::from_le_bytes(elf[at..at + 2].try_into().unwrap()) as usize;
    let table = read_u32(32);
    let size = read_u16(46);
    let count = read_u16(48);
    let names_header = table + read_u16(50) * size;
    let names_at = read_u32(names_header + 16);
    let names_size = read_u32(names_header + 20);
    let names = &elf[names_at..names_at + names_size];
    let header = (0..count)
        .map(|n| table + n * size)
        .find(|&header| {
            let at = read_u32(header);
            names[at..].split(|&b| b == 0).next().unwrap() == b"qc_asm"
        })
        .unwrap();
    let code_at = read_u32(header + 16);
    let allocated = read_u32(header + 20);
    assert!(allocated >= bytes.len());
    // ELF oracle consumes big-endian parcels. Convert our ACTUAL section-2
    // bytes, not an independent encoder or the expected numeric vectors.
    for (to, from) in elf[code_at..code_at + bytes.len()]
        .chunks_exact_mut(2)
        .zip(bytes.chunks_exact(2))
    {
        to.copy_from_slice(&[from[1], from[0]]);
    }
    elf[header + 20..header + 24].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    // Seldump's ELF transport identifies VISA sections by sh_entsize=1.
    // This metadata is an oracle convention, not a QC section layout.
    elf[header + 36..header + 40].copy_from_slice(&1_u32.to_le_bytes());
    fs::write(&object, elf).unwrap();
    let output = Command::new(tools.join("seldump"))
        .args(["-ni", "qc_asm"])
        .arg(&object)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "seldump: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let disassembly = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = disassembly
        .lines()
        .filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|text| text.len() == 8 && text.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .collect();
    assert_eq!(lines.len(), expected.len(), "{disassembly}");
    let normalize = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let mut address = 0;
    for (line, (parcels, mnemonic)) in lines.iter().zip(expected) {
        let fields: Vec<_> = line.split_whitespace().collect();
        assert_eq!(fields[0], format!("{address:08x}"), "{line}");
        assert_eq!(
            fields[1],
            parcels
                .iter()
                .map(|p| format!("{p:04x}"))
                .collect::<String>(),
            "{line}"
        );
        assert_eq!(
            normalize(&fields[2..].join(" ")),
            normalize(mnemonic),
            "{line}"
        );
        address += parcels.len();
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dag1_stores_reject_dag1_register_sources() {
    // ADI UREGXDAG1: a DAG1 store cannot take I0-I7, M0-M7, L0-L7 or B0-B7.
    for source in [0x10, 0x17, 0x20, 0x22, 0x30, 0x40] {
        for instruction in [
            VisaInstruction::StoreDmOffset {
                src: u(source),
                index: i(6),
                offset: -12,
            },
            VisaInstruction::StoreDmPreModify {
                src: u(source),
                index: i(2),
                modifier: m(2),
            },
        ] {
            assert!(
                matches!(instruction.parcels(), Err(VisaError::Dag1StoreSource(code)) if code == source),
                "{source:#x}"
            );
        }
    }
    // DAG2 registers and data registers are legal sources; DAG1 loads stay legal.
    for source in [0x02, 0x0f, 0x18, 0x28] {
        assert!(
            VisaInstruction::StoreDmOffset {
                src: u(source),
                index: i(6),
                offset: -2
            }
            .parcels()
            .is_ok()
        );
    }
    assert!(
        VisaInstruction::LoadDmOffset {
            dst: i(0).into(),
            index: i(6),
            offset: -12
        }
        .parcels()
        .is_ok()
    );
}
