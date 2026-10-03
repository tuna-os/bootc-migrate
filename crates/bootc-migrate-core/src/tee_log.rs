//! Persistent CLI logging: tee this process's stdout/stderr to a log file.
//!
//! Moved from `bootc-migrate`'s `main.rs` so both binaries log the
//! same way. Best-effort throughout: when the log cannot be opened or the
//! pipe setup fails, the caller proceeds on the terminal alone.

use std::fs::File;

/// Open `log_path` for appending and redirect this process's stdout/stderr
/// through a background thread that fans every chunk out to both the real
/// terminal and the log.
///
/// Prints where logging goes (or why it could not start) to the real
/// stderr *before* the redirect, so the notice itself stays out of the log.
/// Returns `None` when logging is unavailable; the caller runs unlogged.
pub fn install(log_path: &str, what: &str) -> Option<TeeGuard> {
    let log_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(f) => {
            eprintln!("Logging {what} output to {log_path}");
            Some(f)
        }
        Err(e) => {
            eprintln!("Warning: could not open log file {log_path}: {e}");
            None
        }
    };
    log_file.and_then(|f| tee_stdio_to_log(f).ok())
}

/// Holds the tee threads + copies of the real stdout and stderr.
///
/// Call [`TeeGuard::finish`] before `process::exit` (which skips
/// destructors). Plain returns drain via [`Drop`]: the migrator's success
/// path historically relied on teardown racing the tee thread, losing
/// trailing lines on fast exits — the `Drop` closes that race without
/// changing a byte of what is printed.
#[derive(Debug)]
pub struct TeeGuard {
    handles: Vec<std::thread::JoinHandle<()>>,
    real_stdout: Option<rustix::fd::OwnedFd>,
    real_stderr: Option<rustix::fd::OwnedFd>,
}

impl TeeGuard {
    /// Flush, restore the real stdout/stderr (closing the pipes so the tee
    /// threads see EOF), and wait for the threads to drain everything to
    /// the terminal + log.
    pub fn finish(mut self) {
        self.drain();
    }

    fn drain(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        if let Some(fd) = self.real_stdout.take() {
            let _ = rustix::stdio::dup2_stdout(&fd);
        }
        if let Some(fd) = self.real_stderr.take() {
            let _ = rustix::stdio::dup2_stderr(&fd);
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for TeeGuard {
    fn drop(&mut self) {
        self.drain();
    }
}

/// Copy everything read from `reader` to both `out` and `log`, until EOF.
fn pump(reader: File, mut out: File, mut log: File) {
    use std::io::{Read, Write};
    let mut reader = reader;
    let mut buf = [0u8; 8192];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        let _ = log.write_all(&buf[..n]);
        let _ = out.write_all(&buf[..n]);
    }
    let _ = log.flush();
    let _ = out.flush();
}

/// Redirect this process's stdout and stderr each through its own pipe to a
/// background thread that fans every chunk out to both the real stream and
/// `log_file`.
///
/// The two streams stay separate: a command whose stdout is machine-read
/// (`boot-entries --json`) must not have its stderr notes (`[audit]
/// Auto-mounted ESP ...`) land in front of the JSON.
fn tee_stdio_to_log(log_file: File) -> rustix::io::Result<TeeGuard> {
    let (out_read, out_write) = rustix::pipe::pipe()?;
    let (err_read, err_write) = rustix::pipe::pipe()?;
    // One dup per stream for its tee thread to reach the terminal, one kept by
    // the guard to restore fd 1/2 on shutdown (which closes the pipes and
    // unblocks the threads).
    let thread_stdout = rustix::io::dup(rustix::stdio::stdout())?;
    let thread_stderr = rustix::io::dup(rustix::stdio::stderr())?;
    let real_stdout = rustix::io::dup(rustix::stdio::stdout())?;
    let real_stderr = rustix::io::dup(rustix::stdio::stderr())?;
    let err_log = log_file
        .try_clone()
        .map_err(|e| rustix::io::Errno::from_io_error(&e).unwrap_or(rustix::io::Errno::IO))?;

    let out_handle =
        std::thread::spawn(move || pump(File::from(out_read), File::from(thread_stdout), log_file));
    let err_handle =
        std::thread::spawn(move || pump(File::from(err_read), File::from(thread_stderr), err_log));

    rustix::stdio::dup2_stdout(&out_write)?;
    rustix::stdio::dup2_stderr(&err_write)?;
    // Dropping our copies of the write ends leaves only the redirected
    // stdout/stderr referencing them, so the tee threads see EOF once those
    // close (process exit or TeeGuard::finish).
    drop(out_write);
    drop(err_write);
    Ok(TeeGuard {
        handles: vec![out_handle, err_handle],
        real_stdout: Some(real_stdout),
        real_stderr: Some(real_stderr),
    })
}
