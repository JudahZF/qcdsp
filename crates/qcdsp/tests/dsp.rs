use qcdsp::{
    Block, BlockBuilder, Category, Channels, Error, MAX_NODES, Parameter, Processor,
    codegen::{self, InputSource, Step},
    examples,
};

fn assert_close(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() <= 2e-6 * expected.abs().max(1.0),
        "{actual} != {expected}"
    );
}

#[test]
fn gain_parameters_validate_without_changing_last_value() {
    let block = examples::gain().unwrap();
    let mut processor = Processor::new(block).unwrap();
    processor.set_parameter(0, 2.0).unwrap();
    assert_eq!(processor.process_sample(0.25), 0.5);
    assert!(processor.set_parameter(0, f32::NAN).is_err());
    assert!(processor.set_parameter(0, -0.1).is_err());
    assert_eq!(processor.process_sample(-0.5), -1.0);
    assert!(processor.set_parameter(1, 0.0).is_err());
}

#[test]
fn states_read_the_previous_sample_and_update_together() {
    let mut dsp = BlockBuilder::new("swap", "Swap", Category::Utility);
    let a = dsp.state(1.0).unwrap();
    let b = dsp.state(2.0).unwrap();
    dsp.update_state(a, b.signal()).unwrap();
    dsp.update_state(b, a.signal()).unwrap();
    // Rejected duplicates must not replace the previous update.
    let zero = dsp.constant(0.0).unwrap();
    assert!(dsp.update_state(a, zero).is_err());
    let block = dsp.finish(a.signal()).unwrap();
    for mut processor in [
        Processor::new(block.clone()).unwrap(),
        Processor::compiled(block).unwrap(),
    ] {
        assert_eq!(processor.process_sample(0.0), 1.0);
        assert_eq!(processor.process_sample(0.0), 2.0);
        assert_eq!(processor.process_sample(0.0), 1.0);
        processor.reset();
        assert_eq!(processor.process_sample(0.0), 1.0);
    }
}

#[test]
fn delay_impulse_feedback_and_reset_are_sample_exact() {
    let block = examples::delay(3).unwrap();
    for mut processor in [
        Processor::new(block.clone()).unwrap(),
        Processor::compiled(block).unwrap(),
    ] {
        processor.set_parameter(0, 1.0).unwrap();
        processor.set_parameter(1, 0.5).unwrap();
        let mut samples = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        processor.process_in_place(&mut samples);
        assert_eq!(samples, [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.5]);
        processor.reset();
        assert_eq!(processor.process_sample(0.0), 0.0);
        // Parameters are retained, so there is no dry component after reset.
        assert_eq!(processor.process_sample(1.0), 0.0);
    }
}

#[test]
fn biquad_matches_the_independent_difference_equation() {
    let coefficients = [0.06745527, 0.13491054, 0.06745527, -1.1429805, 0.4128016];
    let mut processor = Processor::new(examples::biquad(coefficients).unwrap()).unwrap();
    let [b0, b1, b2, a1, a2] = coefficients;
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    for i in 0..1024 {
        let x = if i == 0 { 1.0 } else { 0.0 };
        let expected = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        assert_close(processor.process_sample(x), expected);
        (x2, x1, y2, y1) = (x1, x, y1, expected);
    }
}

#[test]
fn lowering_matches_graph_execution_for_all_examples() {
    let blocks = [
        examples::gain().unwrap(),
        examples::soft_clip().unwrap(),
        examples::delay(17).unwrap(),
        examples::biquad([0.06745527, 0.13491054, 0.06745527, -1.1429805, 0.4128016]).unwrap(),
        examples::tremolo().unwrap(),
        examples::envelope_filter().unwrap(),
        examples::micro_synth(examples::Voicing::Guitar).unwrap(),
        examples::micro_synth(examples::Voicing::Bass).unwrap(),
        examples::saturator().unwrap(),
    ];
    for block in blocks {
        let decoded = Block::from_json(&block.to_json().unwrap()).unwrap();
        let kernel = codegen::compile(&decoded).unwrap();
        assert_eq!(
            kernel.encode().unwrap().len(),
            kernel.contract.instruction_count * 6
        );
        let mut reference = Processor::new(block.clone()).unwrap();
        let mut lowered = Processor::compiled(decoded).unwrap();
        let mut random = 7u32;
        for sample in 0..4096 {
            if sample == 1000 {
                for (index, parameter) in block.parameters().iter().enumerate() {
                    let value = (parameter.min + parameter.max) * 0.5;
                    reference.set_parameter(index, value).unwrap();
                    lowered.set_parameter(index, value).unwrap();
                }
            }
            if sample == 2000 {
                reference.reset();
                lowered.reset();
            }
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let input = (random as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32;
            assert_close(
                lowered.process_sample(input),
                reference.process_sample(input),
            );
        }
    }
}

#[test]
fn allocator_reuses_temporaries_and_handles_wide_blocks() {
    let mut dsp = BlockBuilder::new("chain", "Chain", Category::Utility);
    let one = dsp.constant(1.0).unwrap();
    let mut output = dsp.input();
    for _ in 0..100 {
        output = dsp.add(output, one).unwrap();
    }
    let block = dsp.finish(output).unwrap();
    assert!(codegen::compile(&block).unwrap().contract.registers_used <= 3);
    assert_eq!(
        Processor::compiled(block).unwrap().process_sample(1.0),
        101.0
    );

    // More inputs than registers: loaded at first use instead of all up front.
    let mut dsp = BlockBuilder::new("wide", "Wide", Category::Utility);
    let mut output = dsp.input();
    for index in 0..24 {
        let parameter = dsp
            .parameter(Parameter::new(format!("p{index}"), 0.0, 1.0, 0.5).unwrap())
            .unwrap();
        let state = dsp.state(index as f32).unwrap();
        output = dsp.add(output, parameter).unwrap();
        output = dsp.add(output, state.signal()).unwrap();
        dsp.update_state(state, output).unwrap();
    }
    let block = dsp.finish(output).unwrap();
    let mut reference = Processor::new(block.clone()).unwrap();
    let mut lowered = Processor::compiled(block).unwrap();
    for sample in 0..8 {
        let input = sample as f32 * 0.25;
        assert_eq!(
            lowered.process_sample(input),
            reference.process_sample(input)
        );
    }

    // An op whose operands both stay live needs three registers.
    let mut dsp = BlockBuilder::new("three", "Three", Category::Utility);
    let x = dsp.input();
    let y = dsp.state(1.0).unwrap();
    let product = dsp.mul(x, y.signal()).unwrap();
    let sum = dsp.add(product, x).unwrap();
    let output = dsp.add(sum, y.signal()).unwrap();
    let block = dsp.finish(output).unwrap();
    assert!(matches!(
        codegen::compile_with(&block, 0b11),
        Err(Error::RegisterPressure)
    ));
    assert!(codegen::compile_with(&block, 0b111).is_ok());
}

#[test]
fn parameter_math_is_hoisted_out_of_the_sample_loop() {
    let mut dsp = BlockBuilder::new("taper", "Taper", Category::Utility);
    let input = dsp.input();
    let level = dsp
        .parameter(Parameter::new("Level", 0.0, 1.0, 0.5).unwrap())
        .unwrap();
    let one = dsp.constant(1.0).unwrap();
    let squared = dsp.mul(level, level).unwrap();
    let gain = dsp.add(squared, one).unwrap();
    let output = dsp.mul(input, gain).unwrap();
    let block = dsp.finish(output).unwrap();
    let contract = codegen::compile(&block).unwrap().contract;
    assert_eq!(contract.control_slots, 1);
    let computes = |steps: &[Step]| {
        steps
            .iter()
            .filter(|step| matches!(step, Step::Compute { .. }))
            .count()
    };
    // Control: load 1.0, square, add. Sample: only the multiply.
    assert_eq!(computes(&contract.control), 3);
    assert_eq!(computes(&contract.sample), 1);
    let mut processor = Processor::compiled(block).unwrap();
    processor.set_parameter(0, 0.5).unwrap();
    assert_eq!(processor.process_sample(2.0), 2.5);
}

/// Deterministic LCG, so failures name a reproducible seed.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn below(&mut self, limit: usize) -> usize {
        self.next() as usize % limit
    }
    fn unit(&mut self) -> f32 {
        self.next() as f32 / (1u64 << 31) as f32 * 2.0 - 1.0
    }
}

/// A random graph with many inputs, long live ranges, state chains and swaps,
/// and delay taps. Products are clamped so values stay finite.
fn random_block(seed: u64) -> Block {
    let mut random = Random(seed);
    let mut dsp = BlockBuilder::new("fuzz", "Fuzz", Category::Utility);
    let mut signals = vec![dsp.input()];
    for index in 0..random.below(20) {
        let default = random.unit();
        let parameter = Parameter::new(format!("p{index}"), -1.0, 1.0, default).unwrap();
        signals.push(dsp.parameter(parameter).unwrap());
    }
    let states: Vec<_> = (0..random.below(16))
        .map(|_| dsp.state(random.unit()).unwrap())
        .collect();
    let delays: Vec<_> = (0..random.below(3))
        .map(|_| dsp.delay(1 + random.below(5)).unwrap())
        .collect();
    signals.extend(states.iter().map(|state| state.signal()));
    signals.extend(delays.iter().map(|delay| delay.signal()));
    let low = dsp.constant(-4.0).unwrap();
    let high = dsp.constant(4.0).unwrap();
    for _ in 0..3 {
        signals.push(dsp.constant(random.unit()).unwrap());
    }
    for _ in 0..20 + random.below(100) {
        // Mostly recent operands (chains), sometimes old ones (long live ranges).
        let pick = |random: &mut Random| {
            if random.below(3) == 0 {
                signals[random.below(signals.len())]
            } else {
                signals[signals.len() - 1 - random.below(signals.len().min(6))]
            }
        };
        let (a, b) = (pick(&mut random), pick(&mut random));
        let value = match random.below(7) {
            0 => dsp.add(a, b).unwrap(),
            1 => dsp.sub(a, b).unwrap(),
            2 => {
                let product = dsp.mul(a, b).unwrap();
                let bounded = dsp.max(product, low).unwrap();
                dsp.min(bounded, high).unwrap()
            }
            3 => dsp.min(a, b).unwrap(),
            4 => dsp.max(a, b).unwrap(),
            5 => dsp.neg(a).unwrap(),
            _ => dsp.abs(a).unwrap(),
        };
        signals.push(value);
    }
    for state in states {
        if random.below(5) != 0 {
            dsp.update_state(state, signals[random.below(signals.len())])
                .unwrap();
        }
    }
    for delay in delays {
        if random.below(5) != 0 {
            dsp.update_delay(delay, signals[random.below(signals.len())])
                .unwrap();
        }
    }
    let output = signals[signals.len() - 1 - random.below(signals.len().min(4))];
    dsp.finish(output).unwrap()
}

#[test]
fn random_graphs_lower_exactly_under_any_register_budget() {
    // All registers; six scattered ones; the three-register minimum.
    let budgets = [u16::MAX, 0b0110_0001_1000_0101, 0b111];
    for seed in 0..300 {
        let block = random_block(seed);
        for budget in budgets {
            let kernel = codegen::compile_with(&block, budget).unwrap();
            assert!(kernel.contract.registers_used <= budget.count_ones() as usize);
            let mut reference = Processor::new(block.clone()).unwrap();
            let mut lowered = Processor::lowered(block.clone(), kernel).unwrap();
            let mut random = Random(seed ^ 0x5eed);
            for sample in 0..64 {
                if sample == 20 {
                    for index in 0..block.parameters().len() {
                        let value = random.unit();
                        reference.set_parameter(index, value).unwrap();
                        lowered.set_parameter(index, value).unwrap();
                    }
                }
                if sample == 40 {
                    reference.reset();
                    lowered.reset();
                }
                let input = random.unit();
                let (expected, actual) = (
                    reference.process_sample(input),
                    lowered.process_sample(input),
                );
                // Same operations in the same order: results must be bit-identical.
                assert!(
                    actual.to_bits() == expected.to_bits()
                        || (actual.is_nan() && expected.is_nan()),
                    "seed {seed}, budget {budget:#06x}, sample {sample}: {actual} != {expected}"
                );
            }
        }
    }
}

#[test]
fn duplicate_source_nodes_from_imported_ir_share_one_value() {
    // The builder makes one node per state; hand-written IR can repeat them.
    // State 0 is written early, but three copies of its old value feed later
    // writes of states 1-3. As separate values they would need four registers.
    let block = Block::from_json(
        r#"{"version":1,"blob_name":"dup","name":"Dup","category":"utility",
        "parameters":[],"delays":[],
        "states":[{"initial":0.5,"update":5},{"initial":0.0,"update":2},
            {"initial":0.0,"update":3},{"initial":0.0,"update":4}],
        "nodes":[{"op":"input"},{"op":"state","index":0},{"op":"state","index":0},
            {"op":"state","index":0},{"op":"state","index":0},
            {"op":"binary","kind":"mul","lhs":0,"rhs":1},
            {"op":"state","index":1},{"op":"state","index":2},{"op":"state","index":3},
            {"op":"binary","kind":"add","lhs":6,"rhs":7},{"op":"binary","kind":"add","lhs":9,"rhs":8},
            {"op":"binary","kind":"add","lhs":10,"rhs":5}],
        "output":11}"#,
    )
    .unwrap();
    let kernel = codegen::compile_with(&block, 0b111).unwrap();
    let mut reference = Processor::new(block.clone()).unwrap();
    let mut lowered = Processor::lowered(block, kernel).unwrap();
    for sample in 0..8 {
        let input = 1.0 + sample as f32;
        assert_eq!(
            lowered.process_sample(input),
            reference.process_sample(input)
        );
    }
}

#[test]
fn dead_expressions_do_not_consume_registers() {
    let mut dsp = BlockBuilder::new("dead", "Dead", Category::Utility);
    for index in 0..100 {
        dsp.parameter(Parameter::new(format!("unused{index}"), 0.0, 1.0, 0.0).unwrap())
            .unwrap();
    }
    let input = dsp.input();
    let block = dsp.finish(input).unwrap();
    let kernel = codegen::compile(&block).unwrap();
    assert!(kernel.contract.control.is_empty());
    assert!(matches!(
        kernel.contract.sample[..],
        [
            Step::Load {
                source: InputSource::Audio,
                ..
            },
            Step::StoreOutput { .. }
        ]
    ));
    assert!(kernel.encode().unwrap().is_empty());
    assert_eq!(
        Processor::compiled(block).unwrap().process_sample(-0.125),
        -0.125
    );
}

#[test]
fn graph_handles_and_imported_ir_are_checked() {
    let a = BlockBuilder::new("a", "A", Category::Utility);
    let mut b = BlockBuilder::new("b", "B", Category::Utility);
    assert!(matches!(
        b.add(a.input(), b.input()),
        Err(Error::ForeignSignal)
    ));
    let mut json: serde_json::Value =
        serde_json::from_str(&examples::gain().unwrap().to_json().unwrap()).unwrap();
    json["nodes"][2]["lhs"] = 2.into();
    assert!(Block::from_json(&json.to_string()).is_err());
    json["version"] = 99.into();
    assert!(Block::from_json(&json.to_string()).is_err());
    assert!(Parameter::new("", 0.0, 1.0, 0.5).is_err());
    // Finite bounds whose span overflows would map device values to infinity.
    assert!(Parameter::new("wide", -3e38, 3e38, 0.0).is_err());
    assert!(Parameter::new("empty", 1.0, 1.0, 1.0).is_err());
    assert!(b.delay(0).is_err());
    let long = BlockBuilder::new("a".repeat(81), "Long", Category::Utility);
    let input = long.input();
    assert!(long.finish(input).is_err());
    let maximum = BlockBuilder::new("a".repeat(80), "Maximum", Category::Utility);
    let input = maximum.input();
    assert!(maximum.finish(input).is_ok());
}

#[test]
fn render_chunk_boundaries_and_in_place_processing_agree() {
    let block = examples::delay(4).unwrap();
    let input: Vec<f32> = (0..100).map(|n| (n as f32 * 0.17).sin()).collect();
    let mut all = input.clone();
    Processor::new(block.clone())
        .unwrap()
        .process_in_place(&mut all);
    let mut chunked = vec![0.0; input.len()];
    let mut processor = Processor::new(block).unwrap();
    for (src, dst) in input.chunks(7).zip(chunked.chunks_mut(7)) {
        processor.process(src, dst).unwrap();
    }
    assert_eq!(all, chunked);
    assert!(processor.process(&[0.0], &mut []).is_err());
}

/// One block per IR 2 macro, `output = f(input)`, run through the host reference.
fn unary(f: fn(&mut BlockBuilder, qcdsp::Signal) -> qcdsp::Result<qcdsp::Signal>) -> Processor {
    let mut dsp = BlockBuilder::new("unary", "Unary", Category::Utility);
    let input = dsp.input();
    let output = f(&mut dsp, input).unwrap();
    Processor::new(dsp.finish(output).unwrap()).unwrap()
}

#[test]
fn ir2_approximations_stay_within_their_documented_error() {
    let mut tanh = unary(BlockBuilder::tanh);
    let mut sigmoid = unary(BlockBuilder::sigmoid);
    let mut recip = unary(BlockBuilder::recip);
    for step in -40_000..=40_000 {
        let x = step as f32 * 2.5e-4;
        let t = tanh.process_sample(x * 3.0);
        assert!(
            (t - (x * 3.0).tanh()).abs() <= 1e-4,
            "tanh({}) = {t}",
            x * 3.0
        );
        let s = sigmoid.process_sample(x * 3.0);
        let exact = 1.0 / (1.0 + (-x * 3.0).exp());
        assert!((s - exact).abs() <= 5e-5, "sigmoid({}) = {s}", x * 3.0);
    }
    // Saturates and never leaves [-1, 1], even for huge inputs.
    for x in [5.0_f32, 50.0, 1e30, -1e30] {
        let t = tanh.process_sample(x);
        assert!(
            t.abs() <= 1.0 && (t - x.signum()).abs() <= 1e-4,
            "tanh({x}) = {t}"
        );
    }
    for exponent in -36..=36 {
        for mantissa in [1.0_f32, 1.37, 2.9, 7.5] {
            let x = mantissa * 10f32.powi(exponent);
            for x in [x, -x] {
                let r = recip.process_sample(x);
                assert!((r * x - 1.0).abs() <= 4e-7, "recip({x}) = {r}");
            }
        }
    }
}

#[test]
fn ir_version_is_the_oldest_that_holds_the_block() {
    assert_eq!(examples::gain().unwrap().version(), 1);
    let saturator = examples::saturator().unwrap();
    assert_eq!(saturator.version(), 2);
    assert_eq!(saturator.channels(), Channels::Mono);
    let json = saturator.to_json().unwrap();
    let decoded = Block::from_json(&json).unwrap();
    assert_eq!(decoded.channels(), Channels::Mono);
    // An IR 1 reader of the same graph must not accept IR 2 features.
    let downgraded = json.replace("\"version\": 2", "\"version\": 1");
    assert!(matches!(
        Block::from_json(&downgraded),
        Err(Error::Invalid(_))
    ));
    let stereo = json.replace("\"channels\": \"mono\",", "");
    let stereo_v1 = stereo.replace("\"version\": 2", "\"version\": 1");
    assert!(matches!(
        Block::from_json(&stereo_v1),
        Err(Error::Invalid(_))
    ));
    assert!(Block::from_json(&json.replace("\"version\": 2", "\"version\": 4")).is_err());
    // So must a category that IR 1 does not know.
    let gain = examples::gain().unwrap();
    let compressor = gain.clone().renamed("comp", "Comp", Category::Compressor);
    assert_eq!(compressor.unwrap().version(), 2);
    let json = gain.to_json().unwrap();
    let compressor_v1 = json.replace("\"utility\"", "\"compressor\"");
    assert!(matches!(
        Block::from_json(&compressor_v1),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn node_limit_bounds_the_expanded_graph() {
    // A sigmoid expands to 40 nodes; the input is one more.
    let chain = |sigmoids: usize| {
        let mut dsp = BlockBuilder::new("chain", "Chain", Category::Utility);
        let mut signal = dsp.input();
        for _ in 0..sigmoids {
            signal = dsp.sigmoid(signal).unwrap();
        }
        dsp.finish(signal)
    };
    let largest = (MAX_NODES - 1) / 40;
    let block = chain(largest).unwrap();
    assert!(block.expanded().node_count() <= MAX_NODES);
    assert!(matches!(chain(largest + 1), Err(Error::Invalid(_))));
}

#[test]
fn saturator_matches_its_formula_and_hoists_parameter_math() {
    let block = examples::saturator().unwrap();
    let mut processor = Processor::new(block.clone()).unwrap();
    processor.set_parameter(0, 12.0).unwrap();
    processor.set_parameter(1, 0.2).unwrap();
    processor.set_parameter(2, 0.8).unwrap();
    for step in -100..=100 {
        let x = step as f32 * 0.01;
        let expected = 0.8 * ((12.0 * x + 0.2).tanh() - 0.2_f32.tanh()) / 12.0_f32.tanh();
        let actual = processor.process_sample(x);
        assert!(
            (actual - expected).abs() <= 3e-4,
            "{x}: {actual} vs {expected}"
        );
    }
    // tanh(bias), tanh(drive) and its reciprocal depend only on parameters.
    let kernel = codegen::compile(&block).unwrap();
    assert!(kernel.contract.control_slots >= 2);
    let sample_computes = kernel
        .contract
        .sample
        .iter()
        .filter(|step| matches!(step, Step::Compute { .. }))
        .count();
    let all = kernel.contract.control.len() + kernel.contract.sample.len();
    assert!(
        sample_computes * 2 < all,
        "{sample_computes} of {all} steps run per sample"
    );
}
