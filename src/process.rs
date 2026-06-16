//! Child-process termination helpers, ported from jterm4's state.rs. Used by the
//! block-view PTY to escalate SIGHUP → SIGTERM → SIGKILL off the GTK main thread.

use std::time::Duration;

fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { nix::libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(nix::libc::EPERM)
    )
}

/// Parse `/proc/<pid>/stat` and return the parent pid (the 4th field, after the
/// `comm` parenthesised name which may itself contain spaces/parens).
fn read_ppid(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = contents.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    fields.next(); // state
    fields.next()?.parse::<i32>().ok()
}

/// Read `/proc/<pid>/cmdline` as a NUL-separated argv vector.
fn read_proc_cmdline(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    let args: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

/// Check if an argv matches a known restorable command pattern, returning the
/// command string to replay on session restore. Ported from jterm4 state.rs.
pub(crate) fn match_restorable_command(args: &[String]) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    let bin = std::path::Path::new(&args[0])
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    match bin.as_str() {
        "nix" => {
            if args.len() >= 2 && args[1] == "develop" {
                Some(args.join(" "))
            } else {
                None
            }
        }
        // nix develop execs into e.g. `bash --rcfile /tmp/nix-shell.XXXXX`.
        "bash" | "zsh" | "fish" => {
            for arg in &args[1..] {
                if arg.starts_with("/tmp/nix-shell.") || arg.starts_with("/tmp/nix-shell-") {
                    return Some("nix develop".to_string());
                }
            }
            None
        }
        "ssh" | "mosh" => Some(args.join(" ")),
        "docker" | "podman" => {
            if args.len() >= 2
                && (args[1] == "exec"
                    || (args[1] == "compose" && args.len() >= 3 && args[2] == "exec"))
            {
                Some(args.join(" "))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The foreground process group id on a PTY master fd, or None if the shell
/// itself (`shell_pid`) is in the foreground (nothing interesting is running).
fn foreground_pgid(pty_fd: i32, shell_pid: i32) -> Option<i32> {
    if pty_fd < 0 {
        return None;
    }
    let fg = unsafe { nix::libc::tcgetpgrp(pty_fd) };
    if fg <= 0 || fg == shell_pid {
        return None;
    }
    Some(fg)
}

/// Detect a restorable interactive command (ssh/nix develop/docker exec/…) by
/// walking from the PTY's foreground process up to the shell. Mirrors jterm4's
/// `get_restorable_commands`.
pub(crate) fn restorable_command(pty_fd: i32, shell_pid: i32) -> Option<String> {
    let mut pid = foreground_pgid(pty_fd, shell_pid)?;
    let mut visited = 0;
    while pid != shell_pid && pid > 1 && visited < 16 {
        if let Some(args) = read_proc_cmdline(pid) {
            if let Some(cmd) = match_restorable_command(&args) {
                return Some(cmd);
            }
        }
        pid = match read_ppid(pid) {
            Some(ppid) => ppid,
            None => break,
        };
        visited += 1;
    }
    None
}

/// Name of the foreground process on a PTY (e.g. "ssh", "vim"), or None if the
/// shell itself is in the foreground. Used for the close-confirmation prompt.
pub(crate) fn foreground_process_name(pty_fd: i32, shell_pid: i32) -> Option<String> {
    let fg = foreground_pgid(pty_fd, shell_pid)?;
    let args = read_proc_cmdline(fg)?;
    std::path::Path::new(args.first()?)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

fn get_process_group_id(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let path = format!("/proc/{pid}/stat");
    let contents = std::fs::read_to_string(path).ok()?;
    // stat format: pid (comm) state ppid pgrp ...; comm may contain spaces and
    // parens, so split after the last ')'.
    let rparen_pos = contents.rfind(')')?;
    let after_comm = &contents[rparen_pos + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    if fields.len() >= 3 {
        fields[2].parse().ok()
    } else {
        None
    }
}

fn signal_pid_and_group(pid: i32, sig: std::ffi::c_int) {
    if pid <= 0 {
        return;
    }
    let rc = unsafe { nix::libc::kill(pid, sig) };
    if rc < 0 {
        return;
    }
    // Only signal the group if this pid is the group leader — avoids hitting
    // unrelated processes if the pid was reused.
    if let Some(pgid) = get_process_group_id(pid) {
        if pgid == pid {
            unsafe {
                nix::libc::kill(-pid, sig);
            }
        }
    }
}

fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while process_exists(pid) {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    true
}

pub fn terminate_terminal_process(pid: i32) {
    if pid <= 0 {
        return;
    }
    signal_pid_and_group(pid, nix::libc::SIGHUP);
    std::thread::spawn(move || {
        if wait_for_process_exit(pid, Duration::from_millis(120)) {
            return;
        }
        signal_pid_and_group(pid, nix::libc::SIGTERM);
        if wait_for_process_exit(pid, Duration::from_millis(250)) {
            return;
        }
        signal_pid_and_group(pid, nix::libc::SIGKILL);
        let _ = wait_for_process_exit(pid, Duration::from_millis(150));
    });
}
