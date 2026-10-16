use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::{LineageRelation, TargetAuthorization};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LineageKey {
    repository_id: String,
    candidate_revision: String,
    requested_revision: String,
    release_line_id: String,
    config_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageProof {
    pub repository_id: String,
    pub candidate_revision: String,
    pub requested_revision: String,
    pub release_line_id: String,
    pub config_version: u64,
    pub relation: LineageRelation,
    pub proof_digest: String,
}

/// One Git relationship the scheduler may consult while reconciling a lane.
/// Backend code can resolve these in batches before calling the pure Engine.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoverageQuery {
    pub repository_id: String,
    pub candidate_revision: String,
    pub requested_revision: String,
    pub release_line_id: String,
    pub config_version: u64,
}

impl LineageProof {
    fn key(&self) -> LineageKey {
        LineageKey {
            repository_id: self.repository_id.clone(),
            candidate_revision: self.candidate_revision.clone(),
            requested_revision: self.requested_revision.clone(),
            release_line_id: self.release_line_id.clone(),
            config_version: self.config_version,
        }
    }
}

/// Precomputed Git facts passed into the pure Engine. Building this context
/// may invoke git; looking up facts in it is deterministic and has no I/O.
#[derive(Clone, Debug, Default)]
pub struct CoverageContext {
    proofs: BTreeMap<LineageKey, LineageProof>,
}

impl CoverageContext {
    pub fn insert(&mut self, proof: LineageProof) {
        self.proofs.insert(proof.key(), proof);
    }

    pub fn get(
        &self,
        candidate_revision: &str,
        requested_revision: &str,
        repository_id: &str,
        release_line_id: &str,
        config_version: u64,
    ) -> Option<&LineageProof> {
        self.proofs.get(&LineageKey {
            repository_id: repository_id.to_owned(),
            candidate_revision: candidate_revision.to_owned(),
            requested_revision: requested_revision.to_owned(),
            release_line_id: release_line_id.to_owned(),
            config_version,
        })
    }

    pub fn from_proofs(proofs: impl IntoIterator<Item = LineageProof>) -> Self {
        let mut context = Self::default();
        for proof in proofs {
            context.insert(proof);
        }
        context
    }

    pub fn is_empty(&self) -> bool {
        self.proofs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.proofs.len()
    }
}

#[derive(Clone, Debug)]
pub struct LineageResolver {
    mirrors: BTreeMap<String, PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum LineageError {
    #[error("mirror path for repository {0} is not configured")]
    RepositoryNotConfigured(String),
    #[error("configured mirror is unavailable or invalid: {0}")]
    InvalidMirror(String),
}

impl LineageResolver {
    /// Construct a resolver from administrator-controlled repository IDs and
    /// local bare-mirror paths. Paths are canonicalized once, and no resolver
    /// method fetches from or contacts a remote repository.
    pub fn new(mirrors: BTreeMap<String, PathBuf>) -> Result<Self, LineageError> {
        let mut canonical = BTreeMap::new();
        for (repository_id, path) in mirrors {
            let path = path.canonicalize().map_err(|_| {
                LineageError::InvalidMirror(format!("{} ({})", repository_id, path.display()))
            })?;
            if !path.is_dir() {
                return Err(LineageError::InvalidMirror(format!(
                    "{} ({})",
                    repository_id,
                    path.display()
                )));
            }
            let output = Command::new("git")
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&path)
                .args(["rev-parse", "--is-bare-repository"])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .env("GIT_NO_REPLACE_OBJECTS", "1")
                .env("GIT_NO_LAZY_FETCH", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .map_err(|_| {
                    LineageError::InvalidMirror(format!("{} ({})", repository_id, path.display()))
                })?;
            if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true"
            {
                return Err(LineageError::InvalidMirror(format!(
                    "{} is not a readable bare mirror ({})",
                    repository_id,
                    path.display()
                )));
            }
            let shallow = Command::new("git")
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&path)
                .args(["rev-parse", "--is-shallow-repository"])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .env("GIT_NO_REPLACE_OBJECTS", "1")
                .env("GIT_NO_LAZY_FETCH", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .map_err(|_| {
                    LineageError::InvalidMirror(format!("{} ({})", repository_id, path.display()))
                })?;
            if !shallow.status.success()
                || String::from_utf8_lossy(&shallow.stdout).trim() != "false"
            {
                return Err(LineageError::InvalidMirror(format!(
                    "{} is shallow or could not confirm complete history ({})",
                    repository_id,
                    path.display()
                )));
            }
            canonical.insert(repository_id, path);
        }
        Ok(Self { mirrors: canonical })
    }

    /// Verify that a complete immutable revision exists in the local mirror
    /// and is reachable from the configured release ref. The returned proof
    /// is bound to the repository, ref tip, release-line ID, and config
    /// version. `Unknown` is returned for incomplete history or Git failures.
    pub fn resolve_submission(
        &self,
        repository_id: &str,
        release_ref: &str,
        revision: &str,
        release_line_id: &str,
        config_version: u64,
    ) -> TargetAuthorization {
        let mut proof = TargetAuthorization {
            repository_id: repository_id.to_owned(),
            release_ref: release_ref.to_owned(),
            release_tip: String::new(),
            revision: revision.to_owned(),
            release_line_id: release_line_id.to_owned(),
            config_version,
            relation: LineageRelation::Unknown,
            proof_digest: String::new(),
        };

        let Some(mirror) = self.mirrors.get(repository_id) else {
            proof.proof_digest = authorization_key(&proof);
            return proof;
        };
        if !is_full_revision(revision) || !is_safe_ref(release_ref) {
            proof.proof_digest = authorization_key(&proof);
            return proof;
        }

        let Ok(tip_output) = git_output(
            mirror,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{release_ref}^{{commit}}"),
            ],
        ) else {
            proof.proof_digest = authorization_key(&proof);
            return proof;
        };
        if !tip_output.status.success() {
            proof.proof_digest = authorization_key(&proof);
            return proof;
        }
        let tip = String::from_utf8_lossy(&tip_output.stdout)
            .trim()
            .to_ascii_lowercase();
        if !is_full_revision(&tip) || !commit_exists(mirror, revision) {
            proof.proof_digest = authorization_key(&proof);
            return proof;
        }
        proof.release_tip = tip.clone();
        proof.relation = match is_ancestor(mirror, revision, &tip) {
            Some(true) => LineageRelation::Ancestor,
            Some(false) => LineageRelation::NotAncestor,
            None => LineageRelation::Unknown,
        };
        proof.proof_digest = authorization_key(&proof);
        proof
    }

    /// Compute the version relation needed by descendant coverage. The
    /// requested revision must be an ancestor of the candidate revision.
    pub fn prove_coverage(
        &self,
        repository_id: &str,
        candidate_revision: &str,
        requested_revision: &str,
        release_line_id: &str,
        config_version: u64,
    ) -> LineageProof {
        let mut proof = LineageProof {
            repository_id: repository_id.to_owned(),
            candidate_revision: candidate_revision.to_owned(),
            requested_revision: requested_revision.to_owned(),
            release_line_id: release_line_id.to_owned(),
            config_version,
            relation: LineageRelation::Unknown,
            proof_digest: String::new(),
        };
        let Some(mirror) = self.mirrors.get(repository_id) else {
            proof.proof_digest = lineage_key(&proof);
            return proof;
        };
        if is_full_revision(candidate_revision)
            && is_full_revision(requested_revision)
            && commit_exists(mirror, candidate_revision)
            && commit_exists(mirror, requested_revision)
        {
            proof.relation = match is_ancestor(mirror, requested_revision, candidate_revision) {
                Some(true) => LineageRelation::Ancestor,
                Some(false) => LineageRelation::NotAncestor,
                None => LineageRelation::Unknown,
            };
        }
        proof.proof_digest = lineage_key(&proof);
        proof
    }

    /// Resolve a set of scheduler coverage checks while sharing Git work per
    /// candidate revision. A single `rev-list` produces each candidate's
    /// complete ancestor set; this avoids one subprocess per candidate/request
    /// pair when a lane has a larger waiting queue.
    pub fn resolve_coverage_context(&self, queries: &[CoverageQuery]) -> CoverageContext {
        let mut context = CoverageContext::default();
        let mut requested_by_repo: BTreeMap<String, std::collections::BTreeSet<String>> =
            BTreeMap::new();
        for query in queries {
            if is_full_revision(&query.requested_revision) {
                requested_by_repo
                    .entry(query.repository_id.clone())
                    .or_default()
                    .insert(query.requested_revision.to_ascii_lowercase());
            }
        }
        let existing_by_repo = requested_by_repo
            .into_iter()
            .filter_map(|(repository_id, revisions)| {
                let mirror = self.mirrors.get(&repository_id)?;
                Some((
                    repository_id,
                    existing_commits(mirror, revisions.iter().map(String::as_str)),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let mut grouped: BTreeMap<(String, String, String, u64), Vec<&CoverageQuery>> =
            BTreeMap::new();
        for query in queries {
            grouped
                .entry((
                    query.repository_id.clone(),
                    query.candidate_revision.clone(),
                    query.release_line_id.clone(),
                    query.config_version,
                ))
                .or_default()
                .push(query);
        }

        for ((repository_id, candidate_revision, release_line_id, config_version), group) in grouped
        {
            let mut ancestors = None;
            if is_full_revision(&candidate_revision)
                && let Some(mirror) = self.mirrors.get(&repository_id)
                && let Ok(output) = git_output(mirror, ["rev-list", &candidate_revision])
                && output.status.success()
            {
                ancestors = Some(
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::to_ascii_lowercase)
                        .collect::<std::collections::BTreeSet<_>>(),
                );
            }
            for query in group {
                let relation = match ancestors.as_ref() {
                    Some(ancestor_set)
                        if is_full_revision(&query.requested_revision)
                            && existing_by_repo.get(&repository_id).is_some_and(|commits| {
                                commits.contains(&query.requested_revision.to_ascii_lowercase())
                            }) =>
                    {
                        if ancestor_set.contains(&query.requested_revision.to_ascii_lowercase()) {
                            LineageRelation::Ancestor
                        } else {
                            LineageRelation::NotAncestor
                        }
                    }
                    _ => LineageRelation::Unknown,
                };
                let mut proof = LineageProof {
                    repository_id: repository_id.clone(),
                    candidate_revision: candidate_revision.clone(),
                    requested_revision: query.requested_revision.clone(),
                    release_line_id: release_line_id.clone(),
                    config_version,
                    relation,
                    proof_digest: String::new(),
                };
                proof.proof_digest = lineage_key(&proof);
                context.insert(proof);
            }
        }
        context
    }

    pub fn mirror_path(&self, repository_id: &str) -> Option<&Path> {
        self.mirrors.get(repository_id).map(PathBuf::as_path)
    }
}

pub fn is_full_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_safe_ref(value: &str) -> bool {
    if value.starts_with('-') || value.is_empty() || value.contains("..") || value.contains("@{") {
        return false;
    }
    if is_full_revision(value) {
        return true;
    }
    // `check-ref-format` accepts only a ref token and we also pass it as one
    // process argument, never through a shell.
    Command::new("git")
        .args(["check-ref-format", "--allow-onelevel", value])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn commit_exists(mirror: &Path, revision: &str) -> bool {
    let expression = format!("{revision}^{{commit}}");
    git_output(mirror, ["cat-file", "-e", &expression]).is_ok_and(|output| output.status.success())
}

fn existing_commits<'a>(
    mirror: &Path,
    revisions: impl IntoIterator<Item = &'a str>,
) -> std::collections::BTreeSet<String> {
    let mut child = match Command::new("git")
        .arg("--no-pager")
        .arg("--git-dir")
        .arg(mirror)
        .args(["cat-file", "--batch-check"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return std::collections::BTreeSet::new(),
    };
    if let Some(mut stdin) = child.stdin.take() {
        for revision in revisions {
            if writeln!(stdin, "{revision}").is_err() {
                return std::collections::BTreeSet::new();
            }
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return std::collections::BTreeSet::new();
    };
    if !output.status.success() {
        return std::collections::BTreeSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let revision = fields.next()?;
            let object_type = fields.next()?;
            (object_type == "commit").then(|| revision.to_ascii_lowercase())
        })
        .collect()
}

fn is_ancestor(mirror: &Path, ancestor: &str, descendant: &str) -> Option<bool> {
    let output = git_output(
        mirror,
        ["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

fn git_output<const N: usize>(mirror: &Path, args: [&str; N]) -> std::io::Result<Output> {
    Command::new("git")
        .arg("--no-pager")
        .arg("--git-dir")
        .arg(mirror)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(Stdio::null())
        .output()
}

fn authorization_key(proof: &TargetAuthorization) -> String {
    let material = format!(
        "repo={};ref={};tip={};revision={};line={};config={};relation={:?}",
        proof.repository_id,
        proof.release_ref,
        proof.release_tip,
        proof.revision,
        proof.release_line_id,
        proof.config_version,
        proof.relation
    );
    hex::encode(Sha256::digest(material.as_bytes()))
}

fn lineage_key(proof: &LineageProof) -> String {
    let material = format!(
        "repo={};candidate={};requested={};line={};config={};relation={:?}",
        proof.repository_id,
        proof.candidate_revision,
        proof.requested_revision,
        proof.release_line_id,
        proof.config_version,
        proof.relation
    );
    hex::encode(Sha256::digest(material.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        process::{Command, Output},
    };

    use super::{CoverageQuery, LineageResolver, is_full_revision};
    use crate::domain::LineageRelation;

    struct TempGit {
        root: PathBuf,
    }

    impl TempGit {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("auto-deliver-lineage-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn run(&self, cwd: &std::path::Path, args: &[&str]) -> Output {
            let output = Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }

        fn commit(&self, repo: &std::path::Path, file: &str, content: &str) -> String {
            fs::write(repo.join(file), content).unwrap();
            self.run(repo, &["add", file]);
            self.run(repo, &["commit", "-m", "fixture"]);
            String::from_utf8(self.run(repo, &["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim()
                .to_owned()
        }
    }

    impl Drop for TempGit {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn proves_reachable_submissions_and_descendant_direction_from_local_bare_mirror() {
        let fixture = TempGit::new();
        let work = fixture.root.join("work");
        let mirror = fixture.root.join("mirror.git");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&mirror).unwrap();
        fixture.run(&work, &["init", "-q", "-b", "main"]);
        fixture.run(&work, &["config", "user.name", "fixture"]);
        fixture.run(&work, &["config", "user.email", "fixture@example.invalid"]);
        let first = fixture.commit(&work, "file", "first\n");
        let second = fixture.commit(&work, "file", "second\n");
        fixture.run(&mirror, &["init", "--bare", "-q"]);
        fixture.run(
            &mirror,
            &[
                "fetch",
                "--quiet",
                work.to_str().unwrap(),
                "main:refs/heads/main",
            ],
        );

        let resolver =
            LineageResolver::new(BTreeMap::from([("repo".to_owned(), mirror.clone())])).unwrap();
        let auth = resolver.resolve_submission("repo", "refs/heads/main", &first, "stable", 3);
        assert_eq!(auth.relation, LineageRelation::Ancestor);
        assert_eq!(auth.release_tip, second);

        let forwards = resolver.prove_coverage("repo", &second, &first, "stable", 3);
        assert_eq!(forwards.relation, LineageRelation::Ancestor);
        let backwards = resolver.prove_coverage("repo", &first, &second, "stable", 3);
        assert_eq!(backwards.relation, LineageRelation::NotAncestor);
        let missing = resolver.prove_coverage(
            "repo",
            &second,
            "0000000000000000000000000000000000000000",
            "stable",
            3,
        );
        assert_eq!(missing.relation, LineageRelation::Unknown);
        let batched = resolver.resolve_coverage_context(&[
            CoverageQuery {
                repository_id: "repo".to_owned(),
                candidate_revision: second.clone(),
                requested_revision: first.clone(),
                release_line_id: "stable".to_owned(),
                config_version: 3,
            },
            CoverageQuery {
                repository_id: "repo".to_owned(),
                candidate_revision: first.clone(),
                requested_revision: second.clone(),
                release_line_id: "stable".to_owned(),
                config_version: 3,
            },
            CoverageQuery {
                repository_id: "repo".to_owned(),
                candidate_revision: second.clone(),
                requested_revision: "0000000000000000000000000000000000000000".to_owned(),
                release_line_id: "stable".to_owned(),
                config_version: 3,
            },
        ]);
        assert_eq!(
            batched
                .get(&second, &first, "repo", "stable", 3)
                .unwrap()
                .relation,
            LineageRelation::Ancestor
        );
        assert_eq!(
            batched
                .get(&first, &second, "repo", "stable", 3)
                .unwrap()
                .relation,
            LineageRelation::NotAncestor
        );
        assert_eq!(
            batched
                .get(
                    &second,
                    "0000000000000000000000000000000000000000",
                    "repo",
                    "stable",
                    3,
                )
                .unwrap()
                .relation,
            LineageRelation::Unknown
        );
        assert!(is_full_revision(&second));
        assert!(!is_full_revision("main"));
    }

    #[test]
    fn unknown_repository_fails_closed_without_git_io() {
        let resolver = LineageResolver::new(BTreeMap::new()).unwrap();
        let proof = resolver.prove_coverage(
            "missing",
            "a".repeat(40).as_str(),
            "b".repeat(40).as_str(),
            "line",
            1,
        );
        assert_eq!(proof.relation, LineageRelation::Unknown);
    }
}
