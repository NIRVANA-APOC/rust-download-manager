//! Locating and launching `rdm-gui.exe` from the native messaging host.
//!
//! The host is a short-lived child of Firefox (which puts it in a Job object
//! and kills the whole job when the extension disconnects — see MDN "Native
//! messaging"). The GUI must outlive that bridge, so it is spawned as a
//! detached, job-breaking process. On Windows the first attempt requests
//! `CREATE_BREAKAWAY_FROM_JOB`; some job objects forbid breakaway, in which
//! case we degrade to a detached process and log the caveat.
//!
//! A GUI is **never** launched when `ipc.json` already answers `ping`
//! (requirement from the Lead). If spawning succeeds but startup fails within
//! 8 seconds, the child is killed and reaped so no zombie/orphan build-up is
//! left behind.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::ipc_client;
use crate::logger::Logger;

/// Maximum time to wait for a freshly spawned GUI to publish `ipc.json`.
pub const GUI_START_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[cfg(windows)]
pub const GUI_BINARY_NAME: &str = "rdm-gui.exe";
#[cfg(not(windows))]
pub const GUI_BINARY_NAME: &str = "rdm-gui";

/// Result of "make sure a GUI is available".
#[derive(Debug, Clone)]
pub struct GuiStatus {
    /// A GUI answered the IPC ping.
    pub running: bool,
    /// This host process started the GUI during this call.
    pub launched: bool,
    /// Machine-readable reason when `running == false` (logged, not sent as
    /// an error code; the protocol only exposes `gui_unavailable`).
    pub reason: Option<String>,
}

impl GuiStatus {
    fn available(launched: bool) -> Self {
        Self {
            running: true,
            launched,
            reason: None,
        }
    }

    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            running: false,
            launched: false,
            reason: Some(reason.into()),
        }
    }
}

fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn env_override() -> Option<PathBuf> {
    std::env::var_os("RDM_GUI_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Candidate GUI locations, most specific first.
///
/// Pure function (no env/FS access) so the ordering is unit-testable. When
/// `RDM_GUI_PATH` is set it is authoritative and nothing else is tried.
pub fn gui_candidates(
    env_override: Option<&Path>,
    exe_dir: Option<&Path>,
    state_dir: &Path,
) -> Vec<PathBuf> {
    if let Some(path) = env_override {
        return vec![path.to_path_buf()];
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(dir.join(GUI_BINARY_NAME));
        if let Some(parent) = dir.parent() {
            candidates.push(parent.join(GUI_BINARY_NAME));
        }
    }
    candidates.push(state_dir.join(GUI_BINARY_NAME));
    let mut deduped: Vec<PathBuf> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !deduped.contains(&candidate) {
            deduped.push(candidate);
        }
    }
    deduped
}

/// Ensure a GUI is reachable, optionally launching one.
pub fn ensure_gui(launch: bool, logger: &Logger, state_dir: &Path) -> GuiStatus {
    if ipc_client::ping().is_ok() {
        return GuiStatus::available(false);
    }
    if !launch {
        logger.info("GUI offline and launch_gui=false; not starting it");
        return GuiStatus::unavailable("gui_not_running");
    }

    let override_path = env_override();
    let exe_dir = executable_dir();
    let candidates = gui_candidates(override_path.as_deref(), exe_dir.as_deref(), state_dir);
    let Some(path) = candidates.iter().find(|candidate| candidate.is_file()) else {
        logger.warn(&format!(
            "GUI binary not found; tried: {}",
            candidates
                .iter()
                .map(|candidate| candidate.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return GuiStatus::unavailable("gui_binary_not_found");
    };

    let mut child = match spawn_gui(path) {
        Ok(child) => child,
        Err(err) => {
            logger.error(&format!(
                "failed to launch GUI {}: {err}",
                path.display()
            ));
            return GuiStatus::unavailable("gui_spawn_failed");
        }
    };
    logger.info(&format!(
        "spawned RDM GUI pid={} path={}",
        child.id(),
        path.display()
    ));

    let mut child_slot = Some(child);
    if wait_until_ready(&mut child_slot, GUI_START_TIMEOUT) {
        // Drop the handle without waiting: the GUI must keep running. With
        // CREATE_BREAKAWAY_FROM_JOB it survives Firefox killing our job.
        if let Some(child) = child_slot {
            logger.info(&format!("RDM GUI ready (pid={})", child.id()));
        }
        return GuiStatus::available(true);
    }

    // Startup failed: reclaim the child process to avoid orphan processes.
    if let Some(mut child) = child_slot {
        match child.try_wait() {
            Ok(Some(status)) => {
                logger.error(&format!("GUI exited during startup: {status}"));
            }
            _ => {
                logger.error("GUI did not publish ipc.json in time; terminating it");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    GuiStatus::unavailable("gui_start_timeout")
}

/// Poll the IPC endpoint until the GUI answers a `ping`, the child exits, or
/// the deadline expires.
fn wait_until_ready(child: &mut Option<Child>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        // Cheap liveness first (300 ms connect) so a stale endpoint cannot eat
        // the whole 8 s budget in a single 5 s IPC read timeout.
        if rdm_ipc::is_gui_alive() && ipc_client::ping().is_ok() {
            return true;
        }
        if let Some(child) = child.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return false;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline - now));
    }
}

/// Spawn the GUI detached from our console and (when permitted) from
/// Firefox's Job object.
pub fn spawn_gui(path: &Path) -> std::io::Result<Child> {
    let mut command = Command::new(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

        // Preferred: detached + breakaway so the GUI survives the native host
        // being killed with Firefox's job.
        command.creation_flags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        );
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(_) => {
                // Job objects created without JOB_OBJECT_LIMIT_BREAKAWAY_OK
                // reject the breakaway flag; detached is still worth a try.
                command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
                match command.spawn() {
                    Ok(child) => return Ok(child),
                    Err(_) => {
                        // Last resort: plain spawn (same job, may be killed
                        // together with this host).
                        command.creation_flags(0);
                    }
                }
            }
        }
    }

    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_is_authoritative() {
        let candidates = gui_candidates(
            Some(Path::new(r"D:\Custom\rdm-gui.exe")),
            Some(Path::new(r"C:\app")),
            Path::new(r"C:\Users\me\AppData\Local\RDM"),
        );
        assert_eq!(candidates, vec![PathBuf::from(r"D:\Custom\rdm-gui.exe")]);
    }

    #[test]
    fn candidates_prefer_sibling_then_parent_then_state_dir() {
        let candidates = gui_candidates(
            None,
            Some(Path::new(r"C:\app\bin")),
            Path::new(r"C:\state\RDM"),
        );
        assert_eq!(
            candidates[0],
            Path::new(r"C:\app\bin").join(GUI_BINARY_NAME)
        );
        assert_eq!(candidates[1], Path::new(r"C:\app").join(GUI_BINARY_NAME));
        assert_eq!(
            candidates[2],
            Path::new(r"C:\state\RDM").join(GUI_BINARY_NAME)
        );
    }

    #[test]
    fn candidates_deduplicate_state_dir() {
        let dir = Path::new(r"C:\app");
        let candidates = gui_candidates(None, Some(dir), dir);
        let distinct: std::collections::BTreeSet<_> = candidates.iter().collect();
        assert_eq!(candidates.len(), distinct.len());
    }

    #[test]
    fn no_candidates_without_exe_dir_still_uses_state_dir() {
        let candidates = gui_candidates(None, None, Path::new(r"C:\state\RDM"));
        assert_eq!(
            candidates,
            vec![Path::new(r"C:\state\RDM").join(GUI_BINARY_NAME)]
        );
    }

    #[cfg(windows)]
    #[test]
    fn wait_until_ready_gives_up_when_child_exits() {
        // `cmd /C exit` terminates immediately; the helper must notice the
        // exit instead of sleeping for the full timeout.
        let child = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        let mut slot = Some(child);
        let started = Instant::now();
        assert!(!wait_until_ready(&mut slot, Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(2));
        if let Some(mut child) = slot {
            let _ = child.wait();
        }
    }
}
