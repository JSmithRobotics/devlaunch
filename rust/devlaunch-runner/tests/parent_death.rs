//! A child that leads its own group does not outlive a parent that was SIGKILLed.
//!
//! The child that leads its own group is `devpod up`, and it holds devpod's
//! workspace flock for as long as it lives. A `dl` that is SIGKILLed runs no
//! handler, so before `PR_SET_PDEATHSIG` the `up` was reparented to init and every
//! later `dl <ws>`, `rm` and `devpod delete` waited on its flock. Two of them were
//! found on a host after four hours.
//!
//! A SIGKILL cannot be sent to the test process itself, so this binary is
//! re-executed as the parent: with [`ROLE`] set it is the copy that spawns the
//! child and blocks on it, and the test kills that copy. The same shape as
//! `tests/terminal.rs`, for the same coverage reason given there.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use devlaunch_runner::{Invocation, ProcessRunner, Runner, SpawnSpec};

/// Set, this copy is the parent and the value is where its child writes its pid.
const ROLE: &str = "DEVLAUNCH_TEST_PARENT_DEATH_PIDFILE";

/// The parent's side: one own-group passthrough that would block for a minute.
///
/// A no-op in an ordinary run, where [`ROLE`] is unset.
#[test]
fn parent_side() {
    let Ok(pidfile) = std::env::var(ROLE) else {
        return;
    };
    let script = format!("echo $$ > {pidfile}; exec sleep 60");
    let spec = SpawnSpec::new(Invocation::new("/bin/sh").with_arg("-c").with_arg(script))
        .leading_its_own_group();
    let _ = ProcessRunner.passthrough(&spec);
}

fn read_pid(pidfile: &Path) -> Option<i32> {
    std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()
}

/// Whether `pid` is a live process. A zombie is not one: it has already died and
/// waits only for its new parent to reap it.
fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn an_own_group_child_dies_with_a_parent_that_was_sigkilled() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let pidfile = scratch.path().join("child.pid");
    let mut parent = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "parent_side", "--nocapture", "--test-threads=1"])
        .env(ROLE, &pidfile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the parent copy starts");

    let started = wait_for(|| read_pid(&pidfile).is_some_and(alive));
    if !started {
        let _ = parent.kill();
        let _ = parent.wait();
    }
    assert!(started, "the parent never started its child");
    let child = read_pid(&pidfile).expect("the child's pid");

    parent.kill().expect("SIGKILL the parent");
    parent.wait().expect("reap the parent");

    let died = wait_for(|| !alive(child));
    if !died {
        // Do not leave a minute-long sleep behind a failed test.
        // SAFETY: `kill` on a pid this test's own copy started.
        unsafe {
            libc::kill(child, libc::SIGKILL);
        }
    }
    assert!(
        died,
        "the own-group child {child} outlived the parent that was SIGKILLed"
    );
}
