//! Shared types and validation for the Veryl registry.
//!
//! This crate is deliberately tiny and WASM-safe so the Cloudflare Worker can
//! depend on it without bloating the compiled WASM (no `regex`, no heavy deps).
//! The native indexer used by GitHub Actions reuses the exact same validation,
//! so the intake path and the apply path never drift.

use serde::{Deserialize, Serialize};

/// A registration request sent by `veryl publish` to the intake Worker.
///
/// The registry keys entries by the repository's `<host>/<path>` — e.g.
/// `github.com/<owner>/<repo>` or `gitlab.com/<group>/<subgroup>/<repo>` — which
/// is globally unique, not by project name (which is not unique). `version` is
/// only a hint; the crawler reads each repository's `Veryl.pub` for the
/// authoritative version set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub repo: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Reason a [`Submission`] was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    Repo(String),
    Name(String),
    Version(String),
}

impl core::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ValidationError::Repo(m) => write!(f, "invalid repo: {m}"),
            ValidationError::Name(m) => write!(f, "invalid name: {m}"),
            ValidationError::Version(m) => write!(f, "invalid version: {m}"),
        }
    }
}

impl std::error::Error for ValidationError {}

impl Submission {
    /// Validate every field. Both the Worker and the indexer call this.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_repo(&self.repo)?;
        validate_name(&self.name)?;
        if let Some(version) = &self.version {
            validate_version(version)?;
        }
        Ok(())
    }

    /// Split the key into its host and path segments, e.g.
    /// `github.com/alice/fifo` -> `("github.com", ["alice", "fifo"])` and
    /// `gitlab.com/grp/sub/proj` -> `("gitlab.com", ["grp", "sub", "proj"])`.
    ///
    /// Returns `None` if the shape is wrong; callers that already ran
    /// [`Submission::validate`] can `unwrap`.
    pub fn parts(&self) -> Option<(&str, Vec<&str>)> {
        let mut it = self.repo.split('/');
        let host = it.next()?;
        if host.is_empty() {
            return None;
        }
        let segments: Vec<&str> = it.collect();
        if segments.len() < 2 || segments.iter().any(|s| s.is_empty()) {
            return None;
        }
        Some((host, segments))
    }
}

fn validate_repo(repo: &str) -> Result<(), ValidationError> {
    let mut parts = repo.split('/');
    let host = parts.next().unwrap_or("");
    validate_host(host).map_err(|m| ValidationError::Repo(format!("host: {m}")))?;

    // Any git host and any nesting depth (GitLab subgroups), but at least
    // `<owner>/<repo>` so a bare user/group page is not taken for a repo.
    let mut segments = 0;
    for seg in parts {
        validate_segment(seg).map_err(|m| ValidationError::Repo(format!("path: {m}")))?;
        segments += 1;
    }
    if segments < 2 {
        return Err(ValidationError::Repo(
            "expected `<host>/<owner>/<repo>` with at least two path segments".into(),
        ));
    }
    Ok(())
}

/// A repository host like `github.com` or `git.example.com`. The dot requirement
/// stops a bare path segment from posing as a host; the charset whitelist also
/// guards path traversal, since the host is the top on-disk dir `registry/<host>/`.
/// Ports and userinfo are not accepted.
fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("empty".into());
    }
    if !host.contains('.') {
        return Err("must be a hostname (contain a `.`)".into());
    }
    if host.contains("..") {
        return Err("`..` not allowed".into());
    }
    if host.starts_with(['.', '-']) || host.ends_with(['.', '-']) {
        return Err("must not begin or end with `.` or `-`".into());
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return Err("only [A-Za-z0-9.-] allowed".into());
    }
    Ok(())
}

/// A single path segment after the host (`owner`, `repo`, or a GitLab subgroup).
/// Because these become on-disk path components `registry/<host>/.../<repo>.json`,
/// the whitelist here is also the defense against path traversal (`..`, `/`)
/// reaching the index writer.
fn validate_segment(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("empty".into());
    }
    if s == "." || s == ".." {
        return Err("reserved".into());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("only [A-Za-z0-9._-] allowed".into());
    }
    Ok(())
}

/// Mirrors the compiler's `VALID_PROJECT_NAME` (`^[a-zA-Z_][0-9a-zA-Z_]*$`) plus
/// the reserved `__` prefix rule in `veryl-metadata`'s `check_project_name`.
fn validate_name(name: &str) -> Result<(), ValidationError> {
    let mut chars = name.chars();
    let first = chars
        .next()
        .ok_or_else(|| ValidationError::Name("empty".into()))?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(ValidationError::Name(
            "must start with a letter or `_`".into(),
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ValidationError::Name("only [0-9a-zA-Z_] allowed".into()));
    }
    if name.starts_with("__") {
        return Err(ValidationError::Name(
            "names starting with `__` are reserved".into(),
        ));
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<(), ValidationError> {
    semver::Version::parse(version)
        .map(|_| ())
        .map_err(|e| ValidationError::Version(e.to_string()))
}

/// Whether `name` is a valid Veryl project name. Public so the indexer can reject
/// an untrusted `Veryl.toml` project name before it is used as a filesystem path
/// segment (docs are written under `.../<project>/...`).
pub fn is_valid_project_name(name: &str) -> bool {
    validate_name(name).is_ok()
}

/// Whether `version` parses as semver. Public so the indexer can reject an
/// untrusted `Veryl.pub` version before it is used as a filesystem path segment.
pub fn is_valid_version(version: &str) -> bool {
    validate_version(version).is_ok()
}

/// Whether `s` is a safe `owner`/`repo` path segment: rejects `.`/`..`, `/`, and
/// anything outside `[A-Za-z0-9._-]`.
pub fn is_valid_segment(s: &str) -> bool {
    validate_segment(s).is_ok()
}

/// Split a key `<host>/<path>` into `(host, [segments])`, re-checking the host
/// and every segment. Unlike [`Submission::parts`] (which trusts a prior
/// `validate`), this re-validates, so the crawl can split a hand-edited index
/// entry without a bad host or `..` reaching the docs tree.
pub fn split_key(repo: &str) -> Option<(&str, Vec<&str>)> {
    let mut it = repo.split('/');
    let host = it.next()?;
    if validate_host(host).is_err() {
        return None;
    }
    let segments: Vec<&str> = it.collect();
    if segments.len() < 2 || !segments.iter().all(|s| is_valid_segment(s)) {
        return None;
    }
    Some((host, segments))
}

/// Lowercase the host only (hosts are case-insensitive; path case can be
/// significant), so `GitHub.com/...` can't become a second entry for the repo
/// `github.com/...` already keys.
pub fn canonical_repo(repo: &str) -> String {
    match repo.split_once('/') {
        Some((host, rest)) => format!("{}/{rest}", host.to_ascii_lowercase()),
        None => repo.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(repo: &str, name: &str, version: Option<&str>) -> Submission {
        Submission {
            repo: repo.into(),
            name: name.into(),
            version: version.map(Into::into),
        }
    }

    #[test]
    fn accepts_a_well_formed_submission() {
        let s = sub("github.com/alice/fifo", "fifo", Some("1.2.0"));
        assert!(s.validate().is_ok());
        assert_eq!(s.parts(), Some(("github.com", vec!["alice", "fifo"])));
    }

    #[test]
    fn version_is_optional() {
        assert!(sub("github.com/a/b", "b", None).validate().is_ok());
    }

    #[test]
    fn accepts_any_host_and_nesting_depth() {
        for repo in [
            "gitlab.com/group/proj",
            "gitlab.com/group/subgroup/proj",
            "codeberg.org/alice/fifo",
            "git.example.com/team/sub/deep/repo",
        ] {
            assert!(
                sub(repo, "proj", None).validate().is_ok(),
                "should accept {repo}"
            );
        }
        assert_eq!(
            sub("gitlab.com/group/subgroup/proj", "proj", None).parts(),
            Some(("gitlab.com", vec!["group", "subgroup", "proj"]))
        );
    }

    #[test]
    fn rejects_non_host_or_too_shallow() {
        for repo in [
            "localhost/a/b",    // no dot: not a hostname
            "github.com/owner", // only one path segment (a user page)
            "github.com",       // host only
        ] {
            assert!(
                matches!(
                    sub(repo, "b", None).validate(),
                    Err(ValidationError::Repo(_))
                ),
                "should reject {repo}"
            );
        }
    }

    #[test]
    fn rejects_path_traversal_in_repo() {
        for repo in [
            "github.com/../etc", // `..` path segment
            "github.com/a/..",   // `..` path segment
            "../etc/passwd",     // `..` host
            "github.com/a//b",   // empty segment
        ] {
            assert!(
                matches!(
                    sub(repo, "b", None).validate(),
                    Err(ValidationError::Repo(_))
                ),
                "should reject {repo}"
            );
        }
    }

    #[test]
    fn rejects_bad_names() {
        for name in ["1fifo", "fi-fo", "__reserved", ""] {
            assert!(
                matches!(
                    sub("github.com/a/b", name, None).validate(),
                    Err(ValidationError::Name(_))
                ),
                "should reject {name:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_version() {
        assert!(matches!(
            sub("github.com/a/b", "b", Some("v1")).validate(),
            Err(ValidationError::Version(_))
        ));
    }

    #[test]
    fn split_key_validates_host_and_segments() {
        assert_eq!(
            split_key("github.com/alice/fifo"),
            Some(("github.com", vec!["alice", "fifo"]))
        );
        assert_eq!(
            split_key("gitlab.com/group/sub/proj"),
            Some(("gitlab.com", vec!["group", "sub", "proj"]))
        );
        // A hand-edited entry cannot smuggle traversal or a bogus host past this.
        assert_eq!(split_key("github.com/a/.."), None);
        assert_eq!(split_key("../etc/passwd"), None);
        assert_eq!(split_key("localhost/a/b"), None);
        assert_eq!(split_key("github.com/only-one"), None);
    }

    #[test]
    fn canonical_repo_lowercases_only_the_host() {
        assert_eq!(
            canonical_repo("GitHub.com/Owner/Repo"),
            "github.com/Owner/Repo"
        );
        assert_eq!(
            canonical_repo("github.com/alice/fifo"),
            "github.com/alice/fifo"
        );
        // Path case (which can be significant on some hosts) is preserved.
        assert_eq!(
            canonical_repo("GITLAB.com/Group/Sub/Proj"),
            "gitlab.com/Group/Sub/Proj"
        );
    }

    #[test]
    fn public_validators_guard_path_segments() {
        assert!(is_valid_project_name("fifo"));
        assert!(!is_valid_project_name("../evil"));
        assert!(!is_valid_project_name("1fifo"));
        assert!(is_valid_version("1.2.0"));
        assert!(!is_valid_version("../1.0"));
        assert!(is_valid_segment("alice"));
        assert!(!is_valid_segment(".."));
        assert!(!is_valid_segment("a/b"));
    }
}
