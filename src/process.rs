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
