use qcdsp::{BlockBuilder, Category, Parameter, Processor, codegen};

fn main() -> qcdsp::Result<()> {
    let mut dsp = BlockBuilder::new("custom_gain", "Custom Gain", Category::Utility);
    let input = dsp.input();
    let level = dsp.parameter(Parameter::new("Level", 0.0, 2.0, 1.0)?)?;
    let output = dsp.mul(input, level)?;
    let block = dsp.finish(output)?;

    let mut reference = Processor::new(block.clone())?;
    reference.set_parameter(0, 0.5)?;
    assert_eq!(reference.process_sample(1.0), 0.5);

    let kernel = codegen::compile(&block)?;
    println!("{}", block.to_json()?);
    eprintln!(
        "{} instructions, {} ISA bytes; not a QC callback",
        kernel.contract.instruction_count,
        kernel.encode()?.len()
    );
    Ok(())
}
