//! Reference blocks. The first four take coefficients and delay lengths in DSP
//! units; the rest are complete effects for the QC's 48 kHz sample rate.

mod kit;
mod micro_synth;

use std::f32::consts::TAU;

pub use micro_synth::{Voicing, micro_synth};

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

/// Sine tremolo. RATE is 0.5 to 16 Hz on an exponential taper; DEPTH 1 swings
/// the level between silence and unity. The LFO is a magic-circle oscillator
/// (two states), so it needs no sine function.
pub fn tremolo() -> Result<Block> {
    let mut dsp = BlockBuilder::new("tremolo", "Sine Tremolo", Category::Modulation);
    let input = dsp.input();
    let rate = dsp.parameter(Parameter::new("RATE", 0.0, 1.0, 0.5)?)?;
    let depth = dsp.parameter(Parameter::new("DEPTH", 0.0, 1.0, 0.5)?)?;
    // Angular step per sample: 2 pi * 0.5 Hz * 2^(5 rate) / fs.
    let five = dsp.constant(5.0)?;
    let octaves = dsp.mul(rate, five)?;
    let speed = kit::pow2(&mut dsp, octaves, 0.0, 5.0)?;
    let slowest = dsp.constant(TAU * 0.5 / kit::SAMPLE_RATE)?;
    let step = dsp.mul(speed, slowest)?;
    let sine = dsp.state(0.0)?;
    let cosine = dsp.state(1.0)?;
    let turn = dsp.mul(step, cosine.signal())?;
    let next_sine = dsp.add(sine.signal(), turn)?;
    let turn = dsp.mul(step, next_sine)?;
    let next_cosine = dsp.sub(cosine.signal(), turn)?;
    dsp.update_state(sine, next_sine)?;
    dsp.update_state(cosine, next_cosine)?;
    // gain = 1 - depth * (1 + sine) / 2
    let one = dsp.constant(1.0)?;
    let half = dsp.constant(0.5)?;
    let lifted = dsp.add(next_sine, one)?;
    let lifted = dsp.mul(lifted, half)?;
    let dip = dsp.mul(lifted, depth)?;
    let gain = dsp.sub(one, dip)?;
    let output = dsp.mul(input, gain)?;
    dsp.finish(output)
}

/// Envelope filter (auto-wah): a resonant band-pass from 150 Hz to 4.8 kHz,
/// opened by the playing level. FREQ sets where it rests, RANGE how far the
/// envelope pushes it, SENSITIVITY the envelope gain and MIX the wet share.
pub fn envelope_filter() -> Result<Block> {
    const OCTAVES: f32 = 5.0;
    let mut dsp = BlockBuilder::new("envelope_filter", "Envelope Filter", Category::Filter);
    let input = kit::guarded_input(&mut dsp)?;
    let sensitivity = dsp.parameter(Parameter::new("SENSITIVITY", 0.0, 1.0, 0.5)?)?;
    let frequency = dsp.parameter(Parameter::new("FREQ", 0.0, 1.0, 0.2)?)?;
    let range = dsp.parameter(Parameter::new("RANGE", 0.0, 1.0, 0.7)?)?;
    let resonance = dsp.parameter(Parameter::new("RESONANCE", 0.0, 1.0, 0.6)?)?;
    let mix = dsp.parameter(Parameter::new("MIX", 0.0, 1.0, 1.0)?)?;
    // Envelope gain 2^(10 s - 1): x0.5 to x512.
    let ten = dsp.constant(10.0)?;
    let exponent = dsp.mul(sensitivity, ten)?;
    let minus_one = dsp.constant(-1.0)?;
    let exponent = dsp.add(exponent, minus_one)?;
    let drive = kit::pow2(&mut dsp, exponent, -1.0, 9.0)?;
    // Damping 1.4 down to 0.1, as 1/Q.
    let one = dsp.constant(1.0)?;
    let open = dsp.sub(one, resonance)?;
    let open = dsp.mul(open, open)?;
    let span = dsp.constant(1.3)?;
    let damping = dsp.mul(open, span)?;
    let least = dsp.constant(0.1)?;
    let damping = dsp.add(damping, least)?;

    let level = kit::envelope(&mut dsp, input, 0.002, 0.06)?;
    let push = dsp.mul(level, drive)?;
    let push = dsp.mul(push, range)?;
    let position = dsp.add(frequency, push)?;
    let position = kit::clamp(&mut dsp, position, 0.0, 1.0)?;
    let octaves = dsp.constant(OCTAVES)?;
    let exponent = dsp.mul(position, octaves)?;
    let cutoff = kit::pow2(&mut dsp, exponent, 0.0, OCTAVES)?;
    let base = dsp.constant(kit::svf_coefficient(150.0))?;
    let g = dsp.mul(cutoff, base)?;
    let (_, band) = kit::svf(&mut dsp, input, g, damping)?;
    // The band-pass peaks at 1/damping; scale it back to unity.
    let wet = dsp.mul(band, damping)?;
    let output = kit::lerp(&mut dsp, input, wet, mix)?;
    dsp.finish(output)
}
