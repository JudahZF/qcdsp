//! A model of the Electro-Harmonix Micro Synthesizer and Bass Micro Synthesizer.
//!
//! Signal flow follows the pedals: four voices (sub octave, dry, octave up,
//! square wave) are mixed into a resonant low-pass whose cutoff sweeps from
//! START FREQ to STOP FREQ at RATE each time the input crosses the TRIGGER
//! level, then through an attack-delay VCA. The square wave is a comparator,
//! the sub octave a flip-flop on it and the octave up a full-wave rectifier, so
//! there is no pitch detection: like the pedal, it tracks single notes.
//!
//! This is a behavioural model tuned by ear, not a circuit model. Per EHX the
//! bass version only changes the trigger and the filter sweep range; its
//! envelope smoothing capacitors are about 2.2x larger, so its envelope is
//! about 2.2x slower here, and its filter range sits one octave lower.
//!
//! The pedals are mono, so the block is mono (IR 2): it computes channel A and
//! copies it to channel B, at half the cost of a stereo block.

use super::kit::{self, SATURATE};
use crate::{Block, BlockBuilder, Category, Parameter, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voicing {
    Guitar,
    Bass,
}

struct Tuning {
    blob_name: &'static str,
    name: &'static str,
    dry: &'static str,
    /// Corner of the two low-passes in front of the comparator.
    track_hz: f32,
    /// Cutoff at START/STOP FREQ = 0; the sweep spans `OCTAVES` above it.
    filter_hz: f32,
    attack: f32,
    release: f32,
}

impl Voicing {
    fn tuning(self) -> Tuning {
        match self {
            Self::Guitar => Tuning {
                blob_name: "micro_synth",
                name: "Micro Synth",
                dry: "GUITAR",
                track_hz: 500.0,
                filter_hz: 80.0,
                attack: 0.001,
                release: 0.02,
            },
            Self::Bass => Tuning {
                blob_name: "bass_micro_synth",
                name: "Bass Micro Synth",
                dry: "BASS",
                track_hz: 250.0,
                filter_hz: 40.0,
                attack: 0.0022,
                release: 0.044,
            },
        }
    }
}

/// Filter sweep range above the voicing's base cutoff.
const OCTAVES: f32 = 6.0;
/// Comparator hysteresis as a fraction of the envelope: rejects noise and
/// ringing near zero crossings without missing real ones.
const HYSTERESIS: f32 = 0.1;
/// The square and sub voices take their level from the envelope (times this,
/// up to `SYNTH_LEVEL`), so they sustain like a fuzz and then die with the note.
const SUSTAIN: f32 = 3.0;
const SYNTH_LEVEL: f32 = 0.3;

pub fn micro_synth(voicing: Voicing) -> Result<Block> {
    let tuning = voicing.tuning();
    let mut dsp = BlockBuilder::new(tuning.blob_name, tuning.name, Category::Pitch);
    dsp.mono();
    let input = kit::guarded_input(&mut dsp)?;
    // Controls in the pedal's left-to-right order. All are 0..1 like its sliders.
    let control = |dsp: &mut BlockBuilder, name: &str, default: f32| {
        dsp.parameter(Parameter::new(name, 0.0, 1.0, default)?)
    };
    let trigger = control(&mut dsp, "TRIGGER", 0.3)?;
    let sub_level = control(&mut dsp, "SUB OCTAVE", 0.7)?;
    let dry_level = control(&mut dsp, tuning.dry, 0.0)?;
    let octave_level = control(&mut dsp, "OCTAVE", 0.0)?;
    let square_level = control(&mut dsp, "SQUARE WAVE", 0.5)?;
    let attack_delay = control(&mut dsp, "ATTACK DELAY", 0.0)?;
    let resonance = control(&mut dsp, "RESONANCE", 0.5)?;
    let start = control(&mut dsp, "START FREQ", 0.85)?;
    let stop = control(&mut dsp, "STOP FREQ", 0.15)?;
    let rate = control(&mut dsp, "RATE", 0.6)?;

    // Control laws. These depend only on parameters, so the compiler computes
    // them once per block instead of per sample.
    // TRIGGER: on-level from -66 dBFS (2^-11) to -12 dBFS (2^-2); off at half.
    let nine = dsp.constant(9.0)?;
    let scaled = dsp.mul(trigger, nine)?;
    let floor = dsp.constant(-11.0)?;
    let exponent = dsp.add(scaled, floor)?;
    let on_level = kit::pow2(&mut dsp, exponent, -11.0, -2.0)?;
    let half = dsp.constant(0.5)?;
    let hysteresis = dsp.mul(on_level, half)?;
    // Voice sliders: audio taper.
    let taper = |dsp: &mut BlockBuilder, level| dsp.mul(level, level);
    let sub_gain = taper(&mut dsp, sub_level)?;
    let dry_gain = taper(&mut dsp, dry_level)?;
    let octave_gain = taper(&mut dsp, octave_level)?;
    let square_gain = taper(&mut dsp, square_level)?;
    // ATTACK DELAY: per-sample step 2^(-16 a (2 - a)): instant at 0, about
    // 85 ms at the midpoint, 1.4 s at the top.
    let two = dsp.constant(2.0)?;
    let bend = dsp.sub(two, attack_delay)?;
    let bend = dsp.mul(attack_delay, bend)?;
    let minus_sixteen = dsp.constant(-16.0)?;
    let exponent = dsp.mul(bend, minus_sixteen)?;
    let attack_step = kit::pow2(&mut dsp, exponent, -16.0, 0.0)?;
    // RATE: per-sample sweep step 2^(8 r - 16), time constant 1.4 s to 5 ms.
    let eight = dsp.constant(8.0)?;
    let scaled = dsp.mul(rate, eight)?;
    let exponent = dsp.add(scaled, minus_sixteen)?;
    let sweep_step = kit::pow2(&mut dsp, exponent, -16.0, -8.0)?;
    // RESONANCE: damping 1.4 (none) down to 0.15 (Q about 6.7).
    let one = dsp.constant(1.0)?;
    let open = dsp.sub(one, resonance)?;
    let open = dsp.mul(open, open)?;
    let range = dsp.constant(1.25)?;
    let damping = dsp.mul(open, range)?;
    let least = dsp.constant(0.15)?;
    let damping = dsp.add(damping, least)?;
    // The low-pass peaks at 1/damping (6.7x at full RESONANCE). Pull the input
    // down as resonance rises (to 0.4x at the top) so the peak stays near 2.7x.
    let squared = dsp.mul(resonance, resonance)?;
    let pull = dsp.constant(0.6)?;
    let pull = dsp.mul(squared, pull)?;
    let tame = dsp.sub(one, pull)?;

    // Envelope, trigger with hysteresis, and the edge that restarts the sweep.
    let level = kit::envelope(&mut dsp, input, tuning.attack, tuning.release)?;
    let gate = dsp.state(0.0)?;
    let held = dsp.mul(hysteresis, gate.signal())?;
    let threshold = dsp.sub(on_level, held)?;
    let excess = dsp.sub(level, threshold)?;
    let big = dsp.constant(SATURATE)?;
    let excess = dsp.mul(excess, big)?;
    let gated = kit::clamp(&mut dsp, excess, 0.0, 1.0)?;
    let rise = dsp.sub(gated, gate.signal())?;
    let zero = dsp.constant(0.0)?;
    let fire = dsp.max(rise, zero)?;
    dsp.update_state(gate, gated)?;

    // Filter sweep: an RC-style glide toward STOP in log frequency, restarted
    // at START by each trigger.
    let position = dsp.state(0.5)?;
    let gliding = kit::lerp(&mut dsp, position.signal(), stop, sweep_step)?;
    let swept = kit::lerp(&mut dsp, gliding, start, fire)?;
    dsp.update_state(position, swept)?;

    // Attack delay: the VCA restarts from zero at each trigger and rises
    // toward one; with ATTACK DELAY at 0 the step is 1, so it stays open.
    let gain = dsp.state(1.0)?;
    let kept = dsp.sub(one, fire)?;
    let restarted = dsp.mul(gain.signal(), kept)?;
    let vca = kit::lerp(&mut dsp, restarted, one, attack_step)?;
    dsp.update_state(gain, vca)?;

    // Voices, from a low-passed copy so the comparator follows the fundamental.
    let track = dsp.constant(kit::corner(tuning.track_hz))?;
    let tracked = kit::one_pole(&mut dsp, input, track)?;
    let fundamental = kit::one_pole(&mut dsp, tracked, track)?;
    // Square wave: comparator with hysteresis.
    let square = dsp.state(1.0)?;
    let margin = dsp.constant(HYSTERESIS)?;
    let margin = dsp.mul(level, margin)?;
    let bias = dsp.mul(margin, square.signal())?;
    let compared = dsp.add(fundamental, bias)?;
    let compared = dsp.mul(compared, big)?;
    let squared = kit::clamp(&mut dsp, compared, -1.0, 1.0)?;
    // Sub octave: a flip-flop that toggles on each rising edge of the square.
    let sub = dsp.state(1.0)?;
    let edge = dsp.sub(squared, square.signal())?;
    let edge = dsp.max(edge, zero)?;
    let flipped = dsp.mul(edge, sub.signal())?;
    let flipped = dsp.sub(sub.signal(), flipped)?;
    // The +1 keeps it at exactly ±1 even if an edge was ever partial.
    let flipped = dsp.mul(flipped, big)?;
    let flipped = dsp.add(flipped, one)?;
    let divided = kit::clamp(&mut dsp, flipped, -1.0, 1.0)?;
    dsp.update_state(square, squared)?;
    dsp.update_state(sub, divided)?;
    // Octave up: full-wave rectifier with its DC removed.
    let rectified = dsp.abs(tracked)?;
    let dc_corner = dsp.constant(kit::corner(20.0))?;
    let dc = kit::one_pole(&mut dsp, rectified, dc_corner)?;
    let doubled = dsp.sub(rectified, dc)?;
    let doubled = dsp.mul(doubled, two)?;

    let sustain = dsp.constant(SUSTAIN)?;
    let synth = dsp.mul(level, sustain)?;
    let ceiling = dsp.constant(SYNTH_LEVEL)?;
    let synth = dsp.min(synth, ceiling)?;
    let square_voice = dsp.mul(squared, square_gain)?;
    let sub_voice = dsp.mul(divided, sub_gain)?;
    let synth_voices = dsp.add(square_voice, sub_voice)?;
    let synth_voices = dsp.mul(synth_voices, synth)?;
    let dry_voice = dsp.mul(input, dry_gain)?;
    let octave_voice = dsp.mul(doubled, octave_gain)?;
    let mix = dsp.add(dry_voice, octave_voice)?;
    let mix = dsp.add(mix, synth_voices)?;

    // Resonant low-pass at base * 2^(OCTAVES * position).
    let octaves = dsp.constant(OCTAVES)?;
    let exponent = dsp.mul(swept, octaves)?;
    let cutoff = kit::pow2(&mut dsp, exponent, 0.0, OCTAVES)?;
    let base = dsp.constant(kit::svf_coefficient(tuning.filter_hz))?;
    let g = dsp.mul(cutoff, base)?;
    let mix = dsp.mul(mix, tame)?;
    let (low, _) = kit::svf(&mut dsp, mix, g, damping)?;
    let output = dsp.mul(low, vca)?;
    dsp.finish(output)
}
