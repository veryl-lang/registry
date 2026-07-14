//! Shared types and validation for the Veryl registry.
//!
//! This crate is deliberately tiny and WASM-safe so the Cloudflare Worker can
//! depend on it without bloating the compiled WASM (no `regex`, no heavy deps).
//! The native indexer used by GitHub Actions reuses the exact same validation,
//! so the intake path and the apply path never drift.

use serde::{Deserialize, Serialize};

/// A registration request sent by `veryl publish` to the intake Worker.
///
/// The registry keys entries by `github.com/<owner>/<repo>` (globally unique),
/// not by project name (which is not unique). `version` is only a hint; the
/// crawler reads each repository's `Veryl.pub` for the authoritative version set.
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

    /// Split `github.com/<owner>/<repo>` into `(owner, repo)`.
    ///
    /// Returns `None` if the shape is wrong; callers that already ran
    /// [`Submission::validate`] can `unwrap`.
    pub fn owner_repo(&self) -> Option<(&str, &str)> {
        let rest = self.repo.strip_prefix("github.com/")?;
        let (owner, repo) = rest.split_once('/')?;
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            return None;
        }
        Some((owner, repo))
    }
}

fn validate_repo(repo: &str) -> Result<(), ValidationError> {
    let rest = repo
        .strip_prefix("github.com/")
        .ok_or_else(|| ValidationError::Repo("must start with `github.com/`".into()))?;

    let mut parts = rest.split('/');
    let owner = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err(ValidationError::Repo(
            "expected `github.com/<owner>/<repo>`".into(),
        ));
    }
    validate_segment(owner).map_err(|m| ValidationError::Repo(format!("owner: {m}")))?;
    validate_segment(name).map_err(|m| ValidationError::Repo(format!("repo: {m}")))?;
    Ok(())
}

/// A single path segment of `owner/repo`. Because these become the on-disk path
/// `registry/<owner>/<repo>.json`, the whitelist here is also the defense against
/// path traversal (`..`, `/`) reaching the index writer.
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
        assert_eq!(s.owner_repo(), Some(("alice", "fifo")));
    }

    #[test]
    fn version_is_optional() {
        assert!(sub("github.com/a/b", "b", None).validate().is_ok());
    }

    #[test]
    fn rejects_non_github_repo() {
        assert!(matches!(
            sub("gitlab.com/a/b", "b", None).validate(),
            Err(ValidationError::Repo(_))
        ));
    }

    #[test]
    fn rejects_path_traversal_in_repo() {
        for repo in ["github.com/../etc", "github.com/a/..", "github.com/a/b/c"] {
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
