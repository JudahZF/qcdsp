use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use qc_format::diff::DiffFile;
use qcdsp::{
    Block, Processor, codegen,
    device::{self, VolumeTemplate},
    examples,
};
use serde::Serialize;

mod keys;

#[derive(Parser)]
#[command(
    name = "cargo qcdsp",
    version,
    about = "Build Quad Cortex DSP blocks, test them on the host, and install them."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a block's `.qcdsp.json` graph (for a Tetrad extension package, or to edit).
    Export {
        #[command(flatten)]
        source: Source,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Compile a block into an encrypted, installable stage for this device.
    /// Reads the user's own firmware copy; writes only the new stage directory.
    Build {
        #[command(flatten)]
        source: Source,
        #[command(flatten)]
        device: DeviceArgs,
        /// Unused ModelRepo id for the new block.
        #[arg(long)]
        model_id: u32,
        /// Display name; defaults to `"<block name> (qcdsp)"`.
        #[arg(long)]
        name: Option<String>,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Install a stage on the device over SSH. Prints the plan unless
    /// --confirm-device-write is given. Does not reboot.
    Install {
        #[arg(long)]
        stage: PathBuf,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Restore the files saved by an install. Prints the plan unless confirmed.
    Uninstall {
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Find the report seed in your own ZenUI copy and write it as one hex line
    /// to a new private file. Append that line to your key file.
    FindSeed {
        /// Local copy of the device's /opt/neuraldsp.
        #[arg(long)]
        snapshot: PathBuf,
        /// File holding the device serial (/etc/qc_sn).
        #[arg(long)]
        serial_file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Emit the scalar kernel as a raw ISA48 fragment for inspection (not a block).
    Fragment {
        #[command(flatten)]
        source: Source,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Export an example's portable DSP expression graph.
    ExportIr {
        #[command(flatten)]
        source: Source,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Process mono audio through the host reference. Input and output are a
    /// JSON array of f32 samples or a WAV file (by extension); WAV input is
    /// read from its first channel and written back as 32-bit float.
    Render {
        #[command(flatten)]
        source: Source,
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Use the lowered instruction-list interpreter (not a SHARC emulator).
        #[arg(long)]
        compiled: bool,
        /// Set a parameter by index or name, e.g. --param 0=2.0 or
        /// --param "start freq=0.8". Values are DSP units.
        #[arg(long = "param", value_parser = parse_parameter)]
        parameters: Vec<(String, f32)>,
    },
    /// Read a decrypted ModuleBlob and print section sizes, without dumping data.
    Inspect { blob: PathBuf },
    /// Stage a stock volume clone (an install test of the framework alone). No installation.
    StageClone {
        #[command(flatten)]
        device: DeviceArgs,
        #[arg(long)]
        id: u32,
        #[arg(long, default_value = "Volume (qcdsp)")]
        name: String,
        #[arg(long, default_value = "qcdsp_volume")]
        blob: String,
        #[arg(short, long)]
        output: PathBuf,
    },
}

#[derive(Args)]
struct DeviceArgs {
    /// Local copy of the device's /opt/neuraldsp (never the live path).
    #[arg(long)]
    snapshot: PathBuf,
    /// JSON KernelPin: the SHA-256 of this unit's two ZPUKernel files.
    #[arg(long)]
    pin: PathBuf,
    /// Private (0600) file of hex lines: repo key, blob key and report seed.
    #[arg(long)]
    keys: PathBuf,
    /// File holding the device serial (/etc/qc_sn).
    #[arg(long)]
    serial_file: PathBuf,
}

impl DeviceArgs {
    fn open(&self) -> Result<(qc_deploy::Snapshot, keys::DeviceSecrets), Box<dyn Error>> {
        let snapshot = qc_deploy::Snapshot::open(&self.snapshot, &known_pins(&self.pin)?)?;
        let secrets = keys::load(snapshot.root(), &self.keys, &self.serial_file)?;
        Ok((snapshot, secrets))
    }
}

#[derive(Args)]
struct TargetArgs {
    /// JSON KernelPin: the SHA-256 of this unit's two ZPUKernel files.
    #[arg(long)]
    pin: PathBuf,
    /// File holding the expected device serial (/etc/qc_sn).
    #[arg(long)]
    serial_file: PathBuf,
    /// New local directory for the recovery backup (install) or the existing one (uninstall).
    #[arg(long)]
    backup: PathBuf,
    /// Unique install token; names the private on-device staging directory.
    #[arg(long)]
    token: String,
    #[arg(long, default_value = "qc")]
    host: String,
    /// SSH ProxyCommand, e.g. the byte-at-a-time banner proxy the QC's OpenSSH
    /// 6.7 can need. Without it, ssh uses your SSH config for the host.
    #[arg(long)]
    proxy_command: Option<String>,
    /// Actually write to the device. Without it only the exact commands are printed.
    #[arg(long)]
    confirm_device_write: bool,
}

impl TargetArgs {
    fn transport(&self) -> Result<qc_deploy::SshTransport, Box<dyn Error>> {
        let transport = qc_deploy::SshTransport::new(self.host.clone())?;
        Ok(match &self.proxy_command {
            Some(command) => transport.with_proxy_command(command)?,
            None => transport,
        })
    }

    fn options(
        &self,
    ) -> Result<(Vec<qc_deploy::FirmwarePin>, qc_deploy::DeviceOptions), Box<dyn Error>> {
        let pins = known_pins(&self.pin)?;
        let options = qc_deploy::DeviceOptions {
            token: self.token.clone(),
            expected_serial: fs::read_to_string(&self.serial_file)?.trim().to_owned(),
            backup_directory: self.backup.clone(),
            dry_run: !self.confirm_device_write,
            confirm_device_write: self.confirm_device_write,
        };
        Ok((pins, options))
    }
}

/// The pins of the supported CorOS builds for the unit whose kernel pin is at `path`.
fn known_pins(path: &Path) -> Result<Vec<qc_deploy::FirmwarePin>, Box<dyn Error>> {
    let kernels: qc_deploy::KernelPin = serde_json::from_slice(&fs::read(path)?)?;
    Ok(qc_deploy::known_pins(&kernels))
}

fn report(outcome: qc_deploy::InstallOutcome) {
    match outcome {
        qc_deploy::InstallOutcome::DryRun { commands } => {
            println!("Dry run: nothing was sent. Commands that would run:");
            for command in commands {
                println!("  {command}");
            }
            println!("Re-run with --confirm-device-write to execute.");
        }
        qc_deploy::InstallOutcome::Installed { receipt } => {
            println!(
                "Installed {} (token {}). Reboot the device to load it.",
                receipt.manifest.blob, receipt.token
            );
        }
        qc_deploy::InstallOutcome::Uninstalled { receipt } => {
            println!(
                "Restored the files saved by install {}. Reboot the device.",
                receipt.token
            );
        }
    }
}

/// The stock Volume template from the snapshot: decrypted core 1 blob plus its
/// per-core diff records.
fn template(
    snapshot: &qc_deploy::Snapshot,
    secrets: &keys::DeviceSecrets,
) -> Result<VolumeTemplate, Box<dyn Error>> {
    let root = snapshot.root();
    let core1 = qc_format::crypto::crypt(
        &fs::read(root.join("blobs/core1/volume_blob.bin"))?,
        &secrets.blob,
    );
    let mut diffs = Vec::new();
    for core in 2..=4 {
        let file = DiffFile::parse(&fs::read(
            root.join(format!("blobs/core{core}/diffs.blob")),
        )?)?;
        diffs.push(
            file.get("volume")
                .ok_or("volume has no per-core record")?
                .clone(),
        );
    }
    Ok(VolumeTemplate::new(
        &core1,
        diffs.try_into().expect("three cores"),
    )?)
}

#[derive(Args)]
#[command(group(ArgGroup::new("source").required(true).multiple(false).args(["example", "block"])))]
struct Source {
    #[arg(long, value_enum)]
    example: Option<Example>,
    /// Read a .qcdsp.json graph previously exported by the Rust API.
    #[arg(long)]
    block: Option<PathBuf>,
    #[arg(long, default_value_t = 64)]
    delay_samples: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum Example {
    Gain,
    Biquad,
    Delay,
    SoftClip,
    Tremolo,
    EnvelopeFilter,
    MicroSynth,
    BassMicroSynth,
    Saturator,
}

impl Source {
    fn load(self) -> Result<Block, Box<dyn Error>> {
        if let Some(path) = self.block {
            return Ok(Block::from_json(&fs::read_to_string(path)?)?);
        }
        Ok(match self.example.ok_or("missing DSP source")? {
            Example::Gain => examples::gain()?,
            Example::Biquad => {
                examples::biquad([0.06745527, 0.13491054, 0.06745527, -1.1429805, 0.4128016])?
            }
            Example::Delay => examples::delay(self.delay_samples)?,
            Example::SoftClip => examples::soft_clip()?,
            Example::Tremolo => examples::tremolo()?,
            Example::EnvelopeFilter => examples::envelope_filter()?,
            Example::MicroSynth => examples::micro_synth(examples::Voicing::Guitar)?,
            Example::BassMicroSynth => examples::micro_synth(examples::Voicing::Bass)?,
            Example::Saturator => examples::saturator()?,
        })
    }
}

#[derive(Serialize)]
struct FragmentManifest<'a> {
    format: &'static str,
    installable: bool,
    block: &'a str,
    code_mode: &'static str,
    byte_order: &'static str,
    section: u32,
    address_unit_bytes: u32,
    contract: &'a codegen::Contract,
    required_runtime: &'static str,
}

fn parse_parameter(value: &str) -> Result<(String, f32), String> {
    let (key, value) = value
        .rsplit_once('=')
        .ok_or("use INDEX=VALUE or NAME=VALUE")?;
    let value: f32 = value.parse().map_err(|_| "invalid parameter value")?;
    if !value.is_finite() {
        return Err("parameter value must be finite".into());
    }
    Ok((key.trim().to_owned(), value))
}

fn parameter_index(block: &Block, key: &str) -> Result<usize, Box<dyn Error>> {
    if let Ok(index) = key.parse() {
        return Ok(index);
    }
    block
        .parameters()
        .iter()
        .position(|parameter| parameter.name.eq_ignore_ascii_case(key))
        .ok_or_else(|| format!("{} has no parameter {key:?}", block.name()).into())
}

fn is_wav(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
}

/// Mono samples and their sample rate (48 kHz for JSON).
fn read_samples(path: &Path) -> Result<(Vec<f32>, u32), Box<dyn Error>> {
    if !is_wav(path) {
        return Ok((serde_json::from_slice(&fs::read(path)?)?, 48_000));
    }
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels);
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 2f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|sample| sample as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    Ok((
        samples.into_iter().step_by(channels).collect(),
        spec.sample_rate,
    ))
}

fn write_samples(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), Box<dyn Error>> {
    if !is_wav(path) {
        return write_new(path, &serde_json::to_vec(samples)?);
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(options.open(path)?), spec)?;
    for &sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

fn write_new(path: &Path, data: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    // Cargo passes the subcommand name as argv[1]; direct execution does not.
    let mut arguments: Vec<_> = std::env::args_os().collect();
    if arguments.get(1).is_some_and(|argument| argument == "qcdsp") {
        arguments.remove(1);
    }
    match Cli::parse_from(arguments).command {
        Command::Export { source, output } => {
            write_new(&output, source.load()?.to_json()?.as_bytes())?;
            println!("wrote {}", output.display());
        }
        Command::Build {
            source,
            device: args,
            model_id,
            name,
            output,
        } => {
            let block = source.load()?;
            let (snapshot, secrets) = args.open()?;
            let template = template(&snapshot, &secrets)?;
            let name = name.unwrap_or_else(|| format!("{} (qcdsp)", block.name()));
            let built = device::build(&block, &template, model_id, &name)?;
            let generated = qc_deploy::GeneratedBlock {
                blob_name: built.blob_name.clone(),
                model_id: model_id.to_string(),
                category: built.category.to_string(),
                core1_plain: built.core1.to_bytes()?,
                diffs: built.diffs.clone(),
                model_element: built.model_element.clone(),
                module_element: built.module_element.clone(),
            };
            // Writes the new stage directory; the snapshot is not modified.
            qc_deploy::stage_generated(&snapshot, secrets.as_secrets(), &generated, &output)?;
            let version = qc_deploy::KnownBuild::of(snapshot.firmware_pin())
                .map_or("unknown", |build| build.version);
            println!(
                "Built {name} for CorOS {version}: {} code bytes, {} state bytes, reported CPU {:.3}.\nStage: {}\nInstall with `cargo qcdsp install --stage ...` (dry run by default).",
                built.resources.code_bytes,
                built.resources.state_bytes,
                built.resources.cpu(),
                output.display()
            );
        }
        Command::Install { stage, target } => {
            let (pins, options) = target.options()?;
            let stage = qc_deploy::DeviceStage::open(&stage)?;
            report(qc_deploy::install(
                &mut target.transport()?,
                &stage,
                &pins,
                &options,
            )?);
        }
        Command::Uninstall { target } => {
            let (pins, options) = target.options()?;
            report(qc_deploy::uninstall(
                &mut target.transport()?,
                &pins,
                &options,
            )?);
        }
        Command::FindSeed {
            snapshot,
            serial_file,
            output,
        } => {
            let seed = keys::find_seed(&snapshot, &serial_file)?;
            let line: String = seed.iter().map(|byte| format!("{byte:02x}")).collect();
            write_new(&output, format!("{line}\n").as_bytes())?;
            println!(
                "Found the report seed; wrote {} (mode 0600). Append its line to your key file.",
                output.display()
            );
        }
        Command::Fragment { source, output } => {
            let block = source.load()?;
            let kernel = codegen::compile(&block)?;
            let code = kernel.encode()?;
            let manifest = serde_json::to_vec_pretty(&FragmentManifest {
                format: "qcdsp.scalar-fragment.v2",
                installable: false,
                block: block.blob_name(),
                code_mode: "isa48",
                byte_order: "little_endian",
                section: 3,
                address_unit_bytes: 6,
                contract: &kernel.contract,
                required_runtime: "Run the control steps once per call, then the sample steps per sample, mapping loads, spills and stores onto your memory. Compute steps index kernel.isa48. QC callbacks, memory layout and ISA/VISA transition are not implemented.",
            })?;
            let ir = block.to_json()?;
            if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            fs::create_dir(&output)?;
            write_new(&output.join("kernel.isa48"), &code)?;
            write_new(&output.join("manifest.json"), &manifest)?;
            write_new(&output.join("block.qcdsp.json"), ir.as_bytes())?;
            println!(
                "Built {}: {} instructions, {} bytes, {} registers.\n{}\nNot installable: QC runtime ABI is not yet verified.",
                block.name(),
                kernel.contract.instruction_count,
                code.len(),
                kernel.contract.registers_used,
                output.display()
            );
        }
        Command::ExportIr { source, output } => {
            let block = source.load()?;
            write_new(&output, block.to_json()?.as_bytes())?;
            println!("Exported {} to {}", block.name(), output.display());
        }
        Command::Render {
            source,
            input,
            output,
            compiled,
            parameters,
        } => {
            let block = source.load()?;
            let (mut samples, sample_rate) = read_samples(&input)?;
            if samples.iter().any(|sample| !sample.is_finite()) {
                return Err("input samples must be finite".into());
            }
            if sample_rate != 48_000 {
                eprintln!("qcdsp: input is {sample_rate} Hz; blocks are tuned for the QC's 48 kHz");
            }
            let indices = parameters
                .iter()
                .map(|(key, value)| Ok((parameter_index(&block, key)?, *value)))
                .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
            let mut processor = if compiled {
                Processor::compiled(block)?
            } else {
                Processor::new(block)?
            };
            for (index, value) in indices {
                processor.set_parameter(index, value)?;
            }
            processor.process_in_place(&mut samples);
            if samples.iter().any(|sample| !sample.is_finite()) {
                return Err("DSP output is non-finite; refusing to write it".into());
            }
            write_samples(&output, &samples, sample_rate)?;
            println!(
                "Rendered {} mono samples to {}",
                samples.len(),
                output.display()
            );
        }
        Command::Inspect { blob } => inspect(&blob)?,
        Command::StageClone {
            device: args,
            id,
            name,
            blob,
            output,
        } => {
            let (snapshot, secrets) = args.open()?;
            let prepared = qc_deploy::clone_stock(
                &snapshot,
                secrets.as_secrets(),
                qc_deploy::CloneSpec::volume(id.to_string(), name, blob),
            )?;
            prepared.write_stage(&output)?;
            println!(
                "Staged stock clone at {}. Source unchanged; nothing installed.",
                output.display()
            );
        }
    }
    Ok(())
}

fn inspect(path: &Path) -> Result<(), Box<dyn Error>> {
    // This is structural inspection, not a callback ABI or device fit check.
    let blob = qc_format::blob::ModuleBlob::parse(&fs::read(path)?)?;
    println!("Entry: {:#010x}", blob.entry);
    for contents in &blob.contents {
        println!(
            "Section {} ({:?}): {} bytes, {}-byte address units",
            contents.section.id(),
            contents.section,
            contents.bytes.len(),
            contents.section.unit_bytes()
        );
    }
    for relocations in &blob.relocations {
        println!(
            "Section {} relocations: {} words",
            relocations.section.id(),
            relocations.words.len()
        );
    }
    println!("Structural parse only; runtime ABI and resource fit are not verified.");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("qcdsp: {error}");
            ExitCode::FAILURE
        }
    }
}
