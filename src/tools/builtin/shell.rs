use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::{Instant, sleep, timeout};

use crate::tools::{
    Tool, ToolCapability, ToolConcurrency, ToolDescriptor, ToolError, ToolExecutionContext,
    ToolOutput,
};

use super::{Redactor, no_effect, output};

const CAPTURE_BYTES: usize = 4096;
const CLEANUP_WAIT: Duration = Duration::from_millis(500);

pub(super) struct ShellTool {
    descriptor: ToolDescriptor,
    redactor: Redactor,
}

impl ShellTool {
    pub(super) fn new(redactor: Redactor) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "shell".into(),
                description: "Execute explicitly approved trusted FullHost shell code using /bin/sh; this is unrestricted host execution, not a filesystem sandbox".into(),
                input_schema: json!({"type":"object","properties":{"command":{"type":"string","minLength":1,"maxLength":16384},"timeout_ms":{"type":"integer","minimum":1,"maximum":300000}},"required":["command"],"additionalProperties":false}),
                capabilities: vec![ToolCapability::Shell],
                read_only: false,
                concurrency: ToolConcurrency::Exclusive,
            },
            redactor,
        }
    }
}

fn unknown(redactor: &Redactor, message: impl std::fmt::Display) -> ToolError {
    ToolError::EffectsUnknown {
        name: "shell".into(),
        message: redactor.error(message),
        cleanup_completed: false,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    command: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

struct ProcessGroup {
    pid: Pid,
    armed: bool,
}

impl ProcessGroup {
    fn kill(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        let exited = rustix::process::waitid(
            WaitId::Pid(self.pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )?
        .is_some();
        let group = rustix::process::kill_process_group(self.pid, Signal::KILL);
        let leader = rustix::process::kill_process(self.pid, Signal::KILL);
        for result in [group, leader] {
            match result {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(rustix::io::Errno::PERM) if exited => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.kill();
        }
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    total_bytes: u64,
}

#[async_trait]
impl Tool for ShellTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let redactor = self.redactor.for_task(context);
        redactor.finish(run(context, arguments, &redactor).await)
    }
}

async fn run(
    context: &ToolExecutionContext,
    arguments: Value,
    redactor: &Redactor,
) -> Result<ToolOutput, ToolError> {
    {
        let arguments: Arguments = serde_json::from_value(arguments)
            .map_err(|error| no_effect("shell", redactor.error(error)))?;
        if arguments.command.is_empty()
            || arguments.command.len() > 65_536
            || arguments.command.contains('\0')
            || !(1..=300_000).contains(&arguments.timeout_ms)
        {
            return Err(no_effect("shell", "invalid command or timeout"));
        }
        if context.cancellation().is_cancelled() {
            return Err(no_effect("shell", "shell cancelled before spawning"));
        }
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&arguments.command)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C")
            .env("LC_ALL", "C");
        for (name, value) in context
            .vault()
            .map_or(&[][..], |vault| &vault.environment[..])
        {
            command.env(name, std::ffi::OsStr::from_bytes(value.expose()));
        }
        let mut child = command
            .current_dir(context.workspace_root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(false)
            .spawn()
            .map_err(|error| no_effect("shell", redactor.error(error)))?;
        let pid = match child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw)
        {
            Some(pid) => pid,
            None => {
                let killed = child.start_kill();
                let waited = timeout(CLEANUP_WAIT, child.wait()).await;
                return Err(unknown(
                    redactor,
                    format!(
                        "spawned shell did not expose a valid process identifier; leader kill: {killed:?}; wait: {waited:?}"
                    ),
                ));
            }
        };
        let mut group = ProcessGroup { pid, armed: true };
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| unknown(redactor, "spawned shell has no stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| unknown(redactor, "spawned shell has no stderr pipe"))?;
        let readers = async { tokio::join!(capture(stdout), capture(stderr)) };
        tokio::pin!(readers);
        let mut captured = None;
        let deadline = Instant::now() + Duration::from_millis(arguments.timeout_ms);
        let mut interrupted = None;
        loop {
            tokio::select! {
                biased;
                _ = context.cancellation().cancelled() => {
                    interrupted = Some("shell cancellation requested; effects may have occurred");
                    break;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    interrupted = Some("shell timeout; effects may have occurred");
                    break;
                }
                result = &mut readers, if captured.is_none() => captured = Some(result),
                status = observe_exit(pid) => {
                    if let Err(error) = status {
                        if error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) {
                            group.armed = false;
                            return Err(unknown(redactor, "shell child was reaped outside its owner; process identity is no longer anchored and no signal was sent"));
                        }
                        let kill = group.kill();
                        let waited = timeout(CLEANUP_WAIT, child.wait()).await;
                        if captured.is_none() {
                            let _ = timeout(CLEANUP_WAIT, &mut readers).await;
                        }
                        return Err(unknown(redactor, format!("{error}; process-group cleanup: {kill:?}; leader wait: {waited:?}")));
                    }
                    break;
                }
            }
        }
        let killed = group.kill();
        let status = timeout(CLEANUP_WAIT, child.wait()).await;
        let streams = if let Some(captured) = captured {
            Ok(captured)
        } else {
            timeout(CLEANUP_WAIT, &mut readers).await
        };
        let group_absent = group_absent(pid).await;
        if let Some(reason) = interrupted {
            return Err(unknown(
                redactor,
                format!(
                    "{reason}; group signal delivered: {}; leader reaped: {}; group absent: {}; pipes closed: {}. Process-group escape and remote activity cannot be excluded",
                    killed.is_ok(),
                    matches!(status, Ok(Ok(_))),
                    group_absent,
                    streams.is_ok()
                ),
            ));
        }
        if killed.is_err() || !group_absent {
            return Err(unknown(
                redactor,
                "shell cleanup could not establish that its process group stopped",
            ));
        }
        let status = status
            .map_err(|_| {
                unknown(
                    redactor,
                    "shell leader did not finish cleanup before its deadline",
                )
            })?
            .map_err(|error| unknown(redactor, error))?;
        let (stdout, stderr) = streams.map_err(|_| {
            unknown(
                redactor,
                "shell output pipes remained open after process-group cleanup",
            )
        })?;
        let stdout = stdout.map_err(|error| unknown(redactor, error))?;
        let stderr = stderr.map_err(|error| unknown(redactor, error))?;
        let stdout_text = redactor.range(
            &stdout.bytes,
            0..stdout.bytes.len(),
            CAPTURE_BYTES,
            stdout.total_bytes > stdout.bytes.len() as u64,
        );
        let stderr_text = redactor.range(
            &stderr.bytes,
            0..stderr.bytes.len(),
            CAPTURE_BYTES,
            stderr.total_bytes > stderr.bytes.len() as u64,
        );
        output(
            "shell",
            json!({
                "stdout":stdout_text.text,
                "stderr":stderr_text.text,
                "stdout_bytes":stdout.total_bytes,
                "stderr_bytes":stderr.total_bytes,
                "stdout_truncated":stdout.total_bytes>stdout.bytes.len() as u64 || stdout_text.truncated,
                "stderr_truncated":stderr.total_bytes>stderr.bytes.len() as u64 || stderr_text.truncated,
                "stdout_utf8_lossy":std::str::from_utf8(&stdout.bytes).is_err(),
                "stderr_utf8_lossy":std::str::from_utf8(&stderr.bytes).is_err(),
                "redacted":stdout_text.redacted || stderr_text.redacted,
                "exit_code":status.code(),
                "signal":status.signal(),
                "process_group_stopped":true,
                "sandboxed":false
            }),
            !status.success(),
        )
    }
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> io::Result<Capture> {
    let mut capture = Capture::default();
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(capture);
        }
        capture.total_bytes = capture.total_bytes.saturating_add(count as u64);
        let keep = count.min(CAPTURE_BYTES.saturating_sub(capture.bytes.len()));
        capture.bytes.extend_from_slice(&buffer[..keep]);
    }
}

async fn observe_exit(pid: Pid) -> io::Result<()> {
    loop {
        match rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        ) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) | Err(rustix::io::Errno::INTR) => sleep(Duration::from_millis(10)).await,
            Err(error) => return Err(error.into()),
        }
    }
}

async fn group_absent(pid: Pid) -> bool {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match rustix::process::test_kill_process_group(pid) {
            Err(rustix::io::Errno::SRCH) => return true,
            Ok(()) if Instant::now() < deadline => sleep(Duration::from_millis(10)).await,
            _ => return false,
        }
    }
}
