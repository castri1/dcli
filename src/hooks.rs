//! Pre/post-deploy hooks: shell commands run from the current directory.

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::Hook;
use crate::platform::shell;

const TIMEOUT: Duration = Duration::from_secs(300);

pub struct HookOutcome {
    pub success: bool,
    pub duration: Duration,
    /// Exit status or failure reason, for display.
    pub detail: String,
}

/// Runs a hook with output captured line by line (stdout and stderr interleaved).
pub fn run_captured(hook: &Hook, mut on_line: impl FnMut(String)) -> HookOutcome {
    let start = Instant::now();
    let child = shell(&hook.command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return HookOutcome { success: false, duration: start.elapsed(), detail: format!("cannot start: {e}") };
        }
    };

    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx);

    let status = loop {
        while let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            on_line(crate::util::strip_ansi(&line));
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                break Err("timed out after 5 minutes".to_string());
            }
            Ok(None) => {}
            Err(e) => break Err(e.to_string()),
        }
    };
    for r in readers {
        let _ = r.join();
    }
    for line in rx.try_iter() {
        on_line(crate::util::strip_ansi(&line));
    }

    let duration = start.elapsed();
    match status {
        Ok(s) if s.success() => HookOutcome { success: true, duration, detail: "ok".into() },
        Ok(s) => HookOutcome { success: false, duration, detail: format!("exited with {s}") },
        Err(e) => HookOutcome { success: false, duration, detail: e },
    }
}

/// Runs a hook attached to the terminal (CLI mode).
pub fn run_inherited(hook: &Hook) -> HookOutcome {
    let start = Instant::now();
    match shell(&hook.command).status() {
        Ok(s) => HookOutcome { success: s.success(), duration: start.elapsed(), detail: s.to_string() },
        Err(e) => HookOutcome { success: false, duration: start.elapsed(), detail: format!("cannot start: {e}") },
    }
}
