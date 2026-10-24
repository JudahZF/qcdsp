//! Opt-in end-to-end check of generated device code without a device.
//!
//! The generated blob is disassembled by the independent research decoder
//! (`QC_BLOBDIS`, a separate GPL tool run as a subprocess), and that text is
//! executed here. Each example is called the way the kernel calls Volume's
//! callbacks and must match the host reference on both channels (a mono block
//! copies channel A to channel B), restore every callee-preserved register and
//! the stack, and write only its own state and the audio buffer. Needs
//! `QC_CORPUS_DIR` too. Arithmetic is host f32, not a model of SHARC rounding
//! modes, so outputs are compared with a tolerance.

use std::{collections::HashMap, fs, path::PathBuf, process::Command};

use qc_format::diff::DiffFile;
use qcdsp::{
    Block, BlockBuilder, Category, Channels, Parameter, Processor, codegen,
    device::{self, VolumeTemplate},
    examples,
};

const STATE: u32 = 0x10_0000;
const BUFFER: u32 = 0x20_0000;
const IO: u32 = 0x30_0000;
const STACK: u32 = 0x40_0000;
const RETURN: u32 = 0xdead;
const GARBAGE: u32 = 0x7fc0_dead; // a NaN: any uninitialised read poisons output

struct Line {
    raw: u64,
    text: String,
    next: u32,
}

#[derive(Default, Clone)]
struct Cpu {
    r: [u32; 16],
    i: [u32; 16],
    m: [u32; 16],
    az: bool,
}

struct Machine {
    code: HashMap<u32, Line>,
    memory: HashMap<u32, u32>,
    cpu: Cpu,
    writes: Vec<u32>,
}

fn number(text: &str) -> i64 {
    let (negative, digits) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let value = i64::from_str_radix(digits.trim_start_matches("0x"), 16).unwrap();
    if negative { -value } else { value }
}

/// `r3`, `f3`, `i3`, `m3` → (class, index).
fn register(text: &str) -> (char, usize) {
    let text = text.trim();
    (text.chars().next().unwrap(), text[1..].parse().unwrap())
}

impl Machine {
    fn get(&self, (class, index): (char, usize)) -> u32 {
        match class {
            'r' | 'f' => self.cpu.r[index],
            'i' => self.cpu.i[index],
            'm' => self.cpu.m[index],
            _ => panic!("register class {class}"),
        }
    }

    fn set(&mut self, (class, index): (char, usize), value: u32) {
        match class {
            'r' | 'f' => self.cpu.r[index] = value,
            'i' => self.cpu.i[index] = value,
            'm' => self.cpu.m[index] = value,
            _ => panic!("register class {class}"),
        }
    }

    fn load(&self, address: u32) -> u32 {
        *self.memory.get(&address).unwrap_or(&GARBAGE)
    }

    fn store(&mut self, address: u32, value: u32) {
        self.writes.push(address);
        self.memory.insert(address, value);
    }

    fn float(&self, text: &str) -> f32 {
        f32::from_bits(self.get(register(text)))
    }

    /// Run from `pc` until the C-runtime return, or a jump outside generated code.
    /// Returns the out-of-code target, if any.
    fn run(&mut self, mut pc: u32) -> Option<u32> {
        for _ in 0..1_000_000 {
            let line = &self.code[&pc];
            let (text, raw, mut next) = (line.text.clone(), line.raw, line.next);
            let text = text.split(';').next().unwrap().trim();
            if text == "nop" || text == "rframe" {
                if text == "rframe" {
                    self.cpu.i[7] = self.cpu.i[6];
                    self.cpu.i[6] = self.load(self.cpu.i[6]);
                }
                if self.cpu.i[12] == RETURN && text == "nop" {
                    return None;
                }
            } else if text.starts_with("compu(") || text.starts_with("compu (") {
                let operands = text
                    .trim_start_matches("compu")
                    .trim()
                    .trim_start_matches('(')
                    .trim_end_matches(')');
                let (a, b) = operands.split_once(',').unwrap();
                self.cpu.az = self.get(register(a.trim())) <= self.get(register(b.trim()));
            } else if let Some(rest) = text.strip_prefix("if gt jump (pc,") {
                if !self.cpu.az {
                    next = pc.wrapping_add(number(rest.trim_end_matches(')')) as u32);
                }
            } else if let Some(rest) = text.strip_prefix("if ne jump (pc,") {
                if !self.cpu.az {
                    next = pc.wrapping_add(number(rest.trim_end_matches(')')) as u32);
                }
            } else if let Some(rest) = text.strip_prefix("jump (pc,") {
                next = pc.wrapping_add(number(rest.trim_end_matches(')')) as u32);
                if !self.code.contains_key(&next) {
                    return Some(next);
                }
            } else if text == "jump (m14,i12) (db)" {
                assert_eq!(
                    self.cpu.i[12], RETURN,
                    "pushed return address (jump target is +M14)"
                );
            } else if let Some((lhs, rhs)) = text.split_once('=') {
                self.assign(lhs.trim(), rhs.trim(), raw);
            } else {
                panic!("unhandled instruction {text:?} at {pc:#x}");
            }
            pc = next;
        }
        panic!("did not return");
    }

    fn address(&self, operand: &str, raw: u64) -> (u32, Option<(usize, u32)>) {
        // `dm (A,B)`: (offset|m, i) is pre-modify; (i, m) is post-modify.
        let inner = operand
            .trim_start_matches("dm")
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')');
        let (a, b) = inner.split_once(',').unwrap();
        let (a, b) = (a.trim(), b.trim());
        if a.starts_with('i') {
            let (_, index) = register(a);
            let (_, modifier) = register(b);
            let base = self.cpu.i[index];
            (base, Some((index, base.wrapping_add(self.cpu.m[modifier]))))
        } else {
            let (_, index) = register(b);
            let offset = if a.starts_with('m') {
                // The decoder misreads Type 3a M; ADI puts M[2:0] at bits 40:38
                // of the 48-bit word, i.e. bits 24:22 of this 32-bit form.
                let modifier = ((raw >> 22) & 7) as usize;
                self.cpu.m[modifier]
            } else {
                number(a) as u32
            };
            (self.cpu.i[index].wrapping_add(offset), None)
        }
    }

    fn assign(&mut self, lhs: &str, rhs: &str, raw: u64) {
        if lhs.starts_with("dm") {
            // ADI UREGXDAG1: DAG1 stores cannot source I0-I7 or M0-M7. The
            // decoder prints these happily, so legality is checked here.
            let source = register(rhs);
            assert!(
                !(matches!(source.0, 'i' | 'm') && source.1 < 8),
                "illegal DAG1 store source {rhs}"
            );
            let (address, update) = self.address(lhs, raw);
            let value = self.get(source);
            self.store(address, value);
            if let Some((index, value)) = update {
                self.cpu.i[index] = value;
            }
            return;
        }
        let dst = register(lhs);
        let flags = |value: u32| value == 0 || value == 0x8000_0000;
        let value = if rhs.starts_with("dm") {
            let (address, update) = self.address(rhs, raw);
            let value = self.load(address);
            if let Some((index, value)) = update {
                self.cpu.i[index] = value;
            }
            value
        } else if let Some(rest) = rhs.strip_prefix("modify (") {
            let (_, offset) = rest.trim_end_matches(")(nw)").split_once(',').unwrap();
            self.cpu.i[dst.1].wrapping_add(number(offset) as u32)
        } else if rhs.starts_with("0x") || rhs.starts_with("-0x") {
            number(rhs) as u32
        } else if let Some(source) = rhs.strip_prefix("pass ") {
            let value = self.get(register(source));
            self.cpu.az = value == 0;
            value
        } else if let Some(source) = rhs.strip_suffix("-1").filter(|s| !s.contains(['+', '*'])) {
            let value = self.get(register(source)).wrapping_sub(1);
            self.cpu.az = value == 0;
            value
        } else if let Some(args) = rhs
            .strip_prefix("min(")
            .or_else(|| rhs.strip_prefix("max("))
        {
            let (a, b) = args.trim_end_matches(')').split_once(',').unwrap();
            let (a, b) = (self.float(a), self.float(b));
            let value = if rhs.starts_with("min") {
                a.min(b)
            } else {
                a.max(b)
            };
            self.cpu.az = flags(value.to_bits());
            value.to_bits()
        } else if let Some(source) = rhs.strip_prefix("abs ") {
            let value = self.float(source).abs();
            self.cpu.az = flags(value.to_bits());
            value.to_bits()
        } else if let Some(source) = rhs.strip_prefix('-') {
            (-self.float(source)).to_bits()
        } else if let Some((a, op, b)) = ['*', '+', '-']
            .iter()
            .find_map(|&op| rhs.split_once(op).map(|(a, b)| (a, op, b)))
        {
            let (a, b) = (self.float(a), self.float(b));
            let value = match op {
                '*' => a * b,
                '+' => a + b,
                _ => a - b,
            };
            self.cpu.az = flags(value.to_bits());
            value.to_bits()
        } else {
            self.get(register(rhs))
        };
        self.set(dst, value);
    }

    /// CJUMP: FP = SP, push the old FP and the return address.
    fn call(&mut self, entry: u32, args: [u32; 3]) -> Option<u32> {
        [self.cpu.r[4], self.cpu.r[8], self.cpu.r[12]] = args;
        let old_fp = self.cpu.i[6];
        self.cpu.i[6] = self.cpu.i[7];
        self.store(self.cpu.i[7], old_fp);
        self.store(self.cpu.i[7] - 1, RETURN);
        self.cpu.i[7] -= 2;
        self.run(entry)
    }
}

fn disassemble(bytes: &[u8], name: &str) -> HashMap<u32, Line> {
    let path =
        std::env::temp_dir().join(format!("qcdsp-emulate-{}-{name}.raw", std::process::id()));
    fs::write(&path, bytes).unwrap();
    let output = Command::new(std::env::var_os("QC_BLOBDIS").unwrap())
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(&path).unwrap();
    assert!(output.status.success());
    let mut lines: Vec<(u32, u64, String)> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let address = u32::from_str_radix(line.get(..6)?, 16).ok()?;
            let raw = line.get(8..20)?.trim();
            let text = line.get(22..)?.split("<==").next()?.trim().to_owned();
            Some((address, u64::from_str_radix(raw, 16).ok()?, text))
        })
        .filter(|(address, _, _)| *address >= device::template::STOCK_PARCELS)
        .collect();
    lines.sort_by_key(|line| line.0);
    let mut code = HashMap::new();
    for window in 0..lines.len() {
        let (address, raw, text) = lines[window].clone();
        let next = lines.get(window + 1).map_or(u32::MAX, |line| line.0);
        assert!(
            !text.contains("???"),
            "undecodable generated code at {address:#x}"
        );
        code.insert(address, Line { raw, text, next });
    }
    code
}

fn template() -> Option<VolumeTemplate> {
    let corpus = PathBuf::from(std::env::var_os("QC_CORPUS_DIR")?).join("plain");
    std::env::var_os("QC_BLOBDIS")?;
    let core1 = fs::read(corpus.join("blobs/core1/volume.raw")).unwrap();
    let diffs = [2, 3, 4].map(|core| {
        let path = format!("blobs/core{core}/diffs.blob");
        let path = if corpus.join(&path).is_file() {
            corpus.join(&path)
        } else {
            corpus
                .parent()
                .unwrap()
                .join("rootfs/opt/neuraldsp")
                .join(&path)
        };
        DiffFile::parse(&fs::read(path).unwrap())
            .unwrap()
            .get("volume")
            .unwrap()
            .clone()
    });
    Some(VolumeTemplate::new(&core1, diffs).unwrap())
}

fn check(block: &Block, template: &VolumeTemplate, parameters: &[f32]) {
    let built = device::build(block, template, 4_000_000_000, block.name()).unwrap();
    let code = disassemble(&built.core1.to_bytes().unwrap(), block.blob_name());
    let mut machine = Machine {
        code,
        memory: HashMap::new(),
        cpu: Cpu::default(),
        writes: Vec::new(),
    };
    machine.cpu.m[5] = 0;
    machine.cpu.m[6] = 1;
    machine.cpu.m[7] = u32::MAX;
    machine.cpu.m[14] = 1;
    machine.cpu.i[7] = STACK;
    machine.cpu.i[6] = STACK + 0x100;
    for index in [3, 5, 6, 7, 9, 10, 11, 13, 14, 15] {
        machine.cpu.r[index] = 0x1111_0000 + index as u32;
    }
    for index in 0..4 {
        machine.cpu.i[index] = 0x2222_0000 + index as u32;
    }
    machine.cpu.i[5] = 0x2222_0005;
    machine.cpu.m[2] = 0x3333_0002;
    let preserved = machine.cpu.clone();

    // init must tail-jump to stock init (0xe2) with R4 still the state pointer.
    let target = machine.call(built.entries.init, [STATE, 0, 0]);
    assert_eq!(target, Some(device::template::STOCK_INIT), "init tail jump");
    assert_eq!(machine.cpu.r[4], STATE);
    machine.cpu.i[6] = preserved.i[6];
    machine.cpu.i[7] = preserved.i[7];

    let mut hosts = [
        Processor::new(block.clone()).unwrap(),
        Processor::new(block.clone()).unwrap(),
    ];
    // Delivered values are 0..1; invalid bit patterns leave the value unchanged.
    for (index, &normalized) in parameters.iter().enumerate() {
        assert_eq!(
            machine.call(
                built.entries.set_parameter,
                [STATE, index as u32, normalized.to_bits()]
            ),
            None
        );
        if normalized.to_bits() > 1.0_f32.to_bits() {
            continue;
        }
        let parameter = &block.parameters()[index];
        let value = parameter.min + normalized * (parameter.max - parameter.min);
        for host in &mut hosts {
            host.set_parameter(index, value).unwrap();
        }
    }
    // Out-of-range indices must not write anything.
    let before = machine.writes.len();
    for index in [parameters.len() as u32, 99, u32::MAX] {
        machine.call(
            built.entries.set_parameter,
            [STATE, index, 0.5f32.to_bits()],
        );
    }
    assert!(
        machine.writes[before..]
            .iter()
            .all(|address| (STACK - 0x100..STACK + 0x200).contains(address))
    );

    machine.store(IO, BUFFER);
    machine.store(IO + 1, BUFFER + 32);
    let state_end = STATE + built.layout.total_words;
    let mut seed = 0x1234_5678u32;
    for _ in 0..40 {
        let mut expected = Vec::new();
        for channel in 0..2 {
            for sample in 0..32 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let input = (seed >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0;
                machine.store(BUFFER + channel * 32 + sample, input.to_bits());
                let output = if channel == 1 && block.channels() == Channels::Mono {
                    expected[sample as usize]
                } else {
                    hosts[channel as usize].process_sample(input)
                };
                expected.push(output);
            }
        }
        machine.writes.clear();
        assert_eq!(machine.call(built.entries.process, [STATE, IO, 0]), None);
        for (index, &expected) in expected.iter().enumerate() {
            let actual = f32::from_bits(machine.load(BUFFER + index as u32));
            assert!(
                (actual - expected).abs() <= 1e-5 * expected.abs().max(1.0),
                "{}: sample {index}: device {actual} vs host {expected}",
                block.name()
            );
        }
        for &address in &machine.writes {
            let own_state = (STATE + 17..state_end).contains(&address);
            let audio = (BUFFER..BUFFER + 64).contains(&address);
            let frame = (STACK - 0x40..=STACK).contains(&address);
            assert!(
                own_state || audio || frame,
                "{}: stray write at {address:#x}",
                block.name()
            );
        }
        for index in [3, 5, 6, 7, 9, 10, 11, 13, 14, 15] {
            assert_eq!(
                machine.cpu.r[index], preserved.r[index],
                "R{index} not preserved"
            );
        }
        for index in [0, 1, 2, 3, 5, 6, 7] {
            assert_eq!(
                machine.cpu.i[index], preserved.i[index],
                "I{index} not preserved"
            );
        }
        assert_eq!(machine.cpu.m[2], preserved.m[2], "M2 not preserved");
    }
}

/// More computed values live at once than the device has registers, plus a
/// hoisted control value and a delay line, so the code must spill and reload.
fn pressure() -> Block {
    let mut dsp = BlockBuilder::new("pressure", "Pressure", Category::Utility);
    let input = dsp.input();
    let level = dsp
        .parameter(Parameter::new("Level", 0.0, 1.0, 0.5).unwrap())
        .unwrap();
    let taper = dsp.mul(level, level).unwrap();
    let half = dsp.constant(0.5).unwrap();
    let wet = dsp.mul(input, half).unwrap();
    let line = dsp.delay(3).unwrap();
    let mut terms = Vec::new();
    for index in 0..20 {
        let state = dsp.state(index as f32 * 0.01).unwrap();
        let term = dsp.mul(state.signal(), taper).unwrap();
        let damped = dsp.mul(term, half).unwrap();
        let next = dsp.add(damped, wet).unwrap();
        dsp.update_state(state, next).unwrap();
        terms.push(term);
    }
    let mut sum = line.signal();
    for term in terms.into_iter().rev() {
        sum = dsp.add(sum, term).unwrap();
    }
    let output = dsp.mul(sum, half).unwrap();
    dsp.update_delay(line, output).unwrap();
    dsp.finish(output).unwrap()
}

#[test]
fn generated_callbacks_match_the_host_reference() {
    let Some(template) = template() else {
        eprintln!("skipped: set QC_CORPUS_DIR and QC_BLOBDIS");
        return;
    };
    check(&examples::gain().unwrap(), &template, &[0.6]);
    check(&examples::soft_clip().unwrap(), &template, &[0.6, 1.7]);
    check(&examples::delay(37).unwrap(), &template, &[0.6, -0.5]);
    check(
        &examples::biquad([0.06745527, 0.13491054, 0.06745527, -1.1429805, 0.4128016]).unwrap(),
        &template,
        &[],
    );
    check(&examples::tremolo().unwrap(), &template, &[0.9, 0.7]);
    check(
        &examples::envelope_filter().unwrap(),
        &template,
        &[0.8, 0.3, 0.6, 0.9, 0.5],
    );
    for voicing in [examples::Voicing::Guitar, examples::Voicing::Bass] {
        check(
            &examples::sweep_synth(voicing).unwrap(),
            &template,
            &[0.1, 0.7, 0.3, 0.4, 0.6, 0.2, 0.8, 0.9, 0.1, 0.7],
        );
    }
    let pressure = pressure();
    // R0-R13: the sample loop keeps R14 (delay cursor) and R15 (counter).
    let contract = codegen::compile_with(&pressure, 0x3fff).unwrap().contract;
    assert!(contract.spill_slots > 0 && contract.control_slots > 0);
    check(&pressure, &template, &[0.8]);
}
