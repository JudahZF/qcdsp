use qc_asm::{ByteOrder, CodeMode, Error, Instruction, Program, Register};

fn r(index: u8) -> Register {
    Register::new(index).unwrap()
}

// Numeric words were derived from ADI's ISA diagrams and cross-checked against
// standalone selache. Move uses the real Type5a transfer (not the PASS shortcut).
fn vectors() -> Vec<(Instruction, u64, &'static str)> {
    vec![
        (
            Instruction::FloatAdd {
                dst: r(0),
                lhs: r(1),
                rhs: r(2),
            },
            0x013e00081012,
            "F0 = F1 + F2",
        ),
        (
            Instruction::FloatSub {
                dst: r(3),
                lhs: r(4),
                rhs: r(5),
            },
            0x013e00082345,
            "F3 = F4 - F5",
        ),
        (
            Instruction::FloatMul {
                dst: r(6),
                lhs: r(7),
                rhs: r(8),
            },
            0x013e00130678,
            "F6 = F7 * F8",
        ),
        (
            Instruction::FloatMin {
                dst: r(9),
                lhs: r(10),
                rhs: r(11),
            },
            0x013e000e19ab,
            "F9 = MIN(F10, F11)",
        ),
        (
            Instruction::FloatMax {
                dst: r(12),
                lhs: r(13),
                rhs: r(14),
            },
            0x013e000e2cde,
            "F12 = MAX(F13, F14)",
        ),
        (
            Instruction::FloatNeg {
                dst: r(15),
                src: r(1),
            },
            0x013e000a2f10,
            "F15 = -F1",
        ),
        (
            Instruction::FloatAbs {
                dst: r(2),
                src: r(15),
            },
            0x013e000b02f0,
            "F2 = ABS F15",
        ),
        (
            Instruction::FloatPass {
                dst: r(1),
                src: r(8),
            },
            0x013e000a1180,
            "F1 = PASS F8",
        ),
        (
            Instruction::LoadImmediate {
                dst: r(15),
                value: 1.0_f32.to_bits(),
            },
            0x0f0f3f800000,
            "R15 = 0x3F800000",
        ),
        (
            Instruction::Move {
                dst: r(4),
                src: r(15),
            },
            0x70ff82000000,
            "R4 = R15",
        ),
        (
            Instruction::LoadDm {
                dst: r(3),
                address: 0x12345678,
            },
            0x100312345678,
            "R3 = DM (0x12345678)",
        ),
        (
            Instruction::StoreDm {
                src: r(12),
                address: 0x87654321,
            },
            0x110c87654321,
            "DM (0x87654321) = R12",
        ),
    ]
}

#[test]
fn documented_numeric_words_and_both_container_orders() {
    for (instruction, expected, _) in vectors() {
        assert_eq!(instruction.word().unwrap(), expected, "{instruction:?}");
        let le: [u8; 6] = expected.to_le_bytes()[..6].try_into().unwrap();
        let be: [u8; 6] = expected.to_be_bytes()[2..].try_into().unwrap();
        assert_eq!(instruction.encode().unwrap(), le);
        assert_eq!(
            instruction.encode_with_order(ByteOrder::BigEndian).unwrap(),
            be
        );
    }
    assert_eq!(
        vectors()[0].0.encode().unwrap(),
        [0x12, 0x10, 0x08, 0, 0x3e, 1]
    );
}

#[test]
fn register_fields_and_immediate_boundaries_do_not_overlap() {
    assert_eq!(Register::new(16), Err(Error::InvalidRegister(16)));
    assert_eq!(Register::new(u8::MAX), Err(Error::InvalidRegister(u8::MAX)));
    for index in 0..16 {
        assert_eq!(r(index).index(), index);
    }
    let moves = [
        (0, 15, 0x703e07800000),
        (1, 0, 0x703e80000000),
        (2, 0, 0x703f00000000),
        (4, 0, 0x707e00000000),
        (8, 0, 0x70be00000000),
        (15, 15, 0x70ff87800000),
    ];
    for (src, dst, expected) in moves {
        assert_eq!(
            Instruction::Move {
                dst: r(dst),
                src: r(src)
            }
            .word()
            .unwrap(),
            expected
        );
    }
    assert_eq!(
        Instruction::LoadImmediate {
            dst: r(0),
            value: 0
        }
        .word()
        .unwrap(),
        0x0f0000000000
    );
    assert_eq!(
        Instruction::LoadImmediate {
            dst: r(15),
            value: u32::MAX
        }
        .word()
        .unwrap(),
        0x0f0fffffffff
    );
    assert_eq!(
        Instruction::FloatAdd {
            dst: r(15),
            lhs: r(15),
            rhs: r(15)
        }
        .word()
        .unwrap(),
        0x013e00081fff
    );
}

#[test]
fn program_preserves_instruction_order_in_isa_and_visa() {
    let mut program = Program::new();
    assert!(program.encode().unwrap().is_empty());
    let expected_instructions: Vec<_> = vectors().into_iter().map(|(i, _, _)| i).collect();
    for &instruction in &expected_instructions {
        program.push(instruction);
    }
    assert_eq!(program.instructions(), expected_instructions);
    let bytes = program.encode().unwrap();
    assert_eq!(bytes.len(), expected_instructions.len() * 6);
    for (chunk, instruction) in bytes.chunks_exact(6).zip(expected_instructions) {
        assert_eq!(chunk, instruction.encode().unwrap());
    }
    let visa = program
        .encode_for(CodeMode::Visa, ByteOrder::LittleEndian)
        .unwrap();
    for (chunk, (instruction, word, _)) in visa.chunks_exact(6).zip(vectors()) {
        let high_to_low = word.to_be_bytes();
        let expected = [
            high_to_low[3],
            high_to_low[2],
            high_to_low[5],
            high_to_low[4],
            high_to_low[7],
            high_to_low[6],
        ];
        assert_eq!(chunk, expected);
        assert_eq!(instruction.encode_visa().unwrap(), expected);
        assert_eq!(
            instruction
                .encode_visa_with_order(ByteOrder::BigEndian)
                .unwrap(),
            high_to_low[2..]
        );
    }
}

/// Opt-in subprocess-only validation. SELACHE is the directory containing selas
/// and seldump. No selache library is linked into qc-asm or this test.
#[test]
#[ignore = "set SELACHE to a directory containing standalone selas and seldump"]
fn standalone_selache_decodes_our_words() {
    use std::{
        fs,
        path::PathBuf,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    let tools = PathBuf::from(
        std::env::var_os("SELACHE").expect("set SELACHE to the selache binary directory"),
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("qc-asm-oracle-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let source = directory.join("oracle.asm");
    let object = directory.join("oracle.doj");
    let vectors = vectors();
    fs::write(
        &source,
        format!(".section/pm qc_asm;\n{}", "nop;\n".repeat(vectors.len())),
    )
    .unwrap();

    let output = Command::new(tools.join("selas"))
        .args(["-proc", "ADSP-SC589", "-o"])
        .arg(&object)
        .arg(&source)
        .output()
        .expect("execute selas");
    assert!(
        output.status.success(),
        "selas failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Use the external assembler only to create an ELF shell. Replace its NOPs
    // with our encoded words; this tests decoder agreement rather than relying
    // on an encoder/decoder round trip within one toolchain.
    let mut elf = fs::read(&object).unwrap();
    assert_eq!(
        &elf[..6],
        b"\x7fELF\x01\x01",
        "expected little-endian ELF32 headers"
    );
    let read_u32 = |offset: usize| u32::from_le_bytes(elf[offset..offset + 4].try_into().unwrap());
    let read_u16 = |offset: usize| u16::from_le_bytes(elf[offset..offset + 2].try_into().unwrap());
    let section_table = read_u32(32) as usize;
    let entry_size = usize::from(read_u16(46));
    let count = usize::from(read_u16(48));
    let names_header = section_table + usize::from(read_u16(50)) * entry_size;
    let names_start = read_u32(names_header + 16) as usize;
    let names_size = read_u32(names_header + 20) as usize;
    let names = &elf[names_start..names_start + names_size];
    let (code_start, code_size) = (0..count)
        .find_map(|index| {
            let header = section_table + index * entry_size;
            let name_start = read_u32(header) as usize;
            let name = names[name_start..].split(|&byte| byte == 0).next().unwrap();
            (name == b"qc_asm").then(|| {
                (
                    read_u32(header + 16) as usize,
                    read_u32(header + 20) as usize,
                )
            })
        })
        .expect("assembler-created qc_asm section");
    assert_eq!(code_size, vectors.len() * 6);
    for (slot, (instruction, _, _)) in elf[code_start..code_start + code_size]
        .chunks_exact_mut(6)
        .zip(&vectors)
    {
        slot.copy_from_slice(&instruction.encode_with_order(ByteOrder::BigEndian).unwrap());
    }
    fs::write(&object, elf).unwrap();
    let output = Command::new(tools.join("seldump"))
        .args(["-ni", "qc_asm"])
        .arg(&object)
        .output()
        .expect("execute seldump");
    assert!(
        output.status.success(),
        "seldump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let disassembly = String::from_utf8(output.stdout).unwrap();
    let normalize = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let lines: Vec<_> = disassembly
        .lines()
        .filter(|line| {
            line.split_whitespace().next().is_some_and(|first| {
                first.len() == 8 && first.chars().all(|c| c.is_ascii_hexdigit())
            })
        })
        .collect();
    assert_eq!(
        lines.len(),
        vectors.len(),
        "unexpected disassembly: {disassembly}"
    );
    for (line, (_, word, mnemonic)) in lines.iter().zip(vectors) {
        let fields: Vec<_> = line.split_whitespace().collect();
        assert_eq!(fields[1], format!("{word:012x}"));
        assert_eq!(
            normalize(&fields[2..].join(" ")),
            normalize(mnemonic),
            "{line}"
        );
    }
    fs::remove_dir_all(&directory).unwrap();
}
