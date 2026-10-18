use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "qcdsp-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-qcdsp"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn fragment_outputs_a_non_installable_kernel_and_refuses_overwrite() {
    let scratch = Scratch::new();
    let args = [
        "qcdsp",
        "fragment",
        "--example",
        "gain",
        "--output",
        "gain.qcfragment",
    ];
    let first = scratch.run(&args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let directory = scratch.0.join("gain.qcfragment");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["installable"], false);
    assert_eq!(manifest["section"], 3);
    assert_eq!(manifest["address_unit_bytes"], 6);
    let code = fs::read(directory.join("kernel.isa48")).unwrap();
    assert_eq!(code.len(), 6);
    assert!(!scratch.run(&args).status.success());
    assert_eq!(fs::read(directory.join("kernel.isa48")).unwrap(), code);
}

#[test]
fn exported_ir_can_be_imported_and_rendered_in_both_execution_modes() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("samples.json"), "[1.0,0.5,0.0,-0.5]").unwrap();
    assert!(
        scratch
            .run(&["export-ir", "--example", "gain", "--output", "gain.json"])
            .status
            .success()
    );
    for (output, mode) in [("host.json", false), ("compiled.json", true)] {
        let mut args = vec![
            "render",
            "--block",
            "gain.json",
            "--input",
            "samples.json",
            "--output",
            output,
            "--param",
            "0=2.0",
        ];
        if mode {
            args.push("--compiled");
        }
        let result = scratch.run(&args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let samples: Vec<f32> =
            serde_json::from_slice(&fs::read(scratch.0.join(output)).unwrap()).unwrap();
        assert_eq!(samples, [2.0, 1.0, 0.0, -1.0]);
    }
    assert!(
        scratch
            .run(&["fragment", "--block", "gain.json", "--output", "imported"])
            .status
            .success()
    );
}

#[test]
fn wav_render_reads_the_first_channel_and_takes_parameter_names() {
    let scratch = Scratch::new();
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(scratch.0.join("in.wav"), spec).unwrap();
    for sample in [16384i16, 1000, -8192, 1000] {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    let args = [
        "render",
        "--example",
        "gain",
        "--input",
        "in.wav",
        "--output",
        "out.wav",
        "--param",
        "gain=2",
    ];
    let result = scratch.run(&args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut reader = hound::WavReader::open(scratch.0.join("out.wav")).unwrap();
    assert_eq!(reader.spec().channels, 1);
    let samples: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
    assert_eq!(samples, [1.0, -0.5]);
}

#[test]
fn invalid_sources_and_non_finite_parameters_do_not_create_output() {
    let scratch = Scratch::new();
    assert!(
        !scratch
            .run(&["fragment", "--output", "missing"])
            .status
            .success()
    );
    assert!(
        !scratch
            .run(&[
                "fragment",
                "--example",
                "gain",
                "--block",
                "anything.json",
                "--output",
                "ambiguous"
            ])
            .status
            .success()
    );
    fs::write(scratch.0.join("input.json"), "[0.0]").unwrap();
    assert!(
        !scratch
            .run(&[
                "render",
                "--example",
                "gain",
                "--input",
                "input.json",
                "--output",
                "bad.json",
                "--param",
                "0=NaN"
            ])
            .status
            .success()
    );
    assert!(!scratch.0.join("missing").exists());
    assert!(!scratch.0.join("ambiguous").exists());
    assert!(!scratch.0.join("bad.json").exists());
    // Building for the device needs the firmware copy, pin, keys and serial.
    assert!(
        !scratch
            .run(&["build", "--example", "gain", "--output", "stage"])
            .status
            .success()
    );
    assert!(!scratch.0.join("stage").exists());
}
