//! DSP pieces shared by the examples, built from the plain DSL operations.
//! Constants assume the QC's 48 kHz sample rate.

use std::f32::consts::{PI, TAU};

use crate::{BlockBuilder, Result, Signal};

pub const SAMPLE_RATE: f32 = 48_000.0;

/// Large enough that `clamp(x * SATURATE, -1, 1)` is a comparator for any
/// audible `x`.
pub const SATURATE: f32 = 1.0e6;

/// One-pole coefficient for time constant `seconds`.
pub fn smoothing(seconds: f32) -> f32 {
    1.0 - (-1.0 / (seconds * SAMPLE_RATE)).exp()
}

/// One-pole low-pass coefficient for corner frequency `hz`.
pub fn corner(hz: f32) -> f32 {
    1.0 - (-TAU * hz / SAMPLE_RATE).exp()
}

/// Chamberlin SVF frequency coefficient for `hz`, run twice per sample.
pub fn svf_coefficient(hz: f32) -> f32 {
    2.0 * (PI * hz / (2.0 * SAMPLE_RATE)).sin()
}

/// The block input limited to ±16 (+24 dBFS, far above any real signal). One
/// infinite sample would otherwise make `inf - inf` and latch every filter and
/// envelope state at NaN until reset. A NaN input is not handled: SHARC
/// MIN/MAX behaviour on NaN is unverified.
pub fn guarded_input(dsp: &mut BlockBuilder) -> Result<Signal> {
    let input = dsp.input();
    clamp(dsp, input, -16.0, 16.0)
}

pub fn clamp(dsp: &mut BlockBuilder, input: Signal, low: f32, high: f32) -> Result<Signal> {
    let low = dsp.constant(low)?;
    let high = dsp.constant(high)?;
    let raised = dsp.max(input, low)?;
    dsp.min(raised, high)
}

/// `a + (b - a) * t`.
pub fn lerp(dsp: &mut BlockBuilder, a: Signal, b: Signal, t: Signal) -> Result<Signal> {
    let span = dsp.sub(b, a)?;
    let step = dsp.mul(span, t)?;
    dsp.add(a, step)
}

/// One-pole low-pass `y += k (x - y)`; returns this sample's output.
pub fn one_pole(dsp: &mut BlockBuilder, input: Signal, k: Signal) -> Result<Signal> {
    let state = dsp.state(0.0)?;
    let output = lerp(dsp, state.signal(), input, k)?;
    dsp.update_state(state, output)?;
    Ok(output)
}

/// `2^w` for `w` in `[low, high]`: a cubic for `2^u` on `[-1/2, 1/2]` (relative
/// error 1.2e-4), squared back up. Each squaring doubles the error, so wide
/// ranges are less exact (6 octaves: 0.1%).
pub fn pow2(dsp: &mut BlockBuilder, w: Signal, low: f32, high: f32) -> Result<Signal> {
    let middle = (low + high) / 2.0;
    let mut half_width = (high - low) / 2.0;
    let mut squarings = 0;
    while half_width > 0.5 {
        half_width /= 2.0;
        squarings += 1;
    }
    let offset = dsp.constant(-middle)?;
    let centered = dsp.add(w, offset)?;
    let scale = dsp.constant(0.5f32.powi(squarings))?;
    let u = dsp.mul(centered, scale)?;
    let mut power = dsp.constant(0.054_592_825)?;
    for coefficient in [0.242_217_84, 0.693_368_6, 1.0] {
        power = dsp.mul(power, u)?;
        let coefficient = dsp.constant(coefficient)?;
        power = dsp.add(power, coefficient)?;
    }
    for _ in 0..squarings {
        power = dsp.mul(power, power)?;
    }
    let base = dsp.constant(middle.exp2())?;
    dsp.mul(power, base)
}

/// Envelope of `|input|` with separate one-pole attack and release times.
pub fn envelope(
    dsp: &mut BlockBuilder,
    input: Signal,
    attack: f32,
    release: f32,
) -> Result<Signal> {
    let level = dsp.state(0.0)?;
    let rectified = dsp.abs(input)?;
    let attack = dsp.constant(smoothing(attack))?;
    let release = dsp.constant(smoothing(release))?;
    let rising = lerp(dsp, level.signal(), rectified, attack)?;
    let falling = lerp(dsp, level.signal(), rectified, release)?;
    // Above the level the faster attack step is larger; below it the slower
    // release step is, so max picks the right one either way.
    let next = dsp.max(rising, falling)?;
    dsp.update_state(level, next)?;
    Ok(next)
}

/// Chamberlin state-variable filter, run twice per sample for stability up to
/// about 8 kHz. `g` comes from [`svf_coefficient`]; `damping` is 1/Q. Returns
/// this sample's (low-pass, band-pass). States are bounded to ±4 so extreme
/// settings saturate instead of running away.
pub fn svf(
    dsp: &mut BlockBuilder,
    input: Signal,
    g: Signal,
    damping: Signal,
) -> Result<(Signal, Signal)> {
    let low_state = dsp.state(0.0)?;
    let band_state = dsp.state(0.0)?;
    let (mut low, mut band) = (low_state.signal(), band_state.signal());
    for _ in 0..2 {
        let step = dsp.mul(g, band)?;
        low = dsp.add(low, step)?;
        let resonance = dsp.mul(damping, band)?;
        let high = dsp.sub(input, low)?;
        let high = dsp.sub(high, resonance)?;
        let step = dsp.mul(g, high)?;
        band = dsp.add(band, step)?;
    }
    let low = clamp(dsp, low, -4.0, 4.0)?;
    let band = clamp(dsp, band, -4.0, 4.0)?;
    dsp.update_state(low_state, low)?;
    dsp.update_state(band_state, band)?;
    Ok((low, band))
}
