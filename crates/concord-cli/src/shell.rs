//! Bounded shell execution with process-group cleanup on timeout.
//!
//! Concord runs user- and agent-supplied test/check commands (`check_cmd`,
//! `test_cmd`, peer checks, post-merge repair checks). An agent's generated
//! code can contain an infinite loop; without a timeout and process-group kill,
//! a single stuck test process stalls the entire run.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default timeout for any acceptance-check or test command (2 minutes).
/// Override with `CONCORD_CHECK_TIMEOUT_SECS` for slow test suites.
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 120;

/// Configured timeout for check/test commands.
pub fn check_timeout() -> Duration {
    std::env::var("CONCORD_CHECK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_CHECK_TIMEOUT_SECS))
}

/// Outcome of a bounded shell command.
#[derive(Clone, Debug)]
pub struct ShellOutput {
    pub ok: bool,
    pub timed_out: bool,
    pub output: String,
}

/// Terminate an entire process group on Unix (SIGTERM, brief grace, SIGKILL).
/// No-op on non-Unix platforms.
pub fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let pgid = format!("-{pid}");
        let _ = Command::new("kill")
            .args(["-TERM", &pgid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(200));
        let _ = Command::new("kill")
            .args(["-KILL", &pgid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Run `sh -c <cmd>` in `dir` with a hard wall-clock `timeout`.
///
/// The child runs in its own process group (on Unix) so that grandchildren
/// spawned by the shell (such as `pytest` or `python`) are killed together on
/// timeout rather than left spinning as orphans.
pub fn run_shell_timeout(cmd: &str, dir: &Path, timeout: Duration) -> std::io::Result<ShellOutput> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn()?;
    let pid = child.id();

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut r) = stdout_pipe {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut r) = stderr_pipe {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(st) => break Some(st),
            None => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    kill_process_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let out_bytes = out_handle.join().unwrap_or_default();
    let err_bytes = err_handle.join().unwrap_or_default();
    let mut output = String::new();
    if timed_out {
        output.push_str(&format!(
            "[concord: command timed out after {}s — possible infinite loop]\n",
            timeout.as_secs()
        ));
    }
    output.push_str(&String::from_utf8_lossy(&out_bytes));
    output.push_str(&String::from_utf8_lossy(&err_bytes));

    let ok = !timed_out && status.is_some_and(|s| s.success());
    Ok(ShellOutput { ok, timed_out, output })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_timeout_success_and_failure() {
        let dir = std::env::temp_dir();
        let ok = run_shell_timeout("echo hello && echo err >&2", &dir, Duration::from_secs(5)).unwrap();
        assert!(ok.ok);
        assert!(!ok.timed_out);
        assert!(ok.output.contains("hello"));
        assert!(ok.output.contains("err"));

        let fail = run_shell_timeout("echo bad; exit 3", &dir, Duration::from_secs(5)).unwrap();
        assert!(!fail.ok);
        assert!(!fail.timed_out);
        assert!(fail.output.contains("bad"));
    }

    #[test]
    fn shell_timeout_kills_hanging_command_and_children() {
        let dir = std::env::temp_dir();
        let start = Instant::now();
        let res = run_shell_timeout("sleep 30 & wait", &dir, Duration::from_millis(400)).unwrap();
        let elapsed = start.elapsed();
        assert!(!res.ok);
        assert!(res.timed_out);
        assert!(res.output.contains("timed out"));
        assert!(
            elapsed < Duration::from_secs(5),
            "expected prompt timeout kill, took {elapsed:?}"
        );
    }
}
