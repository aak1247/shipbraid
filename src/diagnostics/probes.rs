use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::timeout,
};

use crate::{
    config::Config,
    diagnostics::{Check, CheckStatus},
};

const MAX_PROBE_STDOUT: usize = 4 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_CANDIDATES: &[&str] = &["/usr/bin/git", "/bin/git"];
const SYSTEMCTL_CANDIDATES: &[&str] = &["/usr/bin/systemctl", "/bin/systemctl"];
const CGROUP_CONTROLLERS: &str = "/sys/fs/cgroup/cgroup.controllers";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessFailure {
    Spawn,
    Io,
    Timeout,
    OutputLimit,
}

#[derive(Debug)]
struct ProcessOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
}

pub(super) async fn repository_checks(config: &Config) -> Vec<Check> {
    let executable = fixed_executable(GIT_CANDIDATES);
    repository_checks_with_git(config, executable.as_deref()).await
}

async fn repository_checks_with_git(config: &Config, git: Option<&Path>) -> Vec<Check> {
    let mut canonical_paths = Vec::with_capacity(config.lanes.len());
    let mut first_by_repository = BTreeMap::<&str, PathBuf>::new();
    let mut indexes_by_repository = BTreeMap::<&str, Vec<usize>>::new();
    let mut inconsistent_repository_ids = BTreeSet::new();

    for (index, lane) in config.lanes.iter().enumerate() {
        let canonical = lane
            .repository_path
            .canonicalize()
            .ok()
            .filter(|path| path.is_dir());
        if let Some(path) = canonical.as_ref() {
            let repository_id = lane.repository_id.as_str();
            indexes_by_repository
                .entry(repository_id)
                .or_default()
                .push(index);
            if let Some(first_path) = first_by_repository.get(repository_id) {
                if first_path != path {
                    inconsistent_repository_ids.insert(repository_id);
                }
            } else {
                first_by_repository.insert(repository_id, path.clone());
            }
        }
        canonical_paths.push(canonical);
    }
    let inconsistent_lanes = indexes_by_repository
        .iter()
        .filter(|(repository_id, _)| inconsistent_repository_ids.contains(*repository_id))
        .flat_map(|(_, indexes)| indexes.iter().copied())
        .collect::<BTreeSet<_>>();

    let mut checks = Vec::with_capacity(config.lanes.len() * 5);
    for (index, lane) in config.lanes.iter().enumerate() {
        let path_field = format!("lanes[{index}].repository_path");
        let ref_field = format!("lanes[{index}].release_ref");
        let Some(repository) = canonical_paths[index].as_deref() else {
            checks.push(Check::new(
                "repository.path",
                CheckStatus::Fail,
                path_field.clone(),
                "The configured repository directory is unavailable.",
                "Check the configured path and local directory permissions.",
            ));
            checks.push(Check::new(
                "repository.mapping",
                CheckStatus::Skipped,
                path_field.clone(),
                "Repository ID mapping could not be checked without a readable directory.",
                "Make the repository directory available, then run doctor again.",
            ));
            checks.push(skipped_check(
                "repository.bare",
                path_field.clone(),
                "The bare repository check was skipped because the directory is unavailable.",
            ));
            checks.push(skipped_check(
                "repository.complete",
                path_field,
                "History completeness could not be checked because the directory is unavailable.",
            ));
            checks.push(skipped_check(
                "repository.release_ref",
                ref_field,
                "The release ref check was skipped because the repository directory is unavailable.",
            ));
            continue;
        };

        checks.push(Check::new(
            "repository.path",
            CheckStatus::Pass,
            path_field.clone(),
            "The configured repository directory is available.",
            "The Git mirror and release ref are checked separately.",
        ));
        if inconsistent_lanes.contains(&index) {
            checks.push(Check::new(
                "repository.mapping",
                CheckStatus::Fail,
                path_field.clone(),
                "Lanes using the same repository ID resolve to different local mirrors.",
                "Configure one canonical mirror path for each repository ID.",
            ));
        } else {
            checks.push(Check::new(
                "repository.mapping",
                CheckStatus::Pass,
                path_field.clone(),
                "The repository ID maps consistently to one local mirror.",
                "No mapping change is needed.",
            ));
        }

        let Some(git) = git else {
            checks.push(unavailable_git_check(
                "repository.bare",
                path_field.clone(),
                "Install Git at a supported system location and run doctor again.",
            ));
            checks.push(unavailable_git_check(
                "repository.complete",
                path_field,
                "Install Git at a supported system location and run doctor again.",
            ));
            checks.push(unavailable_git_check(
                "repository.release_ref",
                ref_field,
                "Install Git at a supported system location and run doctor again.",
            ));
            continue;
        };

        let bare = git_single_value(git, repository, &["rev-parse", "--is-bare-repository"]).await;
        let is_bare = matches!(bare, SingleValue::Value(ref value) if value == b"true");
        if is_bare {
            checks.push(Check::new(
                "repository.bare",
                CheckStatus::Pass,
                path_field.clone(),
                "The configured repository is a readable bare Git mirror.",
                "No mirror change is needed.",
            ));
        } else {
            checks.push(Check::new(
                "repository.bare",
                CheckStatus::Fail,
                path_field.clone(),
                "The configured repository could not be confirmed as a readable bare mirror.",
                "Create or repair a local bare mirror at the configured path.",
            ));
        }

        if is_bare {
            let shallow =
                git_single_value(git, repository, &["rev-parse", "--is-shallow-repository"]).await;
            let partial = git_partial_clone_state(git, repository).await;
            let complete = matches!(shallow, SingleValue::Value(ref value) if value == b"false")
                && matches!(partial, PartialCloneState::Complete);
            checks.push(if complete {
                Check::new(
                    "repository.complete",
                    CheckStatus::Pass,
                    path_field.clone(),
                    "The bare mirror has complete local history and no promisor configuration.",
                    "No mirror change is needed.",
                )
            } else if matches!(partial, PartialCloneState::Partial) {
                Check::new(
                    "repository.complete",
                    CheckStatus::Fail,
                    path_field.clone(),
                    "The repository is shallow or configured as a partial clone.",
                    "Replace it with a complete local bare mirror; doctor does not fetch objects.",
                )
            } else {
                Check::new(
                    "repository.complete",
                    CheckStatus::Fail,
                    path_field.clone(),
                    "Complete local history could not be confirmed.",
                    "Use a complete, non-shallow bare mirror and run doctor again.",
                )
            });

            if complete {
                let release_ref = resolve_release_ref(git, repository, &lane.release_ref).await;
                checks.push(match release_ref {
                    ReleaseRefState::Resolved => Check::new(
                        "repository.release_ref",
                        CheckStatus::Pass,
                        ref_field,
                        "The configured release ref resolves to a commit in the local mirror.",
                        "No release ref change is needed.",
                    ),
                    ReleaseRefState::Invalid => Check::new(
                        "repository.release_ref",
                        CheckStatus::Fail,
                        ref_field,
                        "The configured release ref does not resolve to a commit in the local mirror.",
                        "Correct the release ref or update the local mirror; doctor does not fetch.",
                    ),
                    ReleaseRefState::ProbeFailed => Check::new(
                        "repository.release_ref",
                        CheckStatus::Fail,
                        ref_field,
                        "The configured release ref could not be checked with the bounded local Git probe.",
                        "Check the local Git installation and mirror, then run doctor again.",
                    ),
                });
            } else {
                checks.push(Check::new(
                    "repository.release_ref",
                    CheckStatus::Skipped,
                    ref_field,
                    "The release ref was not probed because complete, non-shallow history could not be confirmed.",
                    "Use a complete local mirror; doctor does not fetch objects.",
                ));
            }
        } else {
            checks.push(skipped_check(
                "repository.complete",
                path_field,
                "History completeness could not be checked because the directory is not a confirmed bare mirror.",
            ));
            checks.push(skipped_check(
                "repository.release_ref",
                ref_field,
                "The release ref check was skipped because the directory is not a confirmed bare mirror.",
            ));
        }
    }
    checks
}

pub(super) async fn runtime_checks(enabled: bool) -> Vec<Check> {
    runtime_checks_with(
        enabled,
        fixed_executable(SYSTEMCTL_CANDIDATES).as_deref(),
        Path::new(CGROUP_CONTROLLERS),
    )
    .await
}

async fn runtime_checks_with(
    enabled: bool,
    systemctl: Option<&Path>,
    cgroup_controllers: &Path,
) -> Vec<Check> {
    if !enabled {
        return vec![
            Check::new(
                "runtime.user_systemd",
                CheckStatus::Skipped,
                "runtime.user_systemd",
                "The user systemd manager was not queried.",
                "Pass --check-local-runtime to run this read-only host check.",
            ),
            Check::new(
                "runtime.cgroup_v2",
                CheckStatus::Skipped,
                "runtime.cgroup_v2",
                "The cgroup v2 controller file was not checked.",
                "Pass --check-local-runtime to run this read-only host check.",
            ),
        ];
    }

    let systemd_check = match systemctl {
        Some(systemctl) => match query_user_systemd(systemctl).await {
            SystemdState::Running => Check::new(
                "runtime.user_systemd",
                CheckStatus::Pass,
                "runtime.user_systemd",
                "The local user systemd manager is running.",
                "No systemd change is needed.",
            ),
            SystemdState::Degraded => Check::new(
                "runtime.user_systemd",
                CheckStatus::Warn,
                "runtime.user_systemd",
                "The local user systemd manager is responding in a degraded state.",
                "Review the user manager before using the local systemd adapter.",
            ),
            SystemdState::Unavailable => Check::new(
                "runtime.user_systemd",
                CheckStatus::Fail,
                "runtime.user_systemd",
                "The local user systemd manager could not be confirmed as running.",
                "Start or repair the user manager before using the local systemd adapter.",
            ),
        },
        None => Check::new(
            "runtime.user_systemd",
            CheckStatus::Fail,
            "runtime.user_systemd",
            "A supported systemctl executable is unavailable.",
            "Install systemd at a supported system location before using the local systemd adapter.",
        ),
    };

    let cgroup_check = match read_bounded_file(cgroup_controllers).await {
        Ok(_) => Check::new(
            "runtime.cgroup_v2",
            CheckStatus::Pass,
            "runtime.cgroup_v2",
            "The cgroup v2 controller file is readable.",
            "No cgroup change is needed.",
        ),
        Err(_) => Check::new(
            "runtime.cgroup_v2",
            CheckStatus::Fail,
            "runtime.cgroup_v2",
            "The cgroup v2 controller file could not be read within the probe limits.",
            "Mount or expose the cgroup v2 hierarchy before using the local systemd adapter.",
        ),
    };

    vec![systemd_check, cgroup_check]
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SingleValue {
    Value(Vec<u8>),
    Failed,
}

async fn git_single_value(git: &Path, repository: &Path, args: &[&str]) -> SingleValue {
    match run_git(git, repository, args).await {
        Ok(output) if output.code == Some(0) => {
            let value = trim_ascii(&output.stdout);
            if value.is_empty() {
                SingleValue::Failed
            } else {
                SingleValue::Value(value.to_vec())
            }
        }
        _ => SingleValue::Failed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PartialCloneState {
    Complete,
    Partial,
    Unknown,
}

async fn git_partial_clone_state(git: &Path, repository: &Path) -> PartialCloneState {
    let output = match run_git(
        git,
        repository,
        &[
            "config",
            "--local",
            "--includes",
            "--get-regexp",
            r"^(extensions\.partialclone|remote\..*\.promisor)$",
        ],
    )
    .await
    {
        Ok(output) => output,
        Err(_) => return PartialCloneState::Unknown,
    };

    match output.code {
        Some(1) if output.stdout.is_empty() => PartialCloneState::Complete,
        Some(0) => parse_partial_clone_config(&output.stdout),
        _ => PartialCloneState::Unknown,
    }
}

fn parse_partial_clone_config(stdout: &[u8]) -> PartialCloneState {
    let mut saw_setting = false;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let Some(separator) = line.iter().position(|byte| *byte == b' ') else {
            return PartialCloneState::Unknown;
        };
        let key = &line[..separator];
        let value = trim_ascii(&line[separator + 1..]);
        saw_setting = true;
        if key.eq_ignore_ascii_case(b"extensions.partialclone") {
            return PartialCloneState::Partial;
        }
        if key
            .windows(b".promisor".len())
            .any(|window| window.eq_ignore_ascii_case(b".promisor"))
        {
            if value.eq_ignore_ascii_case(b"true") {
                return PartialCloneState::Partial;
            }
            if !value.eq_ignore_ascii_case(b"false") {
                return PartialCloneState::Unknown;
            }
        } else {
            return PartialCloneState::Unknown;
        }
    }
    if saw_setting {
        PartialCloneState::Complete
    } else {
        PartialCloneState::Unknown
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseRefState {
    Resolved,
    Invalid,
    ProbeFailed,
}

async fn resolve_release_ref(git: &Path, repository: &Path, release_ref: &str) -> ReleaseRefState {
    if release_ref.is_empty()
        || release_ref.len() > 1024
        || release_ref.starts_with('-')
        || release_ref.as_bytes().contains(&0)
    {
        return ReleaseRefState::Invalid;
    }

    let is_full_object_id = is_full_hex_object_id(release_ref.as_bytes());
    if !is_full_object_id {
        match run_git(
            git,
            repository,
            &["check-ref-format", "--allow-onelevel", release_ref],
        )
        .await
        {
            Ok(output) if output.code == Some(0) => {}
            Ok(_) => return ReleaseRefState::Invalid,
            Err(_) => return ReleaseRefState::ProbeFailed,
        }
    }

    let mut expression = String::with_capacity(release_ref.len() + 10);
    expression.push_str(release_ref);
    expression.push_str("^{commit}");
    match run_git(
        git,
        repository,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &expression,
        ],
    )
    .await
    {
        Ok(output)
            if output.code == Some(0) && is_full_hex_object_id(trim_ascii(&output.stdout)) =>
        {
            ReleaseRefState::Resolved
        }
        Ok(_) => ReleaseRefState::Invalid,
        Err(_) => ReleaseRefState::ProbeFailed,
    }
}

fn is_full_hex_object_id(value: &[u8]) -> bool {
    matches!(value.len(), 40 | 64) && value.iter().all(u8::is_ascii_hexdigit)
}

async fn query_user_systemd(systemctl: &Path) -> SystemdState {
    let bus_address = match std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        Some(address) => {
            let Some(address_text) = address.to_str() else {
                return SystemdState::Unavailable;
            };
            if address_text.is_empty()
                || address_text
                    .split(';')
                    .any(|address| !address.starts_with("unix:"))
            {
                return SystemdState::Unavailable;
            }
            Some(address)
        }
        None => None,
    };
    let mut command = Command::new(systemctl);
    command
        .args([
            "--no-pager",
            "--no-ask-password",
            "--user",
            "is-system-running",
        ])
        .current_dir("/")
        .env_clear()
        .env("SYSTEMD_PAGER", "cat")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_URLIFY", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|runtime_dir| Path::new(runtime_dir).is_absolute())
    {
        command.env("XDG_RUNTIME_DIR", runtime_dir);
    }
    if let Some(bus_address) = bus_address {
        command.env("DBUS_SESSION_BUS_ADDRESS", bus_address);
    }

    let Ok(output) = run_bounded(command).await else {
        return SystemdState::Unavailable;
    };
    let state = trim_ascii(&output.stdout);
    match state {
        b"running" if output.code == Some(0) => SystemdState::Running,
        b"degraded" => SystemdState::Degraded,
        _ => SystemdState::Unavailable,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemdState {
    Running,
    Degraded,
    Unavailable,
}

async fn run_git(
    git: &Path,
    repository: &Path,
    args: &[&str],
) -> Result<ProcessOutput, ProcessFailure> {
    let mut command = Command::new(git);
    command
        .args([
            "--no-pager",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.pager=cat",
            "-c",
            "credential.helper=",
            "-c",
            "credential.interactive=never",
            "-c",
            "protocol.allow=never",
        ])
        .arg(git_dir_argument(repository))
        .args(args)
        .current_dir("/")
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    run_bounded(command).await
}

fn git_dir_argument(path: &Path) -> OsString {
    let mut argument = OsString::from("--git-dir=");
    argument.push(path.as_os_str());
    argument
}

async fn run_bounded(mut command: Command) -> Result<ProcessOutput, ProcessFailure> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| ProcessFailure::Spawn)?;

    let result = timeout(PROBE_TIMEOUT, async {
        let stdout = child.stdout.take().ok_or(ProcessFailure::Io)?;
        let mut reader = Box::pin(read_bounded(stdout));
        let mut waiter = Box::pin(child.wait());
        tokio::select! {
            bytes = &mut reader => {
                let stdout = bytes?;
                let status = waiter.as_mut().await.map_err(|_| ProcessFailure::Io)?;
                Ok(ProcessOutput { code: status.code(), stdout })
            }
            status = &mut waiter => {
                let status = status.map_err(|_| ProcessFailure::Io)?;
                let stdout = reader.as_mut().await?;
                Ok(ProcessOutput { code: status.code(), stdout })
            }
        }
    })
    .await;

    match result {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            reap(&mut child).await;
            Err(error)
        }
        Err(_) => {
            reap(&mut child).await;
            Err(ProcessFailure::Timeout)
        }
    }
}

async fn reap(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn read_bounded<R>(mut reader: R) -> Result<Vec<u8>, ProcessFailure>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(MAX_PROBE_STDOUT);
    let mut buffer = [0_u8; 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| ProcessFailure::Io)?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_PROBE_STDOUT {
            return Err(ProcessFailure::OutputLimit);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

async fn read_bounded_file(path: &Path) -> Result<Vec<u8>, ProcessFailure> {
    timeout(PROBE_TIMEOUT, async {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| ProcessFailure::Io)?;
        let mut file = file.take((MAX_PROBE_STDOUT + 1) as u64);
        let mut bytes = Vec::with_capacity(MAX_PROBE_STDOUT);
        file.read_to_end(&mut bytes)
            .await
            .map_err(|_| ProcessFailure::Io)?;
        if bytes.len() > MAX_PROBE_STDOUT {
            return Err(ProcessFailure::OutputLimit);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ProcessFailure::Timeout)?
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while start < end && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

fn fixed_executable(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_file())
}

fn skipped_check(code: &'static str, field: String, message: &'static str) -> Check {
    Check::new(
        code,
        CheckStatus::Skipped,
        field,
        message,
        "Resolve the repository directory before running this check.",
    )
}

fn unavailable_git_check(code: &'static str, field: String, suggestion: &'static str) -> Check {
    Check::new(
        code,
        CheckStatus::Fail,
        field,
        "The bounded local Git probe could not verify this prerequisite.",
        suggestion,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command as StdCommand, Stdio as StdStdio},
        time::SystemTime,
    };

    use tempfile::tempdir;

    use super::{
        MAX_PROBE_STDOUT, ProcessFailure, repository_checks_with_git, run_bounded,
        runtime_checks_with,
    };
    use crate::{
        config::{Config, LaneConfig},
        diagnostics::CheckStatus,
    };

    const GIT: &str = "/usr/bin/git";

    fn executable_script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn test_command(program: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(program);
        command.current_dir("/").env_clear();
        command
    }

    fn git_command(args: &[&str]) {
        let status = StdCommand::new(GIT)
            .args(args)
            .env_clear()
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_command_at(path: &Path, args: &[&str]) {
        let status = StdCommand::new(GIT)
            .args(args)
            .current_dir(path)
            .env_clear()
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    type TreeSnapshot = BTreeMap<PathBuf, (bool, u64, Option<SystemTime>, Vec<u8>)>;

    fn snapshot_tree(root: &Path) -> TreeSnapshot {
        fn visit(root: &Path, current: &Path, snapshot: &mut TreeSnapshot) {
            for entry in fs::read_dir(current).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                let is_dir = metadata.is_dir();
                let modified = metadata.modified().ok();
                let contents = if metadata.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                snapshot.insert(relative, (is_dir, metadata.len(), modified, contents));
                if is_dir {
                    visit(root, &path, snapshot);
                }
            }
        }

        let mut snapshot = BTreeMap::new();
        visit(root, root, &mut snapshot);
        snapshot
    }

    fn lane(repository_path: PathBuf, release_ref: &str) -> LaneConfig {
        LaneConfig {
            id: "lane-one".into(),
            project: "project".into(),
            environment: "test".into(),
            repository_id: "repository-one".into(),
            repository_path,
            release_ref: release_ref.into(),
            profile_digest: "profile".into(),
            components: vec!["app".into()],
            request_parameters: Default::default(),
            allow_descendant: false,
            allow_preemption: false,
            enabled: true,
            executor: Default::default(),
            failure_observe: None,
            resource_admission: None,
        }
    }

    #[tokio::test]
    async fn bounded_runner_stops_on_stdout_limit_and_timeout() {
        let long_output = "x".repeat(MAX_PROBE_STDOUT + 512);
        let noisy_command = format!("printf '%s' '{long_output}'");
        let mut noisy = test_command(Path::new("/bin/sh"));
        noisy.args(["-c", noisy_command.as_str()]);
        assert_eq!(
            run_bounded(noisy).await.unwrap_err(),
            ProcessFailure::OutputLimit
        );

        let mut slow = test_command(Path::new("/bin/sleep"));
        slow.arg("10");
        assert_eq!(
            run_bounded(slow).await.unwrap_err(),
            ProcessFailure::Timeout
        );
    }

    #[tokio::test]
    async fn repository_probes_are_readonly_and_reports_hide_repository_values() {
        let temp = tempdir().unwrap();
        let work = temp.path().join("work");
        let mirror = temp.path().join("mirror.git");
        fs::create_dir(&work).unwrap();
        git_command(&["init", "--quiet", "--bare", mirror.to_str().unwrap()]);
        git_command(&["init", "--quiet", work.to_str().unwrap()]);
        git_command_at(&work, &["config", "user.name", "Fixture"]);
        git_command_at(&work, &["config", "user.email", "fixture@example.invalid"]);
        fs::write(work.join("content"), "fixture\n").unwrap();
        git_command_at(&work, &["add", "content"]);
        git_command_at(&work, &["commit", "--quiet", "-m", "fixture"]);
        git_command_at(
            &work,
            &[
                "push",
                "--quiet",
                mirror.to_str().unwrap(),
                "HEAD:refs/heads/release",
            ],
        );

        let before = snapshot_tree(&mirror);
        let config = Config {
            lanes: vec![lane(mirror.clone(), "refs/heads/release")],
            ..Config::default()
        };
        let checks = repository_checks_with_git(&config, Some(Path::new(GIT))).await;
        let after = snapshot_tree(&mirror);
        assert_eq!(before, after);
        assert_eq!(checks.len(), 5);
        assert!(checks.iter().all(|check| check.status == CheckStatus::Pass));
        let json = serde_json::to_string(&checks).unwrap();
        assert!(!json.contains(mirror.to_str().unwrap()));
        assert!(!json.contains("refs/heads/release"));
        assert!(!json.contains("Fixture"));

        git_command(&[
            "--git-dir",
            mirror.to_str().unwrap(),
            "config",
            "--local",
            "extensions.partialClone",
            "origin",
        ]);
        git_command(&[
            "--git-dir",
            mirror.to_str().unwrap(),
            "config",
            "--local",
            "remote.origin.promisor",
            "true",
        ]);
        let checks = repository_checks_with_git(&config, Some(Path::new(GIT))).await;
        assert_eq!(checks[3].code, "repository.complete");
        assert_eq!(checks[3].status, CheckStatus::Fail);
        assert_eq!(checks[4].code, "repository.release_ref");
        assert_eq!(checks[4].status, CheckStatus::Skipped);

        git_command(&[
            "--git-dir",
            mirror.to_str().unwrap(),
            "config",
            "--local",
            "--unset",
            "extensions.partialClone",
        ]);
        git_command(&[
            "--git-dir",
            mirror.to_str().unwrap(),
            "config",
            "--local",
            "--unset",
            "remote.origin.promisor",
        ]);
        let included_config = temp.path().join("included-git-config");
        fs::write(
            &included_config,
            "[remote \"included\"]\n\tpromisor = true\n",
        )
        .unwrap();
        git_command(&[
            "--git-dir",
            mirror.to_str().unwrap(),
            "config",
            "--local",
            "include.path",
            included_config.to_str().unwrap(),
        ]);
        let checks = repository_checks_with_git(&config, Some(Path::new(GIT))).await;
        assert_eq!(checks[3].code, "repository.complete");
        assert_eq!(checks[3].status, CheckStatus::Fail);
        assert_eq!(checks[4].code, "repository.release_ref");
        assert_eq!(checks[4].status, CheckStatus::Skipped);

        let canary = "doctor-secret-canary";
        let config = Config {
            lanes: vec![lane(mirror.clone(), canary)],
            ..Config::default()
        };
        let checks = repository_checks_with_git(&config, Some(Path::new(GIT))).await;
        let json = serde_json::to_string(&checks).unwrap();
        assert!(!json.contains(canary));
    }

    #[tokio::test]
    async fn default_runtime_probe_skips_without_running_systemctl() {
        let temp = tempdir().unwrap();
        let systemctl = temp.path().join("systemctl");
        let marker = temp.path().join("systemctl-ran");
        executable_script(
            &systemctl,
            &format!("printf ran > '{}'\nprintf running", marker.display()),
        );
        let controllers = temp.path().join("cgroup.controllers");
        fs::write(&controllers, "cpu memory\n").unwrap();

        let checks = runtime_checks_with(false, Some(&systemctl), &controllers).await;
        assert_eq!(checks.len(), 2);
        assert!(
            checks
                .iter()
                .all(|check| check.status == CheckStatus::Skipped)
        );
        assert!(!marker.exists());
    }
}
