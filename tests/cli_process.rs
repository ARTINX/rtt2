//! Real process/IPC checks; these never connect to a hardware probe.
#![cfg(any(windows, unix))]
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Shell {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Shell {
    fn start(explicit: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rtt2"));
        if explicit {
            command.args(["cli", "--json"]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut shell = Self {
            child,
            input,
            output,
        };
        assert_eq!(shell.read()["event"], "ready");
        shell
    }
    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }
    fn send(&mut self, command: &str) -> Value {
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        self.read()
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn persistent_shells_share_worker_and_exit_independently() {
    // Bare exe with redirected input must enter CLI, without an explicit verb.
    let mut first = Shell::start(false);
    let mut second = Shell::start(true);
    assert_eq!(first.send("status")["event"], "status");
    assert_eq!(second.send("status")["event"], "status");
    assert_eq!(first.send("quit")["event"], "quit");
    assert!(first.child.wait().unwrap().success());
    assert_eq!(second.send("status")["event"], "status");
    let bad = second.send("catch NaN forbidden.csv");
    assert_eq!(bad["event"], "error");
    assert_eq!(bad["ok"], false);
    assert_eq!(second.send("send 0")["data"]["status"], "connection_failed");
    assert_eq!(second.send("quit")["data"]["exit_code"], 5);
    assert_eq!(second.child.wait().unwrap().code(), Some(5));
}

#[test]
fn version_help_and_stdin_eof_have_defined_exit_behavior() {
    let exe = env!("CARGO_BIN_EXE_rtt2");
    let version = Command::new(exe).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8(version.stdout).unwrap().contains(env!("CARGO_PKG_VERSION")));
    let help = Command::new(exe).arg("--help").output().unwrap();
    assert!(help.status.success());
    let output = Command::new(exe)
        .args(["cli", "--json"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["event"], "ready");
    assert_eq!(events.last().unwrap()["event"], "quit");
    assert!(
        !Command::new(exe)
            .args(["send", "0"])
            .output()
            .unwrap()
            .status
            .success()
    );
}
