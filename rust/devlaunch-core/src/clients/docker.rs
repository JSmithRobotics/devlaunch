//! Everything devlaunch asks `docker`, and everything it reads back.
//!
//! Five calls, and between them they cover four commands. `dl <ws> rm` removes
//! the named volumes a workspace's devcontainer created, once the workspace
//! itself is gone (devlaunch#325). `dl <ws> kill` asks which containers a wedged
//! workspace's compose project still has up, and kills them (devlaunch#484) —
//! the one reading in the module, and the reason [`Running`] exists beside
//! [`Answer`].
//!
//! Nothing here removes an *image* — images are shared, they are expensive to
//! rebuild, and which workspace owns one is genuinely ambiguous, so they stay
//! outside the line `dl --purge` and `dl --prune` both print.
//!
//! # Why this is a module and not three lines in the flow
//!
//! For the reason [`crate::clients::devpod`] is one: **exactly one place may
//! decide that docker is absent.** A host with no docker never created these
//! volumes, so "not installed" is the one answer that must be silent — and folded
//! into an exit status it would read as a docker that ran and refused, which is a
//! sentence about a machine that has nothing to remove. [`Answer`] keeps the two
//! apart, and these are the only spawns that can produce either.

use std::io::ErrorKind;
use std::time::Duration;

use crate::domain::workspace_state::NonEmpty;
use crate::runner::{CapturedText, Exit, Invocation, OsFailure, Outcome, Runner, SpawnSpec};

/// The program every call in this module runs. One constant, so no caller spells
/// it and a test's response table has one name to match on.
pub(crate) const PROGRAM: &str = "docker";

/// What asking one compose project for its running containers may cost before it
/// is abandoned.
///
/// Bounded, unlike the `docker volume rm` above it, and for [`super::ps`]'s
/// reason rather than for tidiness. `dl <ws> kill` reaches both of the calls
/// below *after* it has already SIGKILLed something, and nothing it found is
/// printed until they return: a docker daemon that never answers would mean the
/// hammer landed, the marker went, and the person who typed the verb saw not one
/// line of it (devlaunch#484, "print what it killed"). A bound turns that into a
/// report that says docker would not answer, which is the whole difference.
const LIST_ONE_PROJECT: Duration = Duration::from_secs(5);

/// What killing them may cost. Longer than the listing, because this is a signal
/// per container and the daemon does the work, where the listing is a read.
const KILL_THEM: Duration = Duration::from_secs(10);

/// What capping them may cost.
///
/// The listing's bound rather than the kill's: `docker update` writes a cgroup
/// file per container and returns, where a kill waits on a signal being taken.
/// Bounded at all for [`running_for_project`]'s reason -- this runs on the
/// launch path, after the workspace is already up, so a daemon that never
/// answers must cost a notice and not the session.
const CAP_THEM: Duration = Duration::from_secs(5);

/// What asking docker something came to.
///
/// Three arms rather than an exit status, because two of them are not one, and
/// they are not the same absence:
///
/// - [`Answer::NotInstalled`] is a machine with no docker on it. Nothing to
///   report: a machine with no docker made no volumes.
/// - [`Answer::NotStarted`] is a docker that is there and did not answer — the OS
///   refused the spawn, or the child was killed. That is a fact about this
///   machine, and it carries the errno for the binary to phrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// docker ran to completion, having written this to stderr. stdout is
    /// dropped: `docker volume rm` echoes the names back, which nothing reads.
    Ran { exit: Exit, stderr: String },
    /// docker is not on PATH — or is there and cannot be exec'd, which arrives as
    /// the same ENOENT and points at the same fix.
    NotInstalled,
    /// docker never answered. `failure.kind` is
    /// [`ErrorKind::TimedOut`] where the child was killed, which is the one case
    /// carrying no errno.
    NotStarted(OsFailure),
}

/// Remove these named volumes, counting one that is already gone as removed.
///
/// **One call carrying every name, and `--force` is what makes that safe.**
/// Measured against docker 29.7: `--force` makes an absent volume exit 0, so a
/// repository whose devcontainer never declared one of these names is a silent
/// no-op rather than a complaint on every delete — while a volume some container
/// still holds is *still* refused, which is the one refusal worth reporting. A
/// name-at-a-time loop would buy nothing but round trips and a partial-failure
/// state to model.
///
/// Captured rather than inherited: a successful removal has nothing to say to a
/// user's terminal, and a refusal is a notice this process frames.
pub(crate) fn remove_volumes(runner: &dyn Runner, names: &NonEmpty<String>) -> Answer {
    let mut args = vec!["volume".to_owned(), "rm".to_owned(), "--force".to_owned()];
    args.extend(names.iter().cloned());
    let spec = SpawnSpec::from(Invocation::new(PROGRAM).with_args(args));
    answer(runner.capture(&spec))
}

/// The containers one compose project has running, or why docker could not say.
///
/// Its own type rather than an [`Answer`] carrying stdout, because this is the
/// one call in the module whose *output* is the answer: everything else here is
/// a removal, and `Answer` drops stdout on purpose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Running {
    /// The ids docker listed, in its order. Empty is a project with nothing up,
    /// which is the ordinary case rather than a failure.
    These(Vec<String>),
    /// No docker on PATH, or one that cannot be exec'd.
    NotInstalled,
    /// docker ran and would not answer, having written this to stderr.
    Refused { exit: Exit, stderr: String },
    /// docker never answered.
    NotStarted(OsFailure),
}

/// The containers docker has running for one compose project.
///
/// **The project name is the workspace id**, because devpod names the compose
/// project itself and derives it from the id — it does not honour a
/// `COMPOSE_PROJECT_NAME` from the project's own `.env`. That is devpod's
/// convention rather than devlaunch's, and `docs/devcontainer-projects.md`
/// carries the rest of what follows from it.
///
/// **The honest limit**: a single-container devcontainer is in no compose project
/// at all, so this finds nothing for one. That is not a hole in the sweep so much
/// as a statement about what the sweep is for — the container is not what blocks
/// a launch, the flock is, and killing the process that holds it is what unwedges
/// the workspace either way.
pub(crate) fn running_for_project(runner: &dyn Runner, project: &str) -> Running {
    let spec = SpawnSpec::from(Invocation::new(PROGRAM).with_args([
        "ps",
        "--quiet",
        "--no-trunc",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
    ]))
    .with_timeout(LIST_ONE_PROJECT);
    listed(runner.capture(&spec))
}

/// Kill these containers now, in one call.
///
/// `kill` rather than `stop`, and that is the verb the whole command is named
/// for: a container whose build was SIGKILLed has nothing left to shut down in an
/// orderly way, and `stop` would spend its ten-second grace period on each one
/// before sending the same signal.
pub(crate) fn kill_containers(runner: &dyn Runner, ids: &NonEmpty<String>) -> Answer {
    let mut args = vec!["kill".to_owned()];
    args.extend(ids.iter().cloned());
    let spec = SpawnSpec::from(Invocation::new(PROGRAM).with_args(args)).with_timeout(KILL_THEM);
    answer(runner.capture(&spec))
}

/// Every container docker has up whose name ends with this workspace id.
///
/// **Matched on the name, where [`running_for_project`] matches on the compose
/// label, and the difference is the point.** The compose label finds the
/// services of a multi-container devcontainer and finds *nothing* for a
/// single-container one, which is in no compose project at all -- an honest
/// limit for `kill`, where the flock is what blocks a launch, and a hole here,
/// where the container is exactly the thing being capped. A name filter covers
/// both, because devpod ends every container name it creates with the id.
///
/// **The name is matched, never built.** `--filter name=` is a substring match,
/// and the prefix in front of the id belongs to the devcontainer rather than to
/// devlaunch: `kinisi_jazzy_<id>` and `kinisi_unreal_runtime_1.5.0_<id>` are two
/// services of one workspace, measured on this host. Constructing the name would
/// mean knowing the service names, which is the devcontainer author's business.
///
/// The substring is an id devpod generated with a random suffix, so a container
/// of some other workspace's cannot contain it by accident.
pub(crate) fn running_for_workspace(runner: &dyn Runner, id: &str) -> Running {
    let spec = SpawnSpec::from(Invocation::new(PROGRAM).with_args([
        "ps",
        "--quiet",
        "--no-trunc",
        "--filter",
        &format!("name={id}"),
    ]))
    .with_timeout(LIST_ONE_PROJECT);
    listed(runner.capture(&spec))
}

/// Hold these containers to `bytes` of memory, swap included.
///
/// `docker update` rather than a flag on the create, because **devpod has no way
/// to pass one**: `devpod up` takes `--mount` and nothing that reaches docker's
/// run arguments (measured against devpod 0.26.1). That turns out to be the
/// better half of the trade rather than a workaround -- a limit set this way
/// lands on a container that is already running, so a cap applies to the
/// workspaces already up on a host instead of only to the ones created after it,
/// and nothing has to be recreated to take it.
///
/// **`--memory-swap` is set equal to `--memory`, which is what makes the number
/// mean what it says.** Left alone, docker defaults swap to twice memory, so a
/// cap of 8G would really be 8G of RAM and 8G of swap: a container could exceed
/// its stated limit by spilling onto a disk every other container on the host
/// shares. Equal values switch the container's swap off, so the cap binds, and
/// it binds at the number the JVM and the cgroup both report -- which is what a
/// build sizing itself from its own limits reads.
///
/// The cost of that is stated rather than hidden: a process that would have
/// survived by swapping is OOM-killed instead, and a cgroup OOM is quieter than
/// the host's -- exit 137 with nothing in the journal naming the compiler.
///
/// One call carrying every container, as [`remove_volumes`] does, and for the
/// same reason: the daemon takes them together and a loop would buy only round
/// trips and a partial-failure state to model.
pub(crate) fn set_memory_cap(
    runner: &dyn Runner,
    containers: &NonEmpty<String>,
    bytes: u64,
) -> Answer {
    let mut args = vec![
        "update".to_owned(),
        "--memory".to_owned(),
        bytes.to_string(),
        "--memory-swap".to_owned(),
        bytes.to_string(),
    ];
    args.extend(containers.iter().cloned());
    let spec = SpawnSpec::from(Invocation::new(PROGRAM).with_args(args)).with_timeout(CAP_THEM);
    answer(runner.capture(&spec))
}

/// A docker spawn that produced nothing to read, in the two shapes both answers
/// in this module report it as.
enum NoAnswer {
    NotInstalled,
    NotStarted(OsFailure),
}

/// The one reading of a `docker ps` every listing in this module shares.
///
/// Factored out when [`running_for_workspace`] joined [`running_for_project`]:
/// the two differ only in the filter they pass, and a second copy of this match
/// is a second chance to lose the distinction between an empty listing and a
/// docker that would not answer -- which is the distinction [`Running`] exists
/// to carry.
fn listed(outcome: Outcome<CapturedText>) -> Running {
    match ran(outcome) {
        Ok((exit, CapturedText { stdout, stderr })) => {
            if exit.is_success() {
                Running::These(
                    stdout
                        .lines()
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .collect(),
                )
            } else {
                Running::Refused { exit, stderr }
            }
        }
        Err(NoAnswer::NotInstalled) => Running::NotInstalled,
        Err(NoAnswer::NotStarted(failure)) => Running::NotStarted(failure),
    }
}

/// The one reading of a docker spawn every call in this module shares.
///
/// `Ok` is a docker that ran, whatever it exited: what its status and its streams
/// mean is the caller's, because a removal and a listing read them differently.
/// `Err` is the three ways of not running collapsed to the two answers that
/// differ, and it is here rather than in each caller because the ENOENT arm is
/// the subtle one — two copies of this match are two chances to lose it, which is
/// exactly what [`running_for_project`] had.
fn ran(outcome: Outcome<CapturedText>) -> Result<(Exit, CapturedText), NoAnswer> {
    match outcome {
        Outcome::Ran { exit, io } => Ok((exit, io)),
        Outcome::ProgramNotFound => Err(NoAnswer::NotInstalled),
        // A bound each caller sets for itself, so this arm is reachable: it must
        // not read as a docker that ran, which for a removal would be a removal
        // that never happened.
        Outcome::TimedOut => Err(NoAnswer::NotStarted(OsFailure {
            kind: ErrorKind::TimedOut,
            errno: None,
        })),
        // A docker found on PATH that could not be exec'd fails with ENOENT at
        // exec time, which arrives here rather than as `ProgramNotFound`. It gets
        // the docker-is-missing answer for the same reason devpod's spawn gives it
        // one: the fix is to install a working docker either way.
        Outcome::NotStarted(failure) if failure.kind == ErrorKind::NotFound => {
            Err(NoAnswer::NotInstalled)
        }
        Outcome::NotStarted(failure) => Err(NoAnswer::NotStarted(failure)),
    }
}

/// What a docker spawn whose stdout nothing reads came to.
fn answer(outcome: Outcome<CapturedText>) -> Answer {
    match ran(outcome) {
        Ok((exit, CapturedText { stderr, .. })) => Answer::Ran { exit, stderr },
        Err(NoAnswer::NotInstalled) => Answer::NotInstalled,
        Err(NoAnswer::NotStarted(failure)) => Answer::NotStarted(failure),
    }
}

#[cfg(test)]
mod tests {
    use devlaunch_test_support::{FakeRunner, Response};

    use super::*;

    /// Asked for by NAME, which is the whole difference from the call above.
    ///
    /// The compose label finds nothing for a single-container devcontainer,
    /// which is in no compose project; a name filter finds it, because devpod
    /// ends every container name with the workspace id.
    #[test]
    fn a_workspaces_containers_are_asked_for_by_name() {
        let fake = FakeRunner::new();

        let _ = running_for_workspace(&fake, "my-ws-3w80");

        assert_eq!(
            fake.argvs(),
            [[
                "docker",
                "ps",
                "--quiet",
                "--no-trunc",
                "--filter",
                "name=my-ws-3w80"
            ]]
        );
    }

    /// One `docker update` for every container, with swap pinned to memory.
    ///
    /// The equal `--memory-swap` is the assertion worth having: without it
    /// docker defaults swap to twice memory, and an 8G cap silently becomes 8G
    /// of RAM plus 8G of disk. The number is in bytes so nothing has to agree
    /// with docker about what a suffix means.
    #[test]
    fn capping_sets_swap_equal_to_memory_for_every_container() {
        let fake = FakeRunner::new();

        set_memory_cap(
            &fake,
            &names(&["kinisi_jazzy_ws", "kinisi_unreal_ws"]),
            8 * 1024 * 1024 * 1024,
        );

        assert_eq!(
            fake.argvs(),
            [[
                "docker",
                "update",
                "--memory",
                "8589934592",
                "--memory-swap",
                "8589934592",
                "kinisi_jazzy_ws",
                "kinisi_unreal_ws"
            ]]
        );
    }

    /// A host with no docker is the silent arm here too.
    ///
    /// Same reasoning as the removals: a machine with no docker has no
    /// container to cap, and reporting that as a failure would be a sentence
    /// about a machine with nothing to do.
    #[test]
    fn a_cap_on_a_host_with_no_docker_is_not_installed_rather_than_a_refusal() {
        let fake = FakeRunner::new();
        fake.script_missing("docker");

        assert_eq!(
            set_memory_cap(&fake, &names(&["ws"]), 1),
            Answer::NotInstalled
        );
    }

    #[test]
    fn a_compose_projects_containers_are_asked_for_by_label() {
        let fake = FakeRunner::new();

        let _ = running_for_project(&fake, "my-ws");

        assert_eq!(
            fake.argvs(),
            [[
                "docker",
                "ps",
                "--quiet",
                "--no-trunc",
                "--filter",
                "label=com.docker.compose.project=my-ws",
            ]]
        );
    }

    /// One id per line, and a project with nothing up prints nothing at all --
    /// which is the ordinary case rather than a failure, since the container is
    /// usually the first thing to die.
    #[test]
    fn the_listing_is_one_container_id_per_line() {
        let fake = FakeRunner::new();
        fake.script(["docker", "ps"], Response::stdout("abc123\ndef456\n"));

        assert_eq!(
            running_for_project(&fake, "my-ws"),
            Running::These(vec!["abc123".to_owned(), "def456".to_owned()])
        );

        let empty = FakeRunner::new();
        empty.script(["docker", "ps"], Response::stdout(""));
        assert_eq!(running_for_project(&empty, "my-ws"), Running::These(vec![]));
    }

    #[test]
    fn a_machine_with_no_docker_lists_no_containers_rather_than_none() {
        let fake = FakeRunner::new();
        fake.script_missing("docker");

        assert_eq!(running_for_project(&fake, "my-ws"), Running::NotInstalled);
    }

    /// `kill`, not `stop`: a container belonging to a workspace whose build was
    /// SIGKILLed has nothing left to shut down gracefully, and `stop` would spend
    /// its ten-second grace on each one before doing this anyway.
    #[test]
    fn every_container_is_killed_in_one_call() {
        let fake = FakeRunner::new();

        kill_containers(&fake, &names(&["abc123", "def456"]));

        assert_eq!(fake.argvs(), [["docker", "kill", "abc123", "def456"]]);
    }

    /// The two calls `dl <ws> kill` makes of docker, and both carry a deadline.
    /// Nothing the sweep found reaches the terminal until they return, so a
    /// daemon that never answers would swallow the report of a SIGKILL that has
    /// already landed — devlaunch#484's "print what it killed", lost to the one
    /// thing the verb has no bound on.
    #[test]
    fn neither_call_the_kill_makes_can_outlive_a_wedged_daemon() {
        let fake = FakeRunner::new();

        let _ = running_for_project(&fake, "my-ws");
        kill_containers(&fake, &names(&["abc123"]));

        assert_eq!(
            fake.calls()
                .iter()
                .map(|call| call.spec().and_then(|spec| spec.timeout))
                .collect::<Vec<Option<Duration>>>(),
            [Some(LIST_ONE_PROJECT), Some(KILL_THEM)]
        );
    }

    /// The volume sweep is the call that keeps no bound, and that is the contrast
    /// the two above are for: `dl <ws> rm` is not running because the host stopped
    /// answering, and a volume docker is slow to release is a wait worth taking.
    #[test]
    fn the_volume_removal_keeps_no_deadline() {
        let fake = FakeRunner::new();

        remove_volumes(&fake, &names(&["ws-pixi"]));

        assert_eq!(fake.only_call().spec().and_then(|spec| spec.timeout), None);
    }

    fn names(items: &[&str]) -> NonEmpty<String> {
        NonEmpty::of(items.iter().map(|name| (*name).to_owned())).expect("at least one name")
    }

    #[test]
    fn every_name_goes_in_one_forced_removal() {
        let fake = FakeRunner::new();

        remove_volumes(&fake, &names(&["ws-pixi", "dind-var-lib-docker-abc"]));

        assert_eq!(
            fake.argvs(),
            [[
                "docker",
                "volume",
                "rm",
                "--force",
                "ws-pixi",
                "dind-var-lib-docker-abc",
            ]]
        );
    }

    #[test]
    fn a_docker_that_refused_answers_with_its_own_words() {
        let fake = FakeRunner::new();
        fake.script(
            ["docker", "volume", "rm"],
            Response::failed(1, "volume is in use\n"),
        );

        assert_eq!(
            remove_volumes(&fake, &names(&["ws-pixi"])),
            Answer::Ran {
                exit: Exit::Code(1),
                stderr: "volume is in use\n".to_owned(),
            }
        );
    }

    #[test]
    fn a_machine_with_no_docker_says_so_rather_than_failing() {
        let fake = FakeRunner::new();
        fake.script_missing("docker");

        assert_eq!(
            remove_volumes(&fake, &names(&["ws-pixi"])),
            Answer::NotInstalled
        );
    }

    /// A docker on PATH that cannot be exec'd reads as a docker that is not there:
    /// the fix is the same, and the alternative is an errno nobody can act on.
    #[test]
    fn a_docker_that_cannot_be_execd_reads_as_no_docker() {
        let fake = FakeRunner::new();
        fake.script(
            ["docker"],
            Response::NotStarted(OsFailure {
                kind: ErrorKind::NotFound,
                errno: Some(2),
            }),
        );

        assert_eq!(
            remove_volumes(&fake, &names(&["ws-pixi"])),
            Answer::NotInstalled
        );
    }

    #[test]
    fn any_other_os_refusal_keeps_its_errno() {
        let fake = FakeRunner::new();
        let failure = OsFailure {
            kind: ErrorKind::PermissionDenied,
            errno: Some(13),
        };
        fake.script(["docker"], Response::NotStarted(failure));

        assert_eq!(
            remove_volumes(&fake, &names(&["ws-pixi"])),
            Answer::NotStarted(failure)
        );
    }
}
