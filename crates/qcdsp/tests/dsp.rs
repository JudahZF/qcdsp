use qcdsp::{Block, BlockBuilder, Category, Error, Parameter, Processor, codegen, examples};

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
fn allocator_reuses_temporaries_but_rejects_excess_input_pressure() {
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

    let mut dsp = BlockBuilder::new("pressure", "Pressure", Category::Utility);
    let mut output = dsp.input();
    for index in 0..16 {
        let parameter = dsp
            .parameter(Parameter::new(format!("p{index}"), 0.0, 1.0, 0.5).unwrap())
            .unwrap();
        output = dsp.add(output, parameter).unwrap();
    }
    assert!(matches!(
        codegen::compile(&dsp.finish(output).unwrap()),
        Err(Error::RegisterPressure)
    ));
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
    assert_eq!(kernel.contract.inputs.len(), 1);
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
