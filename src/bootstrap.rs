//! Detach from the controlling terminal on Unix using a double `fork`, `setsid`, and `exec`.
//!
//! Forking after the Tokio runtime or other threads have started is undefined behavior, so the
//! async entry point only runs in the exec'd process. The initial process exits immediately so
//! the shell is not left waiting.

#[cfg(unix)]
mod unix {
    use std::env;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use anyhow::{Context, Result};

    pub fn maybe_detach() -> Result<()> {
        if cfg!(test) {
            return Ok(());
        }
        if env::var_os("VIA_RUNTIME_ROOT").is_some() {
            return Ok(());
        }
        if env::var_os("VIA_FOREGROUND").is_some() {
            return Ok(());
        }

        let argv: Vec<_> = env::args_os().collect();
        if argv.is_empty() {
            return Ok(());
        }

        // First fork: parent (shell child) exits so the shell regains the prompt immediately.
        let pid1 = unsafe { libc::fork() };
        if pid1 < 0 {
            return Err(std::io::Error::last_os_error())
                .context("first fork(2) failed while detaching from the terminal");
        }
        if pid1 > 0 {
            unsafe { libc::_exit(0) };
        }

        if unsafe { libc::setsid() } < 0 {
            return Err(std::io::Error::last_os_error()).context("setsid(2) failed");
        }

        // Second fork: relinquish session leadership so a controlling TTY is never acquired again.
        let pid2 = unsafe { libc::fork() };
        if pid2 < 0 {
            return Err(std::io::Error::last_os_error())
                .context("second fork(2) failed while detaching from the terminal");
        }
        if pid2 > 0 {
            unsafe { libc::_exit(0) };
        }

        let pid = std::process::id();
        let root = crate::config::via_data_dir().join(format!("via-{pid}"));
        std::fs::create_dir_all(root.join("logs"))
            .with_context(|| format!("create runtime directory {}", root.display()))?;
        println!("via runtime directory: {}", root.display());
        let _ = io::stdout().flush();

        let log_path = root.join("logs/via.log");
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open log file {}", log_path.display()))?;

        let exe = env::current_exe().context("resolve current executable for exec")?;

        let mut cmd = Command::new(&exe);
        cmd.arg0(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.env("VIA_RUNTIME_ROOT", &root);
        cmd.stdin(Stdio::null());
        let log_err = log_file
            .try_clone()
            .context("duplicate log file descriptor for stderr")?;
        cmd.stdout(Stdio::from(log_file));
        cmd.stderr(Stdio::from(log_err));

        let err = cmd.exec();
        Err(err).context("exec(2) failed while restarting via in the background")
    }
}

#[cfg(unix)]
pub use unix::maybe_detach;

#[cfg(not(unix))]
pub fn maybe_detach() -> anyhow::Result<()> {
    Ok(())
}
