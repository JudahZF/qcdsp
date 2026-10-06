//! Small reference blocks. Coefficients and delay lengths are supplied in DSP
//! units; sample-rate conversion and QC parameter delivery are not inferred.

use crate::{Block, BlockBuilder, Category, Parameter, Result};

pub fn gain() -> Result<Block> {
    let mut dsp = BlockBuilder::new("gain", "Gain", Category::Utility);
    let input = dsp.input();
    let gain = dsp.parameter(Parameter::new("Gain", 0.0, 4.0, 1.0)?)?;
    let output = dsp.mul(input, gain)?;
    dsp.finish(output)
}

/// Direct-form II transposed biquad. Coefficients are [b0, b1, b2, a1, a2],
/// already divided by a0. The caller is responsible for filter stability.
pub fn biquad(coefficients: [f32; 5]) -> Result<Block> {
    let mut dsp = BlockBuilder::new("biquad", "Biquad EQ", Category::Equalizer);
    let input = dsp.input();
    let z1 = dsp.state(0.0)?;
    let z2 = dsp.state(0.0)?;
    let b0 = dsp.constant(coefficients[0])?;
    let b1 = dsp.constant(coefficients[1])?;
    let b2 = dsp.constant(coefficients[2])?;
    let a1 = dsp.constant(coefficients[3])?;
    let a2 = dsp.constant(coefficients[4])?;
    let feed = dsp.mul(input, b0)?;
    let output = dsp.add(feed, z1.signal())?;
    let feed1 = dsp.mul(input, b1)?;
    let back1 = dsp.mul(output, a1)?;
    let next1 = dsp.sub(feed1, back1)?;
    let next1 = dsp.add(next1, z2.signal())?;
    let feed2 = dsp.mul(input, b2)?;
    let back2 = dsp.mul(output, a2)?;
    let next2 = dsp.sub(feed2, back2)?;
    dsp.update_state(z1, next1)?;
    dsp.update_state(z2, next2)?;
    dsp.finish(output)
}

pub fn delay(samples: usize) -> Result<Block> {
    let mut dsp = BlockBuilder::new("delay", "Delay", Category::Delay);
    let input = dsp.input();
    let line = dsp.delay(samples)?;
    let mix = dsp.parameter(Parameter::new("Mix", 0.0, 1.0, 0.5)?)?;
    let feedback = dsp.parameter(Parameter::new("Feedback", 0.0, 0.95, 0.25)?)?;
    let one = dsp.constant(1.0)?;
    let dry_gain = dsp.sub(one, mix)?;
    let dry = dsp.mul(input, dry_gain)?;
    let wet = dsp.mul(line.signal(), mix)?;
    let output = dsp.add(dry, wet)?;
    let returned = dsp.mul(line.signal(), feedback)?;
    let write = dsp.add(input, returned)?;
    dsp.update_delay(line, write)?;
    dsp.finish(output)
}

/// A bounded cubic soft clipper: 1.5*x - 0.5*x^3 on [-1, 1].
pub fn soft_clip() -> Result<Block> {
    let mut dsp = BlockBuilder::new("soft_clip", "Simple Drive", Category::GuitarOverdrive);
    let input = dsp.input();
    let gain = dsp.parameter(Parameter::new("Drive", 0.0, 10.0, 2.0)?)?;
    let level = dsp.parameter(Parameter::new("Level", 0.0, 1.0, 0.5)?)?;
    let low = dsp.constant(-1.0)?;
    let high = dsp.constant(1.0)?;
    let driven = dsp.mul(input, gain)?;
    let limited = dsp.max(driven, low)?;
    let limited = dsp.min(limited, high)?;
    let square = dsp.mul(limited, limited)?;
    let cube = dsp.mul(square, limited)?;
    let a = dsp.constant(1.5)?;
    let b = dsp.constant(0.5)?;
    let linear = dsp.mul(limited, a)?;
    let cubic = dsp.mul(cube, b)?;
    let clipped = dsp.sub(linear, cubic)?;
    let output = dsp.mul(clipped, level)?;
    dsp.finish(output)
}
