//! The interactive default, judged on a real pty.
//!
//! `aid <workspace>` with no prompt on a terminal boots the workspace in the
//! background while the prompt is typed, then attaches with what was typed. A
//! terminal is the one thing `Command` cannot fake — the flow is gated on
//! `isatty` of stdin *and* stdout — so these tests drive the binary through a
//! pty and type at it the way a person would. The world is `dl`'s scenario with
//! `test/fixtures/devpod_shim.py` as devpod, same as `tests/rewrite.rs`; the
//! Ctrl-C case rebuilds `tests/interrupt.rs`'s blocking `up` on the pty.
//!
//! The non-terminal half of the contract — a piped or null stdin keeps the old
//! one-shot behaviour — is pinned in `tests/rewrite.rs`, whose runs have no tty
//! by construction.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// The workspace id this build derives for `blooop/devlaunch@main`, which the
/// scenario records and devpod knows.
const MAIN: &str = "devlaunch-main-3j1t";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repository root")
}

/// One scratch world, the shape `tests/rewrite.rs` builds.
struct World {
    root: PathBuf,
    _scratch: tempfile::TempDir,
}

impl World {
    fn with(fixtures: &[&str]) -> Self {
        let scratch = tempfile::Builder::new()
            .prefix("aidpty")
            .tempdir_in("/tmp")
            .expect("a scratch directory under /tmp");
        let root = scratch.path().to_path_buf();
        let dl_tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dl/tests");
        let built = Command::new("python3")
            .arg(dl_tests.join("launch_scenario.py"))
            .arg(&root)
            .arg(repo_root().join("test/fixtures/devpod_shim.py"))
            .args(fixtures)
            .output()
            .expect("python3 is installed");
        assert!(
            built.status.success(),
            "launch_scenario.py failed: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        World {
            root,
            _scratch: scratch,
        }
    }

    /// The devpod calls made so far, in order, with `devpod list` left out — the
    /// detached completion refresh makes one of its own on its own schedule.
    ///
    /// Unlike `tests/rewrite.rs`'s copy, this one is polled *while* the shim is
    /// appending (the overlap test watches for the `up` mid-edit), so a line read
    /// half-written is skipped rather than panicked on: it is whole on the next
    /// poll, and by the time the post-exit assertions read the log nothing is
    /// still writing.
    fn devpod_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("shim-log.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let call: serde_json::Value = serde_json::from_str(line).ok()?;
                call["argv"]
                    .as_array()?
                    .iter()
                    .map(|word| word.as_str().map(str::to_owned))
                    .collect::<Option<Vec<String>>>()
            })
            .filter(|argv| argv.first().map(String::as_str) != Some("list"))
            .map(|argv| format!("devpod {}", argv.join(" ")))
            .collect()
    }
}

/// `aid` on a pty: the child, a way to type at it, and everything it printed.
struct PtyAid {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn std::io::Write + Send>,
    seen: Arc<Mutex<Vec<u8>>>,
    // Held so the master side outlives the session; dropping it would hang up
    // the terminal under the child.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl PtyAid {
    fn spawn(world: &World, args: &[&str], extra: &[(&str, &str)]) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("a pty");
        let root = world.root.display().to_string();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_aid"));
        command.args(args);
        command.env_clear();
        // `KeepingCoverage` by hand: the trait extends `std::process::Command`,
        // and this builder is not one.
        if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command.env("PATH", format!("{root}/bin:{root}/gh-bin:/usr/bin:/bin"));
        command.env("HOME", format!("{root}/home"));
        command.env("XDG_CACHE_HOME", format!("{root}/cache"));
        command.env("XDG_CONFIG_HOME", format!("{root}/config"));
        command.env("DEVPOD_HOME", format!("{root}/devpod"));
        command.env("DEVPOD_SHIM_STATE", format!("{root}/shim-state.json"));
        command.env("DEVPOD_SHIM_LOG", format!("{root}/shim-log.jsonl"));
        command.env("DEVPOD_SHIM_CONFIG", format!("{root}/shim-config.json"));
        command.env("GIT_SSH_COMMAND", "false");
        command.env("GIT_CONFIG_GLOBAL", "/dev/null");
        command.env("GIT_CONFIG_SYSTEM", "/dev/null");
        // A terminal type, as every real terminal sets: skim reads terminfo by it
        // and draws no picker without one. `aid`'s answer to no `TERM` is its own
        // test below, which removes it again.
        command.env("TERM", "xterm-256color");
        if !world.root.join("gh-bin/gh").exists() {
            command.env("DEVLAUNCH_NO_GH_TOKEN", "1");
        }
        for (name, value) in extra {
            command.env(name, value);
        }
        let child = pty
            .slave
            .spawn_command(command)
            .expect("aid spawns on the pty");
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().expect("the pty reader");
        let writer = pty.master.take_writer().expect("the pty writer");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                sink.lock()
                    .expect("the pty buffer")
                    .extend_from_slice(&chunk[..read]);
            }
        });
        PtyAid {
            child,
            writer,
            seen,
            _master: pty.master,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.seen.lock().expect("the pty buffer")).into_owned()
    }

    fn expect(&self, needle: &str) {
        assert!(
            wait_for(|| self.text().contains(needle)),
            "{needle:?} never appeared; the pty said:\n{}",
            self.text()
        );
    }

    /// Type a line and press Enter, the way a person at the terminal would.
    ///
    /// Enter is `\r`, which is what a terminal sends for it. The prompt editor
    /// holds the terminal in raw mode, where nothing turns it into `\n`, and a
    /// `\n` is Ctrl-J, which adds a line rather than submitting. So a `\n` inside
    /// `line` is a line of the prompt.
    ///
    /// One `write_all`, so the Enter comes in the same burst as the text. The
    /// editor reads an Enter with more input right behind it as a pasted line
    /// break; nothing follows this one, so it submits.
    fn send_line(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r").as_bytes())
            .and_then(|()| self.writer.flush())
            .expect("typing into the pty");
    }

    /// A terminal Ctrl-C. In cooked mode the line discipline turns it into SIGINT
    /// for the whole foreground process group. The prompt editor and the pickers
    /// hold the terminal in raw mode, where it is a byte they read and act on.
    fn interrupt(&mut self) {
        self.writer
            .write_all(b"\x03")
            .and_then(|()| self.writer.flush())
            .expect("interrupting the pty");
    }

    /// Press keys in a picker. Raw bytes, because skim holds the terminal in raw
    /// mode: Enter is `\r` there, not a line.
    fn press(&mut self, keys: &str) {
        self.writer
            .write_all(keys.as_bytes())
            .and_then(|()| self.writer.flush())
            .expect("typing into the pty");
    }

    /// Wait for the picker whose header holds `header` and answer it with `keys`.
    ///
    /// Counted by occurrences, so a second picker with the same words is waited
    /// for rather than answered from the first one's screen.
    fn answer(&mut self, header: &str, keys: &str) {
        let before = self.text().matches(header).count();
        assert!(
            wait_for(|| self.text().matches(header).count() > before),
            "the picker {header:?} never appeared; the pty said:\n{}",
            self.text()
        );
        // skim draws its header before it reads keys; a short pause keeps a key
        // from landing while it is still setting up the terminal.
        std::thread::sleep(Duration::from_millis(150));
        self.press(keys);
    }

    /// Answer a picker by typing `query`, then pressing Enter once skim has matched.
    ///
    /// Two writes with a pause between, because skim takes Enter against the last
    /// match it finished: a query and its Enter in one write reach it before the
    /// match does, and Enter takes the old first row.
    fn answer_typed(&mut self, header: &str, query: &str) {
        self.answer(header, query);
        std::thread::sleep(Duration::from_millis(400));
        self.press("\r");
    }

    /// Take the first row of the agent, model and effort pickers, then wait for
    /// the prompt editor. On a first run that is claude with every default, which
    /// is every launch's way in when nothing is chosen.
    fn reach_the_editor(&mut self) {
        self.answer(AGENT_PICKER, "\r");
        self.answer(MODEL_PICKER, "\r");
        self.answer(EFFORT_PICKER, "\r");
        self.expect(BANNER);
    }

    fn wait(mut self) -> u32 {
        self.child.wait().expect("aid exits").exit_code()
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// The words the editor's banner ends with — the string the tests key on, and
/// the one the e2e suite keys on too.
const BANNER: &str = "press Enter";

/// Words in the header of claude's model picker.
const MODEL_PICKER: &str = "Model for claude";

/// Words in the header of claude's effort picker.
const EFFORT_PICKER: &str = "Effort for claude";

/// Words in the header of the agent picker, which also chooses claude's login.
const AGENT_PICKER: &str = "Agent for this launch";

/// The name [`MAIN`] is put on a terminal under.
///
/// The label and not the id, although the id is what is typed: the scenario records
/// the triple, and a workspace reached by its id is looked up rather than parsed
/// back out (blooop/devlaunch#632).
const TITLED: &str = "devlaunch@main";

/// The OSC 2 escape that names a terminal *name*, bytes and all.
fn osc_title(name: &str) -> String {
    format!("\x1b]2;{name}\x07")
}

#[test]
fn the_terminal_really_is_named_on_a_real_pty() {
    // The one test that proves the bytes arrive. Everything else about the title
    // is judged on a `Host` value or a notice; this watches the escape come out of
    // the pty, through the shipped binary, which is the only place the
    // stderr-is-a-tty guard is exercised for real.
    //
    // A *typed* prompt rather than an argv one, because the editor window is where
    // the name is decided and where a wrong one is looked at longest. `aid <id>`
    // with nothing on the line names the terminal itself, in front of a boot it
    // cannot wait for, and holds that name for the typing plus an entire
    // `devpod up`. So the run writes the escape twice -- once before the banner,
    // once at the handover -- and waiting only for the second one passes whether or
    // not the first said `devlaunch-main-3j1t`, which is exactly what
    // blooop/devlaunch#632 was reported as.
    let titled = osc_title(TITLED);
    let by_id = osc_title(MAIN);
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    // Exact bytes and from the very first one: nothing precedes the name on this
    // terminal, so a prefix check is the whole claim about what the editor window
    // is titled.
    assert!(
        session.text().starts_with(&titled),
        "the editor window was not named {titled:?}; the pty said:\n{:?}",
        session.text()
    );
    session.send_line("fix the bug");
    session.expect("aid -> dl");
    let seen = Arc::clone(&session.seen);
    assert_eq!(session.wait(), 0);

    let whole = String::from_utf8_lossy(&seen.lock().expect("the pty buffer")).into_owned();
    assert_eq!(
        whole.matches(&titled).count(),
        2,
        "the name is written twice, by the editor and by the launch; the pty said:\n{whole:?}"
    );
    assert!(
        !whole.contains(&by_id),
        "the id was written as a title; the pty said:\n{whole:?}"
    );
}

#[test]
fn the_title_switch_really_silences_it_on_a_real_pty() {
    // Off means no escape at all, not an empty title: `ESC ] 2 ; BEL` would blank
    // the terminal's name, which is not what "leave it alone" means.
    let world = World::with(&["--warm"]);
    let session = PtyAid::spawn(
        &world,
        &[MAIN, "fix", "the", "bug"],
        &[("DEVLAUNCH_NO_TITLE", "1")],
    );
    // `SSH command:` is said from inside the session call, which the title is
    // written in front of -- so once this is on screen, a title that was coming
    // has already come and its absence means something.
    session.expect("SSH command:");
    let seen = session.text();

    assert!(
        !seen.contains("\x1b]2;"),
        "an OSC 2 was written anyway; the pty said:\n{seen:?}"
    );
    assert_eq!(session.wait(), 0);
}

#[test]
fn a_falsey_no_tty_leaves_the_terminal_alone_on_a_real_pty() {
    // One variable, one reading. `aid`'s prompt and the ssh transport are both
    // gated on `DEVLAUNCH_NO_TTY`, and they used to disagree about what it says:
    // core lowercased and stripped the value before comparing it against the
    // falsey words, and `dl`'s own copy of that predicate compared the raw bytes
    // with a bare `matches!`. So `FALSE` — a spelling a person writes without
    // thinking, and the one `DEVLAUNCH_NO_TITLE=FALSE` would get right — kept the
    // pty for the transport and took it away from the prompt.
    //
    // Cased *and* padded, because those were two separate halves of the divergence
    // (`to_lowercase` and `osext::strip`) and either one alone still hides the
    // other. The banner appearing is the whole assertion: no banner means aid
    // decided there was no terminal to prompt at.
    // One world for both spellings rather than one each: these tests are timing
    // sensitive under a loaded `--workspace` run, and a scenario build is the
    // expensive half of a case that only needs a warm workspace to prompt about.
    let world = World::with(&["--warm"]);
    for value in ["FALSE", " no "] {
        let mut session = PtyAid::spawn(&world, &[MAIN], &[("DEVLAUNCH_NO_TTY", value)]);
        session.reach_the_editor();
        session.send_line("fix the bug");
        session.expect("aid -> dl");
        assert_eq!(session.wait(), 0, "DEVLAUNCH_NO_TTY={value:?}");
    }

    // And the truthy direction, in the same test because it is the same claim:
    // the reading is consulted at all. Without this, the wrapper's whole body
    // could be `false` — killing `DEVLAUNCH_NO_TTY=1` for every `dl` and `aid`
    // there is — and all three crates stay green, which is what deleting `dl`'s
    // own truthy test left behind.
    //
    // Asserted by what a skipped prompt *does* rather than by waiting out a
    // banner that never comes: with no terminal to prompt at, `aid` hands the
    // agent line straight to dl, so `aid -> dl` arriving with nothing typed is
    // the opt-out working. Waiting for the banner's absence would cost the
    // 60-second deadline on the passing path.
    let opted_out = PtyAid::spawn(&world, &[MAIN], &[("DEVLAUNCH_NO_TTY", "1")]);
    opted_out.expect("aid -> dl");
    assert!(
        !opted_out.text().contains(BANNER),
        "the editor prompted anyway; the pty said:\n{}",
        opted_out.text()
    );
    assert_eq!(opted_out.wait(), 0);
}

#[test]
fn a_typed_prompt_reaches_the_agent_with_no_shell_in_the_way() {
    // The double quotes are the point: they reach the agent literally, because
    // the prompt never passes through a shell on the host — the escaping pain the
    // editor exists to end.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    session.send_line("fix the \"flaky\" test");
    session.expect("aid -> dl");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &format!(
            "devpod ssh {MAIN} --log-output json --command bash -lc 'CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1 IS_SANDBOX=1 claude \
             --dangerously-skip-permissions --remote-control={MAIN} \
             '\"'\"'fix the \"flaky\" test'\"'\"''"
        )
    );
}

#[test]
fn a_pasted_multi_line_prompt_arrives_whole_rather_than_leaking() {
    // A paste delivers its newlines with it, and the terminal holds the later
    // lines as completed input. The editor must drain them into the prompt: a
    // submission cut at the first newline would hand the agent line one and leave
    // the rest queued in the terminal, to land inside the agent's session as
    // keystrokes.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    // One write, as a terminal delivers a paste: both lines arrive together, so
    // the second is already queued when the first's Enter is read. `send_line`
    // issuing exactly one write is what keeps that sentence true.
    session.send_line("fix this\nand then that");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &format!(
            "devpod ssh {MAIN} --log-output json --command bash -lc 'CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1 IS_SANDBOX=1 claude \
             --dangerously-skip-permissions --remote-control={MAIN} \
             '\"'\"'fix this\nand then that'\"'\"''"
        )
    );
}

#[test]
fn an_empty_enter_is_the_plain_session_it_always_was() {
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    session.send_line("");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &format!(
            "devpod ssh {MAIN} --log-output json --command bash -lc 'CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1 IS_SANDBOX=1 claude \
             --dangerously-skip-permissions --remote-control={MAIN}'"
        )
    );
}

/// The session line `aid resume` hands dl for [`MAIN`]: the agent's own line with
/// `--resume` as its last word.
fn resumed_session() -> String {
    format!(
        "devpod ssh {MAIN} --log-output json --command bash -lc 'CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1 IS_SANDBOX=1 claude \
         --dangerously-skip-permissions --remote-control={MAIN} --resume'"
    )
}

#[test]
fn a_resume_line_on_a_terminal_opens_no_editor() {
    // A resume line has no prompt, and an empty prompt on a terminal is exactly
    // what opens the editor for an agent line. Only the real gate on a real pty
    // says which of the two a resume line is.
    let world = World::with(&["--warm"]);
    let session = PtyAid::spawn(&world, &["resume", MAIN], &[]);
    session.expect("aid -> dl");
    let seen = Arc::clone(&session.seen);
    assert_eq!(session.wait(), 0);

    let whole = String::from_utf8_lossy(&seen.lock().expect("the pty buffer")).into_owned();
    assert!(
        !whole.contains("Type the prompt") && !whole.contains(BANNER),
        "a resume line opened the editor; the pty said:\n{whole}"
    );
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &resumed_session()
    );
}

#[test]
fn a_resume_with_no_workspace_resumes_the_row_the_picker_took() {
    // `aid resume` alone asks dl's picker, and the id that comes back is the one
    // the line is then built around. The row is taken through skim, whose answer is
    // a label that dl maps back to an id, so only a pick made on a terminal proves
    // the id that reaches the session is the row's.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &["resume"], &[("TERM", "xterm-256color")]);
    session.expect("Select workspace (type to filter):");
    session.expect("blooop | devlaunch | main");
    session
        .writer
        .write_all(b"\r")
        .and_then(|()| session.writer.flush())
        .expect("taking the row on the pty");
    session.expect("aid -> dl");
    let seen = Arc::clone(&session.seen);
    assert_eq!(session.wait(), 0);

    let whole = String::from_utf8_lossy(&seen.lock().expect("the pty buffer")).into_owned();
    assert!(
        whole.contains(&format!("Picked blooop | devlaunch | main -> {MAIN}")),
        "the pick named no row; the pty said:\n{whole:?}"
    );
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &resumed_session()
    );
}

#[test]
fn the_boot_runs_while_the_prompt_is_still_being_typed() {
    // The overlap itself: a stopped workspace's `devpod up` is on the shim's log
    // while the editor is still open — nothing has been typed yet — and the
    // attach that follows the Enter finds it running.
    let world = World::with(&["--stopped"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    assert!(
        wait_for(|| {
            world
                .devpod_calls()
                .iter()
                .any(|call| call.starts_with("devpod up "))
        }),
        "the boot never asked devpod for an up while the editor was open: {:?}",
        world.devpod_calls()
    );
    session.send_line("go");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &format!(
            "devpod ssh {MAIN} --log-output json --command bash -lc 'CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1 IS_SANDBOX=1 claude \
             --dangerously-skip-permissions --remote-control={MAIN} go'"
        )
    );
}

#[test]
fn a_ctrl_c_at_the_editor_tears_the_whole_boot_down() {
    // `tests/interrupt.rs` on the pty. The editor holds the terminal in raw mode,
    // so the Ctrl-C is a byte and no SIGINT is sent: aid itself interrupts the
    // boot child, whose own handler kills the blocked `devpod up` (in a group of
    // its own) and unlinks the staged token.
    let world = World::with(&["--gh"]);
    let devpod = world.root.join("bin/devpod");
    let original = std::fs::read_to_string(&devpod).expect("the scenario's devpod");
    let delegate = original
        .lines()
        .find(|line| line.starts_with("exec "))
        .expect("the delegate exec line");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"up\" ]; then\n\
         \x20 echo \"$$\" > \"$DL_UP_PID\"\n\
         \x20 : > \"$DL_UP_STARTED\"\n\
         \x20 exec sleep 30\n\
         fi\n\
         {delegate}\n"
    );
    std::fs::write(&devpod, script).expect("rewrite devpod");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&devpod, std::fs::Permissions::from_mode(0o755))
        .expect("keep devpod executable");
    let tmpdir = world.root.join("tmp");
    std::fs::create_dir_all(&tmpdir).expect("a scratch TMPDIR");
    let up_pid = world.root.join("up.pid");
    let up_started = world.root.join("up.started");

    let mut session = PtyAid::spawn(
        &world,
        &["blooop/devlaunch@cold"],
        &[
            ("TMPDIR", &tmpdir.display().to_string()),
            ("DL_UP_PID", &up_pid.display().to_string()),
            ("DL_UP_STARTED", &up_started.display().to_string()),
        ],
    );
    session.reach_the_editor();
    // Interrupt only once the boot is mid-`up` with the token staged — the exact
    // state the interrupt handler exists to clean.
    assert!(
        wait_for(|| up_started.exists() && token_file(&tmpdir).is_some()),
        "devpod up never blocked with a token staged"
    );
    let up = std::fs::read_to_string(&up_pid).expect("the up pid");
    let up = up.trim().to_owned();

    session.interrupt();
    assert_eq!(session.wait(), 130, "aid exits 130 on interrupt");
    // The boot child cleans up on its own clock, a moment after the parent died.
    assert!(
        wait_for(|| token_file(&tmpdir).is_none()),
        "the token file must be gone after the interrupt"
    );
    assert!(
        wait_for(|| !Command::new("kill")
            .args(["-0", &up])
            .output()
            .expect("kill is installed")
            .status
            .success()),
        "the orphaned devpod up (pid {up}) must have been killed"
    );
}

/// The one staged GitHub-token file under `dir`, if any.
fn token_file(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().into_owned();
        (name.starts_with("devlaunch-gh-") && name.ends_with(".env")).then_some(path)
    })
}

// ===========================================================================
// the pickers ahead of the editor
// ===========================================================================

/// The claude payload a launch sends, with the words the pickers added.
fn claude_session(settings: &str, prompt: &str) -> String {
    format!(
        "devpod ssh {MAIN} --log-output json --command bash -lc 'CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1 \
         IS_SANDBOX=1 claude --dangerously-skip-permissions {settings}--remote-control={MAIN} {prompt}'"
    )
}

#[test]
fn a_model_that_is_not_listed_is_typed_and_reaches_the_agent() {
    // Nothing lists `claude-opus-5-5`, so the query matches no row and Enter takes
    // the query itself. The effort is picked from the list by filtering to it.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.answer(AGENT_PICKER, "\r");
    session.answer_typed(MODEL_PICKER, "claude-opus-5-5");
    session.answer_typed(EFFORT_PICKER, "max");
    session.expect("(model claude-opus-5-5, effort max)");
    session.expect(BANNER);
    session.send_line("go");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &claude_session("--model claude-opus-5-5 --effort max ", "go")
    );
}

#[test]
fn the_last_choice_is_the_first_row_of_the_next_launch() {
    // One world, two launches: the second takes the first row of each picker and
    // gets what the first launch chose, because the cache remembered it.
    let world = World::with(&["--warm"]);
    let mut first = PtyAid::spawn(&world, &[MAIN], &[]);
    first.answer(AGENT_PICKER, "\r");
    first.answer_typed(MODEL_PICKER, "sonnet");
    first.answer_typed(EFFORT_PICKER, "low");
    first.expect(BANNER);
    first.send_line("one");
    assert_eq!(first.wait(), 0);

    let mut second = PtyAid::spawn(&world, &[MAIN], &[]);
    second.reach_the_editor();
    second.send_line("two");
    assert_eq!(second.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &claude_session("--model sonnet --effort low ", "two")
    );
}

#[test]
fn a_setting_a_flag_gave_is_not_asked_for() {
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &["--model", "opus", MAIN], &[]);
    session.answer(AGENT_PICKER, "\r");
    session.answer(EFFORT_PICKER, "\r");
    session.expect(BANNER);
    assert!(
        !session.text().contains(MODEL_PICKER),
        "the model picker was drawn although --model gave the model"
    );
    session.send_line("go");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &claude_session("--model opus ", "go")
    );
}

#[test]
fn with_no_terminal_type_there_is_no_picker_and_the_editor_still_opens() {
    // skim cannot draw without a `TERM`, and used to panic on one that was not
    // set. The editor needs none, so the launch goes on with the defaults.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[("TERM", "")]);
    session.expect(BANNER);
    assert!(!session.text().contains(MODEL_PICKER), "{}", session.text());
    session.send_line("go");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &claude_session("", "go")
    );
}

#[test]
fn a_named_claude_login_is_a_row_of_the_agent_picker_and_reaches_dl() {
    // A host with one named profile holding a credential gets a claude row for it
    // in the agent picker. The row is found by filtering to its name, as a person
    // would.
    let world = World::with(&["--warm"]);
    let profiles = world.root.join("profiles");
    std::fs::create_dir_all(profiles.join("work")).expect("a profile directory");
    std::fs::write(profiles.join("work/.credentials.json"), "{}").expect("a credential");
    let mut session = PtyAid::spawn(
        &world,
        &[MAIN],
        &[(
            "DEVLAUNCH_CLAUDE_PROFILES_DIR",
            &profiles.display().to_string(),
        )],
    );
    session.answer_typed(AGENT_PICKER, "work");
    session.answer(MODEL_PICKER, "\r");
    session.answer(EFFORT_PICKER, "\r");
    session.expect(BANNER);
    session.expect("(account work)");
    session.send_line("go");
    session.expect(&format!("aid -> dl --claude-profile work {MAIN} --"));
    session.wait();
}

#[test]
fn another_agent_is_a_row_of_the_same_picker() {
    // codex is chosen by name, and its own pickers follow. Remote Control was only
    // claude's default, so codex starts without it and nothing refuses.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.answer_typed(AGENT_PICKER, "codex");
    session.answer(MODEL_PICKER.replace("claude", "codex").as_str(), "\r");
    session.answer_typed(EFFORT_PICKER.replace("claude", "codex").as_str(), "high");
    session.expect(BANNER);
    session.send_line("hi");
    assert_eq!(session.wait(), 0);
    let last = world.devpod_calls().last().expect("a session").clone();
    assert!(
        last.contains(
            "codex --dangerously-bypass-approvals-and-sandbox -c model_reasoning_effort=high hi"
        ),
        "{last}"
    );
}

#[test]
fn a_typed_agent_with_one_login_is_not_asked_which_agent() {
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &["--claude", MAIN], &[]);
    session.answer(MODEL_PICKER, "\r");
    session.answer(EFFORT_PICKER, "\r");
    session.expect(BANNER);
    assert!(!session.text().contains(AGENT_PICKER), "{}", session.text());
    session.send_line("");
    assert_eq!(session.wait(), 0);
}

#[test]
fn esc_in_a_picker_stops_the_boot_and_launches_nothing() {
    // The same world as the Ctrl-C test above: an `up` that blocks with the token
    // staged. In a picker Ctrl-C and Esc are keys, not signals, so nothing reaches
    // the boot unless aid sends it. The cleanup is the proof that it did.
    let world = World::with(&["--gh"]);
    let devpod = world.root.join("bin/devpod");
    let original = std::fs::read_to_string(&devpod).expect("the scenario's devpod");
    let delegate = original
        .lines()
        .find(|line| line.starts_with("exec "))
        .expect("the delegate exec line");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"up\" ]; then\n\
         \x20 echo \"$$\" > \"$DL_UP_PID\"\n\
         \x20 : > \"$DL_UP_STARTED\"\n\
         \x20 exec sleep 30\n\
         fi\n\
         {delegate}\n"
    );
    std::fs::write(&devpod, script).expect("rewrite devpod");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&devpod, std::fs::Permissions::from_mode(0o755))
        .expect("keep devpod executable");
    let tmpdir = world.root.join("tmp");
    std::fs::create_dir_all(&tmpdir).expect("a scratch TMPDIR");
    let up_pid = world.root.join("up.pid");
    let up_started = world.root.join("up.started");

    let mut session = PtyAid::spawn(
        &world,
        &["blooop/devlaunch@cold"],
        &[
            ("TMPDIR", &tmpdir.display().to_string()),
            ("DL_UP_PID", &up_pid.display().to_string()),
            ("DL_UP_STARTED", &up_started.display().to_string()),
        ],
    );
    session.answer(AGENT_PICKER, "\r");
    session.answer(MODEL_PICKER, "");
    assert!(
        wait_for(|| up_started.exists() && token_file(&tmpdir).is_some()),
        "devpod up never blocked with a token staged"
    );
    let up = std::fs::read_to_string(&up_pid).expect("the up pid");
    let up = up.trim().to_owned();

    session.press("\x1b");
    session.expect("cancelled");
    let seen = Arc::clone(&session.seen);
    assert_eq!(session.wait(), 130);
    assert!(
        !String::from_utf8_lossy(&seen.lock().expect("the pty buffer")).contains(BANNER),
        "the editor opened after a cancel"
    );
    assert!(
        token_file(&tmpdir).is_none(),
        "the token file must be gone once aid has waited the boot out"
    );
    assert!(
        wait_for(|| !Command::new("kill")
            .args(["-0", &up])
            .output()
            .expect("kill is installed")
            .status
            .success()),
        "the devpod up (pid {up}) must have been killed"
    );
    assert!(
        !world
            .devpod_calls()
            .iter()
            .any(|call| call.starts_with("devpod ssh")),
        "a session was opened after a cancel: {:?}",
        world.devpod_calls()
    );
}

// ===========================================================================
// the prompt editor
// ===========================================================================

/// A bracketed paste of `text`, as a terminal sends one.
fn pasted(text: &str) -> String {
    format!("\x1b[200~{text}\x1b[201~")
}

#[test]
fn a_pasted_prompt_keeps_its_line_breaks_and_waits_for_enter() {
    // The paste ends in a line break, as a copied block of text often does. Under
    // the cooked read that break submitted the prompt; here it is text, and the
    // prompt is sent only on the Enter after it. Windows line ends are one break.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    session.press(&pasted("first line\r\nsecond line\r\n"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !session.text().contains("aid -> dl"),
        "the paste's own line break submitted the prompt; devpod was asked for {:?}",
        world.devpod_calls()
    );
    session.press("\r");
    assert_eq!(session.wait(), 0);
    let last = world.devpod_calls().last().expect("a session").clone();
    assert!(last.contains("first line\nsecond line'"), "{last}");
}

#[test]
fn a_paste_longer_than_the_kernels_line_limit_arrives_whole() {
    // The cooked read went through the line discipline, which holds 4096 bytes of
    // one line and drops the rest. Twice that, in pieces, to be sure.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    let long = "y".repeat(9000);
    session.press("\x1b[200~");
    for chunk in long.as_bytes().chunks(3000) {
        session.press(std::str::from_utf8(chunk).expect("ascii"));
        std::thread::sleep(Duration::from_millis(50));
    }
    session.press("\x1b[201~");
    std::thread::sleep(Duration::from_millis(200));
    session.press("\r");
    assert_eq!(session.wait(), 0);
    let last = world.devpod_calls().last().expect("a session").clone();
    assert!(
        last.contains(&format!(" {long}'")),
        "{} bytes long",
        last.len()
    );
}

#[test]
fn a_paste_that_arrives_late_does_not_leak_into_the_agent() {
    // The second half of the paste comes after a pause. The cooked read took what
    // was queued at the first line break and left the rest for the agent session
    // as keystrokes; the editor waits for the end of the paste.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    session.press("\x1b[200~part one\r");
    std::thread::sleep(Duration::from_millis(300));
    session.press("part two\x1b[201~");
    std::thread::sleep(Duration::from_millis(200));
    session.press("\r");
    assert_eq!(session.wait(), 0);
    let last = world.devpod_calls().last().expect("a session").clone();
    assert!(last.contains("part one\npart two'"), "{last}");
}

#[test]
fn a_line_is_added_by_typing_alt_enter_or_ctrl_j() {
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[MAIN], &[]);
    session.reach_the_editor();
    session.press("one\x1b\r");
    std::thread::sleep(Duration::from_millis(100));
    session.press("two\n");
    std::thread::sleep(Duration::from_millis(100));
    session.press("three");
    std::thread::sleep(Duration::from_millis(100));
    session.press("\r");
    assert_eq!(session.wait(), 0);
    let last = world.devpod_calls().last().expect("a session").clone();
    assert!(last.contains("one\ntwo\nthree'"), "{last}");
}

// ===========================================================================
// no workspace named
// ===========================================================================

/// Words in the header of dl's workspace picker.
const WORKSPACE_PICKER: &str = "Select workspace";

#[test]
fn a_bare_aid_picks_a_workspace_as_a_bare_dl_does() {
    // The scenario's one workspace is the only row, so Enter takes it, and the
    // launch that follows is the ordinary one for that workspace.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[], &[]);
    session.answer(WORKSPACE_PICKER, "\r");
    session.expect(&format!("-> {MAIN}"));
    session.reach_the_editor();
    session.send_line("go");
    assert_eq!(session.wait(), 0);
    assert_eq!(
        world.devpod_calls().last().expect("a session"),
        &claude_session("", "go")
    );
}

#[test]
fn leading_flags_with_no_workspace_pick_one_and_keep_the_flags() {
    // `--codex` still chooses the agent, so there is no agent picker after the
    // workspace one: the typed flag already said.
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &["--codex"], &[]);
    session.answer(WORKSPACE_PICKER, "\r");
    session.answer("Model for codex", "\r");
    session.answer("Effort for codex", "\r");
    session.expect(BANNER);
    assert!(!session.text().contains(AGENT_PICKER), "{}", session.text());
    session.send_line("hi");
    assert_eq!(session.wait(), 0);
    let last = world.devpod_calls().last().expect("a session").clone();
    assert!(
        last.contains("codex --dangerously-bypass-approvals-and-sandbox hi"),
        "{last}"
    );
}

#[test]
fn a_quit_workspace_picker_launches_nothing() {
    let world = World::with(&["--warm"]);
    let mut session = PtyAid::spawn(&world, &[], &[]);
    session.answer(WORKSPACE_PICKER, "\x1b");
    assert_eq!(session.wait(), 1);
    assert!(
        world.devpod_calls().is_empty(),
        "{:?}",
        world.devpod_calls()
    );
}
