//! Persistent CLI logging: tee this process's stdout/stderr to a log file.
//!
//! Moved verbatim from `bootc-migrate`'s `main.rs` so both binaries log the
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

/// Holds the tee thread + a copy of the real stdout.
///
/// Call [`TeeGuard::finish`] before `process::exit` (which skips
/// destructors). Plain returns drain via [`Drop`]: the migrator's success
/// path historically relied on teardown racing the tee thread, losing
/// trailing lines on fast exits — the `Drop` closes that race without
/// changing a byte of what is printed.
#[derive(Debug)]
pub struct TeeGuard {
    handle: Option<std::thread::JoinHandle<()>>,
    real_stdout: Option<rustix::fd::OwnedFd>,
}

impl TeeGuard {
    /// Flush, restore the real stdout/stderr (closing the pipe so the tee
    /// thread sees EOF), and wait for the thread to drain everything to
    /// stdout + log.
    pub fn finish(mut self) {
        self.drain();
    }

    fn drain(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        if let Some(fd) = self.real_stdout.take() {
            let _ = rustix::stdio::dup2_stdout(&fd);
            let _ = rustix::stdio::dup2_stderr(&fd);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TeeGuard {
    fn drop(&mut self) {
        self.drain();
    }
}

/// Redirect this process's stdout/stderr through a pipe to a background
/// thread that fans every chunk out to both the real terminal and `log_file`.
fn tee_stdio_to_log(log_file: File) -> rustix::io::Result<TeeGuard> {
    use std::io::{Read, Write};

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
        handle: Some(handle),
        real_stdout: Some(real_stdout),
    })
}
