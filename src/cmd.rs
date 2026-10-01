//! Running external tools with captured output and an optional timeout.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Context, Error, Result};

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// The most useful diagnostic text the command produced.
    pub fn diagnostics(&self) -> String {
        let err = self.stderr.trim();
        if err.is_empty() {
            self.stdout.trim().to_string()
        } else {
            err.to_string()
        }
    }
}

/// Runs `program` with `args`, waiting at most `timeout` (if given).
///
/// The child never inherits our stdin, and both pipes are drained on helper
/// threads so a chatty tool can never dead-lock against a full pipe buffer.
pub fn run(program: &Path, args: &[&str], timeout: Option<Duration>) -> Result<Output> {
    run_with_env(program, args, &[], timeout)
}

/// Options for [`run_with`].
#[derive(Default)]
pub struct Options<'a> {
    /// Additional environment variables.
    pub env: &'a [(&'a str, &'a str)],
    /// Start from an empty environment (plus `env` and `LC_ALL=C`).
    pub clear_env: bool,
    /// Data written to the child's stdin (which is closed afterwards).
    pub input: Option<&'a [u8]>,
    pub timeout: Option<Duration>,
}

/// Like [`run`], with additional environment variables.
pub fn run_with_env(
    program: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Option<Duration>,
) -> Result<Output> {
    run_with(
        program,
        args,
        &Options {
            env,
            timeout,
            ..Options::default()
        },
    )
}

pub fn run_with(program: &Path, args: &[&str], options: &Options<'_>) -> Result<Output> {
    let display = program.display().to_string();
    let mut command = Command::new(program);
    if options.clear_env {
        command.env_clear();
    }
    command
        .args(args)
        .env("LC_ALL", "C")
        .envs(options.env.iter().copied())
        .stdin(if options.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context(format!("Could not start {display}"))?;

    let writer = match (child.stdin.take(), options.input) {
        (Some(mut stdin), Some(input)) => {
            let input = input.to_vec();
            Some(thread::spawn(move || {
                use std::io::Write;
                // A child that exits early closes the pipe; that is not our error.
                let _ = stdin.write_all(&input);
            }))
        }
        _ => None,
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let status = wait(&mut child, options.timeout, &display)?;
    if let Some(writer) = writer {
        let _ = writer.join();
    }

    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn wait(child: &mut Child, timeout: Option<Duration>, display: &str) -> Result<ExitStatus> {
    let Some(timeout) = timeout else {
        return child.wait().context(format!("Waiting for {display} failed"));
    };
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .context(format!("Waiting for {display} failed"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new(format!(
                "{display} did not finish within {} seconds",
                timeout.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_status() {
        let out = run(
            Path::new("/bin/sh"),
            &["-c", "echo out; echo err >&2; exit 3"],
            None,
        );
        let out = out.ok();
        assert!(out.is_some());
        if let Some(out) = out {
            assert_eq!(out.stdout.trim(), "out");
            assert_eq!(out.stderr.trim(), "err");
            assert_eq!(out.status.code(), Some(3));
            assert_eq!(out.diagnostics(), "err");
        }
    }

    #[test]
    fn passes_input_and_clears_env() {
        let out = run_with(
            Path::new("/bin/sh"),
            &["-c", "cat; echo \"[${HOME-unset}]\""],
            &Options {
                clear_env: true,
                input: Some(b"hello "),
                ..Options::default()
            },
        );
        assert_eq!(out.ok().map(|o| o.stdout), Some("hello [unset]\n".to_string()));
    }

    #[test]
    fn times_out() {
        let res = run(Path::new("/bin/sleep"), &["5"], Some(Duration::from_millis(200)));
        assert!(res.is_err());
    }

    #[test]
    fn missing_program_is_error() {
        assert!(run(Path::new("/nonexistent/tool"), &[], None).is_err());
    }
}
