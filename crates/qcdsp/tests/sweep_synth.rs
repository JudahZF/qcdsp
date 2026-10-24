//! Behaviour of the Sweep Synth model, on the compiled kernel so the checks
//! cover what the device runs (up to float modes).

use std::f32::consts::TAU;

use qcdsp::{
    Processor,
    examples::{Voicing, sweep_synth},
};

const RATE: f32 = 48_000.0;

fn synth(voicing: Voicing, settings: &[(&str, f32)]) -> Processor {
    let block = sweep_synth(voicing).unwrap();
    let names: Vec<String> = block
        .parameters()
        .iter()
        .map(|parameter| parameter.name.clone())
        .collect();
    let mut processor = Processor::compiled(block).unwrap();
    for &(name, value) in settings {
        let index = names.iter().position(|known| known == name).unwrap();
        processor.set_parameter(index, value).unwrap();
    }
    processor
}

fn sine(hz: f32, amplitude: f32, seconds: f32) -> Vec<f32> {
    (0..(seconds * RATE) as usize)
        .map(|n| amplitude * (TAU * hz * n as f32 / RATE).sin())
        .collect()
}

/// Goertzel power of `hz` in `samples`.
fn power(samples: &[f32], hz: f32) -> f32 {
    let coefficient = 2.0 * (TAU * hz / RATE).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &sample in samples {
        let s0 = sample + coefficient * s1 - s2;
        (s2, s1) = (s1, s0);
    }
    s1 * s1 + s2 * s2 - coefficient * s1 * s2
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

#[test]
fn each_voice_sounds_at_its_interval() {
    for (voicing, dry, hz) in [
        (Voicing::Guitar, "GUITAR", 220.0),
        (Voicing::Bass, "BASS", 110.0),
    ] {
        // Filter fully open and flat, no attack delay, one voice at a time.
        let open = [
            ("START FREQ", 1.0),
            ("STOP FREQ", 1.0),
            ("RESONANCE", 0.0),
            ("ATTACK DELAY", 0.0),
            ("SUB OCTAVE", 0.0),
            (dry, 0.0),
            ("OCTAVE", 0.0),
            ("SQUARE WAVE", 0.0),
        ];
        for (voice, expected) in [("SUB OCTAVE", 0.5), ("SQUARE WAVE", 1.0), ("OCTAVE", 2.0)] {
            let mut settings = open.to_vec();
            settings.push((voice, 1.0));
            let mut processor = synth(voicing, &settings);
            let mut samples = sine(hz, 0.3, 1.0);
            processor.process_in_place(&mut samples);
            let tail = &samples[samples.len() / 2..];
            let strongest = [0.5, 1.0, 2.0]
                .into_iter()
                .max_by(|a, b| power(tail, a * hz).total_cmp(&power(tail, b * hz)))
                .unwrap();
            assert_eq!(strongest, expected, "{voicing:?} {voice}");
        }
    }
}

#[test]
fn each_trigger_restarts_the_filter_sweep() {
    for (voicing, hz) in [(Voicing::Guitar, 220.0), (Voicing::Bass, 110.0)] {
        // Square voice only; the cutoff rises from the bottom to the top.
        let mut processor = synth(
            voicing,
            &[
                ("SUB OCTAVE", 0.0),
                ("SQUARE WAVE", 1.0),
                ("START FREQ", 0.0),
                ("STOP FREQ", 1.0),
                ("RATE", 0.3),
                ("RESONANCE", 0.0),
            ],
        );
        let note = sine(hz, 0.3, 0.5);
        let window = (0.02 * RATE) as usize;
        for _ in 0..2 {
            let mut samples = note.clone();
            processor.process_in_place(&mut samples);
            let opening = rms(&samples[..window]);
            let later = rms(&samples[samples.len() - window..]);
            assert!(opening < 0.5 * later, "{voicing:?}: {opening} vs {later}");
            let mut silence = vec![0.0; (0.3 * RATE) as usize];
            processor.process_in_place(&mut silence);
        }
    }
}

#[test]
fn extreme_settings_and_infinite_input_stay_bounded_and_fall_silent() {
    for voicing in [Voicing::Guitar, Voicing::Bass] {
        let dry = if voicing == Voicing::Guitar {
            "GUITAR"
        } else {
            "BASS"
        };
        let mut processor = synth(
            voicing,
            &[
                ("TRIGGER", 0.0),
                ("SUB OCTAVE", 1.0),
                (dry, 1.0),
                ("OCTAVE", 1.0),
                ("SQUARE WAVE", 1.0),
                ("RESONANCE", 1.0),
                ("START FREQ", 1.0),
                ("STOP FREQ", 0.0),
                ("RATE", 1.0),
            ],
        );
        let mut seed = 1u32;
        let mut samples: Vec<f32> = (0..RATE as usize)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect();
        // One infinite sample each way must not latch any state.
        samples[1000] = f32::INFINITY;
        samples[1001] = f32::NEG_INFINITY;
        samples.extend(vec![0.0; RATE as usize]);
        processor.process_in_place(&mut samples);
        assert!(
            samples.iter().all(|sample| sample.abs() <= 4.0),
            "{voicing:?}"
        );
        let tail = &samples[samples.len() - (0.1 * RATE) as usize..];
        assert!(rms(tail) < 1e-4, "{voicing:?}: {}", rms(tail));
    }
}
