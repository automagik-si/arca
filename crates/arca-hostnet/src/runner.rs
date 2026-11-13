//! Running a command. The root part reaches the host only through a [`Runner`], so a test can stand in a fake host.

use std::process::{Command, Stdio};

/// One command: a program and its arguments, run without a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Cmd {
        Cmd {
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }

    /// The command as one line, for a status file and a log.
    pub fn line(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

/// What a command answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs commands. An `Err` is a command that could not start at all, such as a program that is not installed.
pub trait Runner {
    fn run(&self, cmd: &Cmd) -> Result<Output, String>;
}

/// The real runner: the command runs with an empty environment and the system's own `PATH`.
pub struct System;

impl Runner for System {
    fn run(&self, cmd: &Cmd) -> Result<Output, String> {
        let output = Command::new(&cmd.program)
            .args(&cmd.args)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("{} could not start: {error}", cmd.program))?;
        Ok(Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_reads_as_one_line() {
        assert_eq!(
            Cmd::new("sysctl", &["-w", "net.ipv4.ip_forward=1"]).line(),
            "sysctl -w net.ipv4.ip_forward=1"
        );
    }

    #[test]
    fn the_system_runner_runs_without_a_shell_and_reports_a_missing_program() {
        let said = System
            .run(&Cmd::new("echo", &["a;b", "$HOME"]))
            .expect("echo runs");
        assert_eq!(said.code, 0);
        assert_eq!(said.stdout, "a;b $HOME\n");
        assert!(System.run(&Cmd::new("arca-no-such-program", &[])).is_err());
    }
}
