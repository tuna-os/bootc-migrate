//! Persistent run log for the destructive CLIs.
//!
//! Both `bootc-migrate` and `bootc-rebase` mutate the boot path and then ask
//! the user to reboot, so the terminal that carried the output is routinely
//! gone by the time anyone needs to know what happened. [`start`] appends a
//! run header to a log file and tees this process's stdout/stderr into it, so
//! the run survives the session that produced it.
//!
//! This is local-only: the log is a file on the machine being migrated.
//! Nothing here opens a socket or ships data anywhere.

use std::fs::File;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Holds the tee thread + a copy of the real stdout. Call [`TeeGuard::finish`]
/// before the process exits so short-lived commands (`commit --dry-run`) don't
/// lose their stdout: the thread only sees EOF once every writer of the pipe is
/// closed, which on a fast exit races process teardown.
#[derive(Debug)]
pub struct TeeGuard {
    handle: std::thread::JoinHandle<()>,
    real_stdout: rustix::fd::OwnedFd,
}

impl TeeGuard {
    /// Flush, restore the real stdout/stderr (closing the pipe so the tee thread
    /// sees EOF), and wait for the thread to drain everything to stdout + log.
    pub fn finish(self) {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        let _ = rustix::stdio::dup2_stdout(&self.real_stdout);
        let _ = rustix::stdio::dup2_stderr(&self.real_stdout);
        let _ = self.handle.join();
    }
}

/// Seconds since the epoch, or 0 if the clock is set before it (a badly-set
/// RTC should not cost us the whole log).
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The line that separates one run from the next in an appended log.
///
/// Without it a log holding a migration, a reboot, and a later `commit` reads
/// as one continuous run, and there is nothing in it saying which build
/// produced which half. Arguments are recorded because the failure mode
/// usually depends on them (`--skip-preflight`, `--bootloader`, the target
/// image); none of this tool's flags carry a secret.
fn run_header(program: &str, version: &str, unix_secs: u64, args: &[String]) -> String {
    let args = if args.is_empty() {
        "(none)".to_string()
    } else {
        args.join(" ")
    };
    format!("\n=== {program} {version} — run start, unix time {unix_secs} — args: {args} ===\n")
}

/// Append a run header to `log_path` and tee stdout/stderr into it.
///
/// Best-effort: on failure the reason is reported on stderr and `None` comes
/// back, in which case the caller simply runs without a persistent log rather
/// than refusing to work. `version` is whatever the binary reports for
/// `--version`.
pub fn start(log_path: &str, program: &str, version: &str) -> Option<TeeGuard> {
    let mut log_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Warning: could not open log file {log_path}: {e}");
            return None;
        }
    };
    eprintln!("Logging {program} output to {log_path}");

    // Written before the tee is installed so the header lands in the file
    // only — it is postmortem context, not something the operator watching
    // the terminal needs read back to them.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let _ = log_file.write_all(run_header(program, version, now_unix_secs(), &args).as_bytes());

    match tee_stdio_to_log(log_file) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("Warning: could not tee output to {log_path}: {e}");
            None
        }
    }
}

/// Redirect this process's stdout/stderr through a pipe to a background thread
/// that fans every chunk out to both the real terminal and `log_file`.
///
/// Best-effort: returns an error if the pipe/dup setup fails, in which case the
/// caller proceeds without persistent logging.
fn tee_stdio_to_log(log_file: File) -> rustix::io::Result<TeeGuard> {
    use std::io::Read;

    let (pipe_read, pipe_write) = rustix::pipe::pipe()?;
    // One dup for the tee thread to reach the terminal, one kept by the guard to
    // restore fd 1/2 on shutdown (which closes the pipe and unblocks the thread).
    let thread_stdout = rustix::io::dup(rustix::stdio::stdout())?;
    let real_stdout = rustix::io::dup(rustix::stdio::stdout())?;

    let handle = std::thread::spawn(move || {
        let mut reader = File::from(pipe_read);
        let mut stdout = File::from(thread_stdout);
        let mut log = log_file;
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = log.write_all(&buf[..n]);
            let _ = stdout.write_all(&buf[..n]);
        }
        let _ = log.flush();
        let _ = stdout.flush();
    });

    rustix::stdio::dup2_stdout(&pipe_write)?;
    rustix::stdio::dup2_stderr(&pipe_write)?;
    // Dropping our copy of the write end leaves only the redirected stdout/stderr
    // referencing it, so the tee thread sees EOF once those close (process exit
    // or TeeGuard::finish).
    drop(pipe_write);
    Ok(TeeGuard {
        handle,
        real_stdout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_starts_on_its_own_line_and_names_the_build() {
        let h = run_header(
            "bootc-rebase",
            "0.5.0",
            1_756_000_000,
            &[
                "rebase".into(),
                "-t".into(),
                "ghcr.io/example/img:tag".into(),
            ],
        );
        // Leading newline: the previous run's last line may not have ended in
        // one, and a header glued to it is the thing a reader scrolls for.
        assert!(h.starts_with('\n'));
        assert!(h.ends_with("===\n"));
        assert!(h.contains("bootc-rebase 0.5.0"));
        assert!(h.contains("unix time 1756000000"));
        assert!(h.contains("args: rebase -t ghcr.io/example/img:tag"));
    }

    #[test]
    fn header_records_a_bare_invocation_explicitly() {
        let h = run_header("bootc-migrate", "abc1234", 0, &[]);
        assert!(h.contains("args: (none)"));
    }

    #[test]
    fn now_unix_secs_is_past_the_2020s() {
        // Guards the unwrap_or(0) fallback from silently becoming the norm.
        assert!(now_unix_secs() > 1_577_836_800);
    }
}
