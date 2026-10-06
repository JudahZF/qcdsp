use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{Change, Error, Result, fs, sha256};

pub(crate) const ROOT: &str = "/opt/neuraldsp";
const SERIAL: &str = "/etc/qc_sn";
const CHUNK_BYTES: usize = 128 * 1024;

/// Typed operations let test transports use the same plan as SSH. Paths and shell
/// arguments are validated and quoted before any command is handed to a runner.
#[derive(Clone, Debug)]
pub enum RemoteOperation {
    Check {
        path: String,
        hash: Option<String>,
        mode: Option<u32>,
    },
    Read {
        path: String,
    },
    List {
        paths: Vec<String>,
    },
    CreateStage {
        token: String,
    },
    Upload {
        path: String,
        bytes: Vec<u8>,
    },
    Backup {
        from: String,
        to: String,
        hash: String,
        mode: u32,
    },
    Publish {
        from: String,
        to: String,
        change: Change,
    },
    CheckRestorable {
        path: String,
        change: Change,
    },
    Restore {
        backup: String,
        temporary: String,
        target: String,
        change: Change,
    },
    Sync,
}

/// No method on a transport is called by a dry run except `describe`.
pub trait Transport {
    fn execute(&mut self, operation: &RemoteOperation) -> Result<Vec<u8>>;
    fn describe(&self, operation: &RemoteOperation) -> Result<Vec<String>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRequest {
    pub program: String,
    pub arguments: Vec<String>,
    pub stdin: Vec<u8>,
}

impl CommandRequest {
    pub fn display(&self) -> String {
        let mut command = std::iter::once(&self.program)
            .chain(self.arguments.iter())
            .map(|s| quote(s))
            .collect::<Vec<_>>()
            .join(" ");
        if !self.stdin.is_empty() {
            command.push_str(&format!(
                " # stdin: {} bytes, sha256={}",
                self.stdin.len(),
                sha256(&self.stdin)
            ));
        }
        command
    }
}

pub trait CommandRunner {
    fn run(&mut self, request: &CommandRequest) -> Result<Vec<u8>>;
}

#[derive(Debug, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&mut self, request: &CommandRequest) -> Result<Vec<u8>> {
        run_process(&request.program, &request.arguments, &request.stdin)
    }
}

fn run_process(program: &str, arguments: &[String], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| Error::Transport("missing command stdin".into()))?
        .write_all(input);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::Transport(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    written?;
    Ok(output.stdout)
}

/// Runs each step as one short `ssh` command. Without a proxy command, `ssh`
/// uses the caller's own SSH config for the host. Construction and command
/// rendering do not start SSH. Host keys remain checked.
pub struct SshTransport<R = SystemCommandRunner> {
    host: String,
    proxy_command: Option<String>,
    runner: R,
}

impl SshTransport<SystemCommandRunner> {
    pub fn new(host: impl Into<String>) -> Result<Self> {
        Self::with_runner(host, SystemCommandRunner)
    }
}

impl<R: CommandRunner> SshTransport<R> {
    pub fn with_runner(host: impl Into<String>, runner: R) -> Result<Self> {
        let host = host.into();
        if host.is_empty()
            || host.starts_with('-')
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"@._:-".contains(&c))
        {
            return Err(Error::Invalid("unsafe SSH host".into()));
        }
        Ok(Self {
            host,
            proxy_command: None,
            runner,
        })
    }

    /// Pass `-o ProxyCommand=<command>`, e.g. the byte-at-a-time banner proxy
    /// that the QC's OpenSSH 6.7 needs on some networks.
    pub fn with_proxy_command(mut self, command: impl Into<String>) -> Result<Self> {
        let command = command.into();
        if command.trim().is_empty() || command.contains(['\n', '\r', '\0']) {
            return Err(Error::Invalid("unsafe SSH proxy command".into()));
        }
        self.proxy_command = Some(command);
        Ok(self)
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }
    pub fn runner_mut(&mut self) -> &mut R {
        &mut self.runner
    }

    fn requests(&self, operation: &RemoteOperation) -> Result<Vec<CommandRequest>> {
        scripts(operation)?
            .into_iter()
            .map(|(script, stdin)| {
                let proxy = self
                    .proxy_command
                    .iter()
                    .flat_map(|command| ["-o".to_owned(), format!("ProxyCommand={command}")]);
                let arguments = proxy
                    .chain(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"].map(String::from))
                    .chain([self.host.clone(), script])
                    .collect();
                Ok(CommandRequest {
                    program: "ssh".into(),
                    arguments,
                    stdin,
                })
            })
            .collect()
    }
}

impl<R: CommandRunner> Transport for SshTransport<R> {
    fn execute(&mut self, operation: &RemoteOperation) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        for request in self.requests(operation)? {
            output.extend(self.runner.run(&request)?);
        }
        Ok(output)
    }

    fn describe(&self, operation: &RemoteOperation) -> Result<Vec<String>> {
        Ok(self
            .requests(operation)?
            .iter()
            .map(CommandRequest::display)
            .collect())
    }
}

pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub(crate) fn token_valid(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

pub(crate) fn remote_stage(token: &str) -> String {
    format!("{ROOT}/.qc-block-{token}")
}

fn validate_path(path: &str) -> Result<()> {
    if path == SERIAL {
        return Ok(());
    }
    let relative = path
        .strip_prefix(&format!("{ROOT}/"))
        .ok_or_else(|| Error::Invalid(format!("unsafe remote path: {path}")))?;
    if relative.split('/').any(|part| {
        part.is_empty()
            || part == "."
            || part == ".."
            || !part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    }) {
        return Err(Error::Invalid(format!("unsafe remote path: {path}")));
    }
    Ok(())
}

// Check ancestors too: a symlinked parent could redirect a rename or upload.
fn safe(path: &str) -> Result<String> {
    validate_path(path)?;
    let mut ancestors = Vec::new();
    let mut current = Path::new(path);
    while current != Path::new("/") {
        ancestors.push(format!("[ ! -L {} ]", quote(&current.to_string_lossy())));
        current = current
            .parent()
            .ok_or_else(|| Error::Invalid("remote parent".into()))?;
    }
    Ok(ancestors.join(" && "))
}

fn check(path: &str, hash: Option<&str>, mode: Option<u32>) -> Result<String> {
    let safety = safe(path)?;
    let p = quote(path);
    let Some(hash) = hash else {
        return Ok(format!("{safety} && [ ! -e {p} ]"));
    };
    if !fs::valid_hash(hash) || mode.is_some_and(|m| m > 0o777) {
        return Err(Error::Invalid("invalid remote hash or permissions".into()));
    }
    let mut script = format!(
        "{safety} && [ -f {p} ] && [ \"$(stat -c %h {p})\" = 1 ] && h=$(openssl dgst -sha256 {p}) && [ \"${{h##* }}\" = {} ]",
        quote(hash)
    );
    if let Some(mode) = mode {
        script.push_str(&format!(" && [ \"$(stat -c %a {p})\" = {mode:o} ]"));
    }
    Ok(script)
}

fn stage_path(path: &str) -> Result<()> {
    validate_path(path)?;
    let relative = path
        .strip_prefix(&format!("{ROOT}/.qc-block-"))
        .ok_or_else(|| Error::Invalid("writes must be staged".into()))?;
    let token = relative.split('/').next().unwrap_or_default();
    if !token_valid(token) {
        return Err(Error::Invalid("invalid remote token".into()));
    }
    Ok(())
}

fn scripts(operation: &RemoteOperation) -> Result<Vec<(String, Vec<u8>)>> {
    let script = match operation {
        RemoteOperation::Check { path, hash, mode } => check(path, hash.as_deref(), *mode)?,
        RemoteOperation::Read { path } => format!(
            "{} && [ -f {} ] && cat {}",
            safe(path)?,
            quote(path),
            quote(path)
        ),
        RemoteOperation::List { paths } => {
            let mut items = Vec::new();
            for path in paths {
                items.push(format!(
                    "{} && if [ -e {} ]; then ls -ld {}; else printf 'absent: %s\\n' {}; fi",
                    safe(path)?,
                    quote(path),
                    quote(path),
                    quote(path)
                ));
            }
            items.join(" && ")
        }
        RemoteOperation::CreateStage { token } => {
            if !token_valid(token) {
                return Err(Error::Invalid("invalid stage token".into()));
            }
            let stage = remote_stage(token);
            let mut dirs = vec![stage.clone()];
            for prefix in ["files", "before", "restore"] {
                dirs.push(format!("{stage}/{prefix}"));
                dirs.push(format!("{stage}/{prefix}/blobs"));
                for core in 1..=4 {
                    dirs.push(format!("{stage}/{prefix}/blobs/core{core}"));
                }
            }
            format!(
                "{} && umask 077 && mkdir {} && mkdir {}",
                safe(&stage)?,
                quote(&stage),
                dirs[1..]
                    .iter()
                    .map(|d| quote(d))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        RemoteOperation::Upload { path, bytes } => {
            stage_path(path)?;
            let safety = safe(path)?;
            let mut requests = Vec::new();
            // A killed upload only damages a private staged file. Bounded chunks
            // keep each cat short even on the slow Wi-Fi link.
            for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                let guard = if index == 0 {
                    format!("[ ! -e {} ]", quote(path))
                } else {
                    format!(
                        "[ -f {} ] && [ \"$(stat -c %s {})\" = {} ]",
                        quote(path),
                        quote(path),
                        index * CHUNK_BYTES
                    )
                };
                requests.push((
                    format!(
                        "{safety} && {guard} && umask 077 && cat {} {}",
                        if index == 0 { ">" } else { ">>" },
                        quote(path)
                    ),
                    chunk.to_vec(),
                ));
            }
            if bytes.is_empty() {
                requests.push((
                    format!(
                        "{safety} && [ ! -e {} ] && umask 077 && cat > {}",
                        quote(path),
                        quote(path)
                    ),
                    Vec::new(),
                ));
            }
            return Ok(requests);
        }
        RemoteOperation::Backup {
            from,
            to,
            hash,
            mode,
        } => {
            stage_path(to)?;
            format!(
                "{} && {} && cp -p {} {} && {} && sync",
                check(from, Some(hash), Some(*mode))?,
                check(to, None, None)?,
                quote(from),
                quote(to),
                check(to, Some(hash), Some(*mode))?
            )
        }
        RemoteOperation::Publish { from, to, change } => {
            stage_path(from)?;
            format!(
                "{} && chmod {:o} {} && {} && mv {} {} && {} && sync",
                check(from, Some(&change.after_sha256), None)?,
                change.mode,
                quote(from),
                check(
                    to,
                    change.before_sha256.as_deref(),
                    change.before_sha256.as_ref().map(|_| change.mode)
                )?,
                quote(from),
                quote(to),
                check(to, Some(&change.after_sha256), Some(change.mode))?
            )
        }
        RemoteOperation::CheckRestorable { path, change } => restorable(path, change)?,
        RemoteOperation::Restore {
            backup,
            temporary,
            target,
            change,
        } => {
            stage_path(backup)?;
            stage_path(temporary)?;
            let before = check(
                target,
                change.before_sha256.as_deref(),
                change.before_sha256.as_ref().map(|_| change.mode),
            )?;
            let after = check(target, Some(&change.after_sha256), Some(change.mode))?;
            if let Some(hash) = &change.before_sha256 {
                let temporary_guard = format!(
                    "{} && {{ [ ! -e {} ] || {{ [ -f {} ] && [ \"$(stat -c %h {})\" = 1 ]; }}; }}",
                    safe(temporary)?,
                    quote(temporary),
                    quote(temporary),
                    quote(temporary)
                );
                format!(
                    "if {before}; then :; else {} && {temporary_guard} && cp -p {} {} && {} && {after} && mv {} {} && {before} && sync; fi",
                    check(backup, Some(hash), Some(change.mode))?,
                    quote(backup),
                    quote(temporary),
                    check(temporary, Some(hash), Some(change.mode))?,
                    quote(temporary),
                    quote(target)
                )
            } else {
                format!(
                    "if {before}; then :; else {after} && rm {} && {before} && sync; fi",
                    quote(target)
                )
            }
        }
        RemoteOperation::Sync => "sync".into(),
    };
    Ok(vec![(script, Vec::new())])
}

fn restorable(path: &str, change: &Change) -> Result<String> {
    Ok(format!(
        "{{ {}; }} || {{ {}; }}",
        check(
            path,
            change.before_sha256.as_deref(),
            change.before_sha256.as_ref().map(|_| change.mode)
        )?,
        check(path, Some(&change.after_sha256), Some(change.mode))?
    ))
}

/// Local fake runner. It never invokes SSH/SCP: it rewrites the two fixed device
/// paths into a checked private tree and runs the rendered POSIX shell locally.
/// This tests the actual shell commands, not a separate model of their behavior.
pub struct FakeCommandRunner {
    root: PathBuf,
    pub commands: Vec<CommandRequest>,
    /// Zero-based command index. Fails once before running that command.
    pub fail_at: Option<usize>,
}

impl FakeCommandRunner {
    pub fn new(local_fake_device_root: impl AsRef<Path>) -> Result<Self> {
        let root = fs::root(local_fake_device_root.as_ref())?;
        fs::root(&fs::checked_path(&root, "opt/neuraldsp")?)?;
        fs::checked_path(&root, "etc/qc_sn")?;
        Ok(Self {
            root,
            commands: Vec::new(),
            fail_at: None,
        })
    }
}

impl CommandRunner for FakeCommandRunner {
    fn run(&mut self, request: &CommandRequest) -> Result<Vec<u8>> {
        if request.program != "ssh" || request.arguments.len() < 6 {
            return Err(Error::Invalid(
                "fake runner only accepts rendered SSH requests".into(),
            ));
        }
        let index = self.commands.len();
        self.commands.push(request.clone());
        if self.fail_at == Some(index) {
            self.fail_at = None;
            return Err(Error::Transport("injected fake command failure".into()));
        }
        // First remove recognized quoted paths. Reject any remaining production
        // reference BEFORE inserting the local paths, rather than checking for
        // the presence of just one correctly mapped occurrence.
        let script = request.arguments[request.arguments.len() - 1]
            .replace("'/opt/neuraldsp", "'__QC_LOCAL_NEURALDSP__")
            .replace("'/etc/qc_sn", "'__QC_LOCAL_SERIAL__")
            .replace("'/opt'", "'__QC_LOCAL_OPT__'")
            .replace("'/etc'", "'__QC_LOCAL_ETC__'");
        if script.contains("/opt/neuraldsp") || script.contains("/etc/qc_sn") {
            return Err(Error::Invalid("unmapped fake device path".into()));
        }
        let prefix = quote(&self.root.to_string_lossy());
        let prefix = prefix.strip_suffix('\'').unwrap_or(&prefix);
        let script = script
            .replace(
                "'__QC_LOCAL_NEURALDSP__",
                &format!("{prefix}/opt/neuraldsp"),
            )
            .replace("'__QC_LOCAL_SERIAL__", &format!("{prefix}/etc/qc_sn"))
            .replace("'__QC_LOCAL_OPT__", &format!("{prefix}/opt"))
            .replace("'__QC_LOCAL_ETC__", &format!("{prefix}/etc"));
        run_process("sh", &["-c".into(), script], &request.stdin)
    }
}

pub type FakeTransport = SshTransport<FakeCommandRunner>;
