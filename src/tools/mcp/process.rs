use std::sync::Arc;
use std::time::Duration;

use rmcp::service::{RunningService, RunningServiceCancellationToken};
use rmcp::{ClientHandler, RoleClient};
use tokio::process::Child;
use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio_util::sync::CancellationToken;

use super::McpError;

pub(super) struct ServiceOwner {
    stop: CancellationToken,
    done: watch::Receiver<Option<bool>>,
    timeout: Duration,
}

impl ServiceOwner {
    pub fn supervise<H: ClientHandler>(
        service: RunningService<RoleClient, H>,
        stop: CancellationToken,
        timeout: Duration,
    ) -> Self {
        let mut guard = ServiceCancellation(Some(service.cancellation_token()));
        let shutdown = stop.clone();
        let (sender, done) = watch::channel(None);
        tokio::spawn(async move {
            let waiter = service.waiting();
            tokio::pin!(waiter);
            let clean = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    guard.cancel();
                    matches!(tokio::time::timeout(timeout, &mut waiter).await, Ok(Ok(_)))
                }
                result = &mut waiter => result.is_ok(),
            };
            shutdown.cancel();
            sender.send_replace(Some(clean));
        });
        Self {
            stop,
            done,
            timeout,
        }
    }

    pub async fn shutdown(&self) -> Result<(), McpError> {
        self.stop.cancel();
        await_cleanup(self.done.clone(), self.timeout).await
    }
}

impl Drop for ServiceOwner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct ServiceCancellation(Option<RunningServiceCancellationToken>);
impl ServiceCancellation {
    fn cancel(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}
impl Drop for ServiceCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(super) struct ProcessOwner {
    pub stop: CancellationToken,
    pub pid: u32,
    done: watch::Receiver<Option<bool>>,
    timeout: Duration,
}

impl ProcessOwner {
    pub fn supervise(
        child: Child,
        permit: OwnedSemaphorePermit,
        timeout: Duration,
    ) -> Result<
        (
            Arc<Self>,
            tokio::process::ChildStdout,
            tokio::process::ChildStdin,
        ),
        McpError,
    > {
        let pid = child.id().ok_or(McpError::Process)?;
        let mut owned = ManagedProcess {
            group: ProcessGroup { pid, armed: true },
            child,
            admission: ProcessAdmission(Some(permit)),
        };
        let stdout = owned.child.stdout.take().ok_or(McpError::Cleanup)?;
        let stdin = owned.child.stdin.take().ok_or(McpError::Cleanup)?;
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let (sender, done) = watch::channel(None);
        tokio::spawn(async move {
            let observed = tokio::select! {
                biased;
                _ = shutdown.cancelled() => Ok(()),
                result = observe_exit(pid) => result,
            };
            let group_stopped = owned.group.kill();
            let deadline = tokio::time::Instant::now() + timeout;
            let reaped = tokio::time::timeout_at(deadline, owned.child.wait()).await;
            let absent = group_absent(pid, deadline).await;
            let clean = observed.is_ok() && group_stopped && matches!(reaped, Ok(Ok(_))) && absent;
            if clean {
                owned.admission.release();
            }
            shutdown.cancel();
            sender.send_replace(Some(clean));
        });
        Ok((
            Arc::new(Self {
                stop,
                pid,
                done,
                timeout,
            }),
            stdout,
            stdin,
        ))
    }

    pub async fn shutdown(&self) -> Result<(), McpError> {
        self.stop.cancel();
        await_cleanup(self.done.clone(), self.timeout).await
    }
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct ManagedProcess {
    group: ProcessGroup,
    child: Child,
    admission: ProcessAdmission,
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if !self.group.kill() {
            tracing::error!("MCP process cleanup lost its child identity anchor");
        }
    }
}

async fn observe_exit(pid: u32) -> Result<(), McpError> {
    #[cfg(unix)]
    {
        use rustix::process::{WaitId, WaitIdOptions};
        let pid = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or(McpError::Cleanup)?;
        let mut exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            .map_err(|_| McpError::Cleanup)?;
        loop {
            match rustix::process::waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            ) {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return Err(McpError::Cleanup),
            }
            exits.recv().await.ok_or(McpError::Cleanup)?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(McpError::UnsupportedPlatform)
    }
}

async fn group_absent(pid: u32, deadline: tokio::time::Instant) -> bool {
    #[cfg(unix)]
    {
        let Some(pid) = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return false;
        };
        loop {
            match rustix::process::test_kill_process_group(pid) {
                Err(rustix::io::Errno::SRCH) => return true,
                Ok(()) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                _ => return false,
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, deadline);
        false
    }
}

struct ProcessAdmission(Option<OwnedSemaphorePermit>);
impl ProcessAdmission {
    fn release(&mut self) {
        self.0.take();
    }
}
impl Drop for ProcessAdmission {
    fn drop(&mut self) {
        if let Some(permit) = self.0.take() {
            permit.forget();
        }
    }
}

struct ProcessGroup {
    pid: u32,
    armed: bool,
}

impl ProcessGroup {
    fn kill(&mut self) -> bool {
        if !self.armed {
            return true;
        }
        self.armed = false;
        #[cfg(unix)]
        {
            use rustix::process::{Signal, WaitId, WaitIdOptions};
            let Some(pid) = i32::try_from(self.pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            else {
                return false;
            };
            if rustix::process::waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            )
            .is_err()
            {
                return false;
            }
            let group = rustix::process::kill_process_group(pid, Signal::KILL);
            let leader = rustix::process::kill_process(pid, Signal::KILL);
            [group, leader]
                .into_iter()
                .all(|result| matches!(result, Ok(()) | Err(rustix::io::Errno::SRCH)))
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.kill() {
            tracing::error!("MCP process group cleanup could not be confirmed");
        }
    }
}

async fn await_cleanup(
    mut done: watch::Receiver<Option<bool>>,
    timeout: Duration,
) -> Result<(), McpError> {
    let wait = async {
        loop {
            if let Some(clean) = *done.borrow_and_update() {
                return if clean {
                    Ok(())
                } else {
                    Err(McpError::Cleanup)
                };
            }
            done.changed().await.map_err(|_| McpError::Cleanup)?;
        }
    };
    tokio::time::timeout(timeout + Duration::from_millis(100), wait)
        .await
        .map_err(|_| McpError::Cleanup)?
}
