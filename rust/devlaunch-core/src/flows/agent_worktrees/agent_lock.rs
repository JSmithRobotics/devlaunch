//! Whether the Claude agent that locked a worktree is still running.
//!
//! # The one lock that can be proved stale
//!
//! Claude Code locks every worktree it makes for a subagent, with a reason of
//! its own shape: `claude agent <id> (pid <N> start <T>)`. `N` is the agent
//! process's pid and `T` its start time, both as the process saw them. Read out
//! of Claude Code 2.1.289's own source: on Linux `T` is field 22 of
//! `/proc/<N>/stat`, the start time in clock ticks since boot. Claude Code
//! reads the same two numbers back to decide whether a lock it finds is stale,
//! and this module asks the same question.
//!
//! A pid alone proves nothing, because pids are reused and because a pid is a
//! number in one pid namespace. So the answer is "gone" only when **every**
//! namespace that could have written the lock says the process is gone:
//!
//! - **Which namespaces.** The one this pass runs in, when the path git
//!   recorded resolves here to the site itself, and only when that namespace is
//!   the whole machine's (see below). And every container whose own mounts put
//!   the recorded path on the site: the mount that covers the path inside the
//!   container is found (the longest destination that is a prefix of it), and
//!   its source, joined with the rest of the path, has to resolve to the site.
//!   A running container whose source is not on this machine any more is asked
//!   by device and inode instead, because a bind mount follows a renamed
//!   directory. A stopped container whose mapping cannot be worked out is
//!   ignored: it has no process to veto with. A recorded path that no namespace
//!   maps to the site is a "could not tell".
//! - **Gone, in one namespace.** A container that is not running (exited,
//!   created, dead) has no processes left. Otherwise the process table is read
//!   (the host's `/proc`, or the container's through `docker exec` as root),
//!   and the agent is still running when any process in it has start time `T`
//!   and pid `N`, either in that namespace or as the innermost of its `NSpid`
//!   entries, which is how a process in a nested container is matched. No such
//!   process is gone. A pid `N` with another start time is a reused pid, and is
//!   gone too.
//! - **The whole machine, first, when this pass can see it.** A procfs that
//!   lists a kernel thread is the initial pid namespace's, and lists every
//!   process on the machine with its `NSpid`. When this pass has one, the same
//!   test runs over all of it before anything else, so an agent in a namespace
//!   the mounts did not map (another docker, a renamed clone's old path) still
//!   stands the site. It only ever adds a refusal.
//! - **Everything else is "could not tell".** A reason in any other shape, a
//!   docker that is missing, refuses or times out, a mount source this machine
//!   cannot read, a process table that does not list pid 1 (no procfs, or one
//!   mounted with `hidepid`), a host-side lock read from inside a pid namespace,
//!   a line that does not parse, a process with start time `T` whose `NSpid`
//!   could not be read, and a container that is paused or restarting.
//!
//! The lock file is never touched here. `dl <ws> rm` removes the whole clone,
//! and a site whose lock is proved stale still has to pass every other question
//! (clean, nothing unpushed) before anything clears.

use std::cell::OnceCell;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::clients::docker;
use crate::domain::workspace_state::NonEmpty;
use crate::runner::Runner;

/// A lock reason in Claude Code's own shape, read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentLock {
    pid: u32,
    start: u64,
}

/// The reason's prefix. Only an agent's lock: a `claude session` lock is a
/// person's `claude --worktree`, and it is left a claim like any other.
const AGENT_PREFIX: &str = "claude agent ";

/// Read `reason` as Claude Code's agent lock, or nothing when it is not exactly
/// that shape. Strict on purpose: a reason this does not recognise stays a
/// claim nobody can interrogate.
pub(crate) fn parse(reason: &str) -> Option<AgentLock> {
    let rest = reason.strip_prefix(AGENT_PREFIX)?;
    let (id, rest) = rest.split_once(" (pid ")?;
    let id_is_a_name = !id.is_empty()
        && id.len() <= 255
        && id
            .chars()
            .all(|it| it.is_ascii_alphanumeric() || it == '-' || it == '_');
    if !id_is_a_name {
        return None;
    }
    let (pid, start) = rest.strip_suffix(')')?.split_once(" start ")?;
    let pid: u32 = digits(pid, 10)?.parse().ok().filter(|pid| *pid > 0)?;
    let start: u64 = digits(start, 20)?.parse().ok()?;
    Some(AgentLock { pid, start })
}

fn digits(text: &str, at_most: usize) -> Option<&str> {
    (!text.is_empty() && text.len() <= at_most && text.bytes().all(|it| it.is_ascii_digit()))
        .then_some(text)
}

/// What became of a lock's agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Owner {
    /// Every namespace that could have written the lock says the process is
    /// gone.
    Gone,
    /// A process with the lock's pid and start time is running, in the place
    /// these words name.
    StillRunning(String),
    /// Why devlaunch could not tell.
    CouldNotTell(String),
}

/// The pid every process table has to list before a missing pid means
/// anything: init in a container, and init on a host. A table without it is
/// no procfs at all, or one that hides other users' processes.
const CONTROL_PID: u32 = 1;

/// Asks where a lock's agent could be running, and whether it is.
///
/// One per clone's weighing, so the container listing is read at most once for
/// all the sites in one clone, and never carried into a later pass.
pub(crate) struct Owners<'r> {
    runner: &'r dyn Runner,
    /// The host's procfs. Not configurable outside tests.
    proc_root: PathBuf,
    containers: OnceCell<Result<Vec<Container>, String>>,
}

impl<'r> Owners<'r> {
    pub(crate) fn new(runner: &'r dyn Runner) -> Self {
        Self::reading(runner, PathBuf::from("/proc"))
    }

    pub(crate) fn reading(runner: &'r dyn Runner, proc_root: PathBuf) -> Self {
        Self {
            runner,
            proc_root,
            containers: OnceCell::new(),
        }
    }

    /// Whether the agent that wrote `lock` on the worktree at `site` (on this
    /// machine) is still running. `recorded` is the path git recorded for it.
    pub(crate) fn owner(&self, site: &Path, recorded: &Path, lock: &AgentLock) -> Owner {
        // First the whole machine, when this pass can see all of it. It finds
        // an agent in any namespace, the ones the mounts below do not map
        // included, and it can only add a refusal.
        let whole_machine = host_table(&self.proc_root)
            .ok()
            .filter(|table| the_whole_machine(table));
        if let Some(table) = &whole_machine {
            match decide(table, lock) {
                InOne::Gone => {}
                InOne::Running => {
                    return Owner::StillRunning(format!(
                        "on this machine (pid {} here, start {})",
                        running_as(table, lock).unwrap_or(lock.pid),
                        lock.start
                    ));
                }
                InOne::CouldNotTell(why) => return Owner::CouldNotTell(why),
            }
        }
        // The whole machine saying gone is not enough on its own: the
        // namespaces below are what says the lock was written on this machine
        // at all, and each is still asked.
        match self.places(site, recorded) {
            Err(why) => Owner::CouldNotTell(why),
            Ok(places) => {
                let mut unsure = None;
                for place in places.iter() {
                    match self.in_place(place, lock) {
                        InOne::Gone => {}
                        InOne::Running => {
                            return Owner::StillRunning(format!(
                                "{} (pid {})",
                                place.describe(),
                                lock.pid
                            ));
                        }
                        InOne::CouldNotTell(why) => {
                            unsure.get_or_insert(why);
                        }
                    }
                }
                match unsure {
                    Some(why) => Owner::CouldNotTell(why),
                    None => Owner::Gone,
                }
            }
        }
    }

    /// Every namespace that sees the site at the recorded path, never empty.
    fn places(&self, site: &Path, recorded: &Path) -> Result<NonEmpty<Place<'_>>, String> {
        if !recorded.is_absolute() {
            return Err(format!(
                "git recorded a relative path for it ({})",
                recorded.display()
            ));
        }
        let site = std::fs::canonicalize(site)
            .map_err(|error| format!("could not resolve {}: {error}", site.display()))?;
        let mut places = Vec::new();
        if resolves_to(recorded, &site)? {
            places.push(Place::Here);
        }
        let containers = self
            .containers
            .get_or_init(|| containers(self.runner))
            .as_ref()
            .map_err(Clone::clone)?;
        for container in containers {
            let sees = match container.sees(recorded, &site) {
                Ok(Sees::Yes) => true,
                Ok(Sees::No) => false,
                // A stopped container has no process left to veto with, so
                // what it would have seen does not matter: it is ignored
                // rather than allowed to block the answer. Exited containers
                // whose clone was deleted since are common.
                Err(_) | Ok(Sees::SourceIsGone) if container.stopped() => false,
                Ok(Sees::SourceIsGone) if container.status == "running" => {
                    self.asked_inside(container, recorded, &site)?
                }
                Ok(Sees::SourceIsGone) => {
                    return Err(format!(
                        "container {} is {} and mounts a directory that is not here any more",
                        container.name, container.status
                    ));
                }
                Err(why) => return Err(why),
            };
            if sees {
                places.push(Place::Container(container));
            }
        }
        NonEmpty::of(places).ok_or_else(|| {
            format!(
                "nothing on this machine sees it at the path git recorded ({})",
                recorded.display()
            )
        })
    }

    /// Whether `recorded`, inside a running container whose mount source is no
    /// longer here, is the site, asked of the container by device and inode.
    ///
    /// A bind mount follows the directory, not its name: a clone renamed while
    /// its container runs is still mounted there. So a source that is not here
    /// any more proves nothing either way, and the container itself is asked.
    fn asked_inside(
        &self,
        container: &Container,
        recorded: &Path,
        site: &Path,
    ) -> Result<bool, String> {
        let here = std::fs::metadata(site)
            .map_err(|error| format!("could not read {}: {error}", site.display()))?;
        let recorded = recorded.to_string_lossy();
        let out = docker::exec_as_root(
            self.runner,
            &container.id,
            &["sh", "-c", IS_IT_THE_SITE, "sh", &recorded],
        )
        .map_err(|why| format!("could not ask container {}: {why}", container.name))?;
        match out.trim() {
            "devlaunch-absent" => Ok(false),
            answer => {
                let identity = answer
                    .split_once(':')
                    .and_then(|(dev, ino)| {
                        Some((dev.parse::<u64>().ok()?, ino.parse::<u64>().ok()?))
                    })
                    .ok_or_else(|| format!("container {} answered {answer:?}", container.name))?;
                Ok(identity == (here.dev(), here.ino()))
            }
        }
    }

    fn in_place(&self, place: &Place<'_>, lock: &AgentLock) -> InOne {
        match place {
            Place::Here => match host_table(&self.proc_root) {
                Ok(table) if the_whole_machine(&table) => decide(&table, lock),
                Ok(_) => InOne::CouldNotTell(
                    "this pass runs in a pid namespace that is not the whole machine's, so a \
                     process outside it would read as gone"
                        .to_owned(),
                ),
                Err(why) => InOne::CouldNotTell(why),
            },
            Place::Container(container) => match container.status.as_str() {
                "running" => match docker::exec_as_root(
                    self.runner,
                    &container.id,
                    &["sh", "-c", READ_THE_TABLE],
                )
                .and_then(|out| container_table(&out))
                {
                    Ok(table) => decide(&table, lock),
                    Err(why) => InOne::CouldNotTell(format!(
                        "could not read the processes in container {}: {why}",
                        container.name
                    )),
                },
                _ if container.stopped() => InOne::Gone,
                other => InOne::CouldNotTell(format!("container {} is {other}", container.name)),
            },
        }
    }
}

/// A namespace a lock could have been written in.
enum Place<'c> {
    /// The one this pass runs in.
    Here,
    Container(&'c Container),
}

impl Place<'_> {
    fn describe(&self) -> String {
        match self {
            Self::Here => "on this machine".to_owned(),
            Self::Container(container) => format!("in container {}", container.name),
        }
    }
}

/// Whether `path` resolves here to `site`, which is already canonical. Nothing
/// at `path` is "no": the site exists, so a path to it would resolve.
fn resolves_to(path: &Path, site: &Path) -> Result<bool, String> {
    match std::fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved == site),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Ok(false)
        }
        Err(error) => Err(format!("could not resolve {}: {error}", path.display())),
    }
}

// ===========================================================================
// containers
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
struct Container {
    id: String,
    /// What a report calls it.
    name: String,
    /// docker's `State.Status`.
    status: String,
    mounts: Vec<Mount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    /// The directory on this machine. Empty for a tmpfs.
    source: String,
    destination: PathBuf,
}

/// What a container's mounts say about the recorded path.
enum Sees {
    Yes,
    No,
    /// The mount that covers the path names a source that is not on this
    /// machine any more.
    SourceIsGone,
}

impl Container {
    /// Exited, created or dead: no process is left in it.
    fn stopped(&self) -> bool {
        matches!(self.status.as_str(), "exited" | "created" | "dead")
    }

    /// Whether `recorded`, read inside this container, is the site.
    ///
    /// The mount with the longest destination covering the path is the one the
    /// path lands in, so a volume or a tmpfs mounted deeper than the clone
    /// shadows it. A source this machine cannot read is "could not tell": the
    /// path may well land on the site, and nothing here can say it does not.
    fn sees(&self, recorded: &Path, site: &Path) -> Result<Sees, String> {
        let Some(mount) = self
            .mounts
            .iter()
            .filter(|mount| recorded.starts_with(&mount.destination))
            .max_by_key(|mount| mount.destination.components().count())
        else {
            return Ok(Sees::No);
        };
        if mount.source.is_empty() {
            return Ok(Sees::No);
        }
        let rest = recorded
            .strip_prefix(&mount.destination)
            .expect("the filter kept only mounts the path starts with");
        let source = match std::fs::canonicalize(&mount.source) {
            Ok(source) => source,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Sees::SourceIsGone),
            Err(error) => {
                return Err(format!(
                    "could not see {}, which container {} mounts at {}: {error}",
                    mount.source,
                    self.name,
                    mount.destination.display()
                ));
            }
        };
        let mapped = source.join(rest);
        if mapped == site || resolves_to(&mapped, site)? {
            Ok(Sees::Yes)
        } else {
            Ok(Sees::No)
        }
    }
}

/// Every container docker knows, described.
fn containers(runner: &dyn Runner) -> Result<Vec<Container>, String> {
    let ids = docker::every_container(runner)?;
    let Some(ids) = NonEmpty::of(ids) else {
        return Ok(Vec::new());
    };
    parse_inspect(&docker::inspect(runner, &ids)?)
}

/// `docker inspect`'s array, read for the fields this module needs. A
/// container missing any of them fails the whole read, rather than dropping a
/// container that might be the one that matters.
fn parse_inspect(json: &str) -> Result<Vec<Container>, String> {
    let bad = |what: &str| format!("docker inspect printed {what}");
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| bad(&format!("something unreadable: {error}")))?;
    let array = value.as_array().ok_or_else(|| bad("no array"))?;
    array
        .iter()
        .map(|item| {
            let text = |field: &serde_json::Value, name: &str| {
                field
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad(&format!("a container with no {name}")))
            };
            let id = text(&item["Id"], "Id")?;
            let name = text(&item["Name"], "Name")?
                .trim_start_matches('/')
                .to_owned();
            let status = text(&item["State"]["Status"], "State.Status")?;
            let mounts = item["Mounts"]
                .as_array()
                .ok_or_else(|| bad("a container with no Mounts"))?
                .iter()
                .map(|mount| {
                    Ok(Mount {
                        source: mount["Source"].as_str().unwrap_or_default().to_owned(),
                        destination: PathBuf::from(text(&mount["Destination"], "Destination")?),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(Container {
                id,
                name,
                status,
                mounts,
            })
        })
        .collect()
}

// ===========================================================================
// process tables
// ===========================================================================

/// One process: its pid where the table was read, its start time, and its
/// `NSpid` entries when they could be read (outermost first, innermost last).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Process {
    pid: u32,
    kernel_thread: bool,
    start: u64,
    nspid: Option<Vec<u32>>,
}

/// What one namespace says about the lock's agent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InOne {
    Gone,
    Running,
    CouldNotTell(String),
}

/// The whole decision for one table. See the module header.
fn decide(table: &[Process], lock: &AgentLock) -> InOne {
    if !table.iter().any(|process| process.pid == CONTROL_PID) {
        return InOne::CouldNotTell(format!(
            "the process table did not list pid {CONTROL_PID}, so a missing pid proves nothing"
        ));
    }
    let mut unsure = false;
    for process in table.iter().filter(|process| process.start == lock.start) {
        let innermost = process.nspid.as_ref().and_then(|nspid| nspid.last());
        if process.pid == lock.pid || innermost == Some(&lock.pid) {
            return InOne::Running;
        }
        if innermost.is_none() {
            unsure = true;
        }
    }
    if unsure {
        InOne::CouldNotTell(
            "a process started at the same moment, and its pid in its own namespace could not \
             be read"
                .to_owned(),
        )
    } else {
        InOne::Gone
    }
}

/// Whether `table` is the initial pid namespace's, which lists every process
/// on the machine. Kernel threads live in that namespace and no other, so one
/// is in the table exactly then. Not a parent of 0: a process `docker exec`
/// starts inside a container has its parent outside the namespace, and reads
/// as parent 0 there too.
fn the_whole_machine(table: &[Process]) -> bool {
    table.iter().any(|process| process.kernel_thread)
}

/// `PF_KTHREAD`, in the flags field of `/proc/<pid>/stat`. Set by the kernel on
/// its own threads, and nothing in user space can set it.
const PF_KTHREAD: u64 = 0x0020_0000;

/// The pid, in the table's own namespace, of the process that matched.
fn running_as(table: &[Process], lock: &AgentLock) -> Option<u32> {
    table
        .iter()
        .find(|process| {
            process.start == lock.start
                && (process.pid == lock.pid
                    || process.nspid.as_ref().and_then(|it| it.last()) == Some(&lock.pid))
        })
        .map(|process| process.pid)
}

/// The fields this module reads out of one `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatFields {
    pid: u32,
    kernel_thread: bool,
    start: u64,
}

/// The pid, whether it is a kernel thread, and the start time out of one
/// `/proc/<pid>/stat`.
///
/// The command name sits in parentheses and may hold anything, spaces and
/// parentheses included, so the fields are counted from the *last* `)`. The
/// flags are field 9, the 7th after the name, and the start time field 22, the
/// 20th.
fn stat_fields(stat: &str) -> Option<StatFields> {
    let (head, _) = stat.split_once(" (")?;
    let pid = digits(head, 10)?.parse().ok()?;
    let after: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    let flags: u64 = digits(after.get(6)?, 20)?.parse().ok()?;
    Some(StatFields {
        pid,
        kernel_thread: flags & PF_KTHREAD != 0,
        start: digits(after.get(19)?, 20)?.parse().ok()?,
    })
}

/// The pids out of a `NSpid:` line of `/proc/<pid>/status`.
fn nspid_entries(line: &str) -> Option<Vec<u32>> {
    let rest = line.strip_prefix("NSpid:")?;
    let pids: Option<Vec<u32>> = rest
        .split_whitespace()
        .map(|pid| digits(pid, 10)?.parse().ok())
        .collect();
    pids.filter(|pids| !pids.is_empty())
}

/// The host's process table, read straight out of `proc_root`.
fn host_table(proc_root: &Path) -> Result<Vec<Process>, String> {
    let entries = std::fs::read_dir(proc_root)
        .map_err(|error| format!("could not read {}: {error}", proc_root.display()))?;
    let mut table = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str().and_then(|name| digits(name, 10)) else {
            continue;
        };
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            // Ended between the listing and the read.
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("could not read /proc/{name}/stat: {error}")),
        };
        let StatFields {
            pid,
            kernel_thread,
            start,
        } = stat_fields(&stat).ok_or_else(|| format!("could not read /proc/{name}/stat"))?;
        let nspid = std::fs::read_to_string(entry.path().join("status"))
            .ok()
            .and_then(|status| status.lines().find_map(nspid_entries));
        table.push(Process {
            pid,
            kernel_thread,
            start,
            nspid,
        });
    }
    Ok(table)
}

/// What runs inside a container to print its process table: every stat file,
/// a marker, every `NSpid` line, and a closing marker that says the script ran
/// to its end. Three programs every image `dl` opens carries: `sh`, `cat` and
/// `grep`. An image without them fails the read, which is "could not tell".
const READ_THE_TABLE: &str = "cat /proc/[0-9]*/stat 2>/dev/null; echo devlaunch-nspid; \
     grep -H '^NSpid:' /proc/[0-9]*/status 2>/dev/null; echo devlaunch-end";

/// What runs inside a container to say what `$1` is: its device and inode, or
/// that nothing is there.
const IS_IT_THE_SITE: &str =
    "if [ -e \"$1\" ]; then stat -L -c %d:%i -- \"$1\"; else echo devlaunch-absent; fi";

/// Read what [`READ_THE_TABLE`] printed.
fn container_table(out: &str) -> Result<Vec<Process>, String> {
    let (stats, rest) = out
        .split_once("devlaunch-nspid\n")
        .ok_or("the listing stopped before its middle")?;
    let nspids = rest
        .strip_suffix("devlaunch-end\n")
        .ok_or("the listing stopped before its end")?;
    let mut table = Vec::new();
    for line in stats.lines() {
        let StatFields {
            pid,
            kernel_thread,
            start,
        } = stat_fields(line).ok_or_else(|| format!("a stat line did not parse: {line}"))?;
        table.push(Process {
            pid,
            kernel_thread,
            start,
            nspid: None,
        });
    }
    for line in nspids.lines() {
        // `/proc/<pid>/status:NSpid:\t<pid>\t…`
        let Some((path, entry)) = line.split_once(":NSpid:") else {
            return Err(format!("an NSpid line did not parse: {line}"));
        };
        let pid: Option<u32> = path
            .strip_prefix("/proc/")
            .and_then(|it| it.strip_suffix("/status"))
            .and_then(|it| digits(it, 10))
            .and_then(|it| it.parse().ok());
        let (Some(pid), Some(entries)) = (pid, nspid_entries(&format!("NSpid:{entry}"))) else {
            return Err(format!("an NSpid line did not parse: {line}"));
        };
        if let Some(process) = table.iter_mut().find(|process| process.pid == pid) {
            process.nspid = Some(entries);
        }
    }
    Ok(table)
}

#[cfg(test)]
mod tests;
