//! Child-process runner that logs everything it does.

use std::ffi::OsString;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::log::Logger;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Result of a finished child process.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn quote_for_log(a: &std::ffi::OsStr) -> String {
    let s = a.to_string_lossy();
    if s.is_empty() || s.contains([' ', '\t', '"']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.into_owned()
    }
}

/// Runs `program` with `args` (each passed as its own argument, never concatenated),
/// logs the command line, exit code and captured output, and returns them.
pub fn run_tool(log: &Logger, program: &Path, args: &[OsString]) -> io::Result<ToolOutput> {
    let mut cmdline = quote_for_log(program.as_os_str());
    for a in args {
        cmdline.push(' ');
        cmdline.push_str(&quote_for_log(a));
    }
    log.info(&format!("run: {cmdline}"));

    let out = match Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            log.error(&format!("failed to start {}: {e}", program.display()));
            return Err(e);
        }
    };

    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    log.info(&format!("exit code {code}"));
    for l in stdout.lines() {
        log.info(&format!("  stdout: {l}"));
    }
    for l in stderr.lines() {
        log.info(&format!("  stderr: {l}"));
    }
    Ok(ToolOutput {
        code,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Logger;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn runs_cmd_echo_and_logs_command_line() {
        let base = tempfile::Builder::new()
            .prefix("log dir ")
            .tempdir()
            .unwrap();
        assert!(base.path().to_string_lossy().contains(' '));
        let log = Logger::open(base.path(), "run").unwrap();
        let args: Vec<OsString> = ["/c", "echo", "a b"].iter().map(OsString::from).collect();
        let out = run_tool(&log, Path::new("cmd.exe"), &args).unwrap();
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("a b"), "stdout={:?}", out.stdout);
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(text.contains("cmd.exe"), "{text}");
        assert!(text.contains("echo"), "{text}");
        assert!(text.contains("exit code 0"), "{text}");
    }

    #[test]
    fn nonzero_exit_code_is_returned() {
        let base = tempfile::tempdir().unwrap();
        let log = Logger::open(base.path(), "run").unwrap();
        let args: Vec<OsString> = ["/c", "exit", "3"].iter().map(OsString::from).collect();
        let out = run_tool(&log, Path::new("cmd.exe"), &args).unwrap();
        assert_eq!(out.code, 3);
    }
}
