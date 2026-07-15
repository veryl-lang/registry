//! Registry indexer: `apply` writes an entry from a submission; `crawl` builds docs.
//!
//! `apply` runs on the `registry-submit` repository_dispatch: it verifies a
//! submission and writes its entry file (the workflow opens the PR). `crawl` runs
//! on a schedule to build docs + the gallery for gh-pages; it is read-only on the
//! index.
//!
//! Verification is **tolerant** (see PLAN.md): if the repository or its published
//! version is not visible yet (e.g. not pushed), the entry is written as `pending`
//! rather than failing. Re-registering after a push re-verifies it to `active`.

mod crawl;
mod model;

use anyhow::{Context, Result, anyhow};
use chrono::{SecondsFormat, Utc};
use clap::{Parser, Subcommand};
use model::Entry;
use registry_common::Submission;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use walkdir::WalkDir;

// Bound the verify clone so one pathological repository cannot stall `apply`.
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Parser)]
#[command(about = "Apply Veryl registry submissions and build docs")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Verify a submission and write its index entry.
    Apply(ApplyArgs),
    /// Crawl the index and build the docs site (read-only on the index).
    Crawl(crawl::CrawlArgs),
}

#[derive(Parser)]
struct ApplyArgs {
    /// Event payload JSON (defaults to $GITHUB_EVENT_PATH). Useful for local runs.
    #[arg(long)]
    event: Option<PathBuf>,

    /// Index root that holds `registry/` (defaults to the current directory).
    #[arg(long, default_value = ".")]
    index_root: PathBuf,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Apply(args) => apply(args),
        Cmd::Crawl(args) => crawl::run(args),
    }
}

fn apply(args: ApplyArgs) -> Result<()> {
    let mut submission = read_submission(&args)?;
    submission
        .validate()
        .map_err(|e| anyhow!("invalid submission: {e}"))?;
    // The CLI lowercases the host, but a direct API post or hand-edit may not;
    // canonicalize so a case variant can't double-register the same repo.
    submission.repo = registry_common::canonical_repo(&submission.repo);
    let (host, segments) = submission
        .parts()
        .ok_or_else(|| anyhow!("malformed repo: {}", submission.repo))?;
    // Flatten `/` to `__` for a ref-safe branch (a slashed ref would D/F-conflict).
    // Stable per repo, so re-registration updates the same PR; a collision needs
    // pathological `_`-adjacent paths.
    let slug = submission.repo.clone();
    let branch = slug.replace('/', "__");

    // Verify by clone. A failure to clone or find a published project is not
    // fatal; it just means the entry stays/becomes `pending`.
    let clone_dir = std::env::temp_dir()
        .join("veryl-registry-verify")
        .join(host)
        .join(segments.join("__"));
    let _ = fs::remove_dir_all(&clone_dir);
    let verify = if clone_repo(&submission.repo, &clone_dir) {
        inspect_repo(&clone_dir, &submission.name)
    } else {
        Verify::default()
    };
    let _ = fs::remove_dir_all(&clone_dir);

    // Respect an explicit in-repo opt-out: a third party cannot register a
    // project whose author committed `[publish] register = false`.
    if verify.opted_out {
        github_output("slug", &slug);
        github_output("branch", &branch);
        github_output("status", "opted-out");
        github_output("changed", "false");
        github_summary(&format!(
            "- **{slug}** → opted out (`[publish] register = false`)\n"
        ));
        println!("{slug}: opted out ([publish] register = false)");
        return Ok(());
    }

    let path = entry_path(&args.index_root, host, &segments);
    let existing = load_existing(&path);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let entry = build_entry(&submission, existing.as_ref(), &verify, &now);

    let mut json = serde_json::to_string_pretty(&entry)?;
    json.push('\n');
    // Only a material change (status/projects/repo) opens a PR. A re-register that
    // merely refreshes `last_verified` is a no-op, so a re-registered active entry
    // never produces a timestamp-only PR for a maintainer to merge.
    let changed = material_changed(existing.as_ref(), &entry);
    if changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
    }

    github_output("slug", &slug);
    github_output("branch", &branch);
    github_output("status", &entry.status);
    github_output("changed", &changed.to_string());
    github_summary(&format!(
        "- **{slug}** → `{}`{}\n",
        entry.status,
        if changed { "" } else { " (no change)" }
    ));
    println!("{slug}: {} (changed={changed})", entry.status);
    Ok(())
}

/// Result of inspecting a cloned repository.
#[derive(Debug, Default)]
struct Verify {
    /// The named project exists and its `Veryl.pub` has at least one release.
    verified: bool,
    /// All Veryl project names found in the repository.
    projects: Vec<String>,
    /// The named project set `[publish] register = false` (explicit opt-out).
    opted_out: bool,
}

fn build_entry(sub: &Submission, existing: Option<&Entry>, verify: &Verify, now: &str) -> Entry {
    let registered_at = existing
        .map(|e| e.registered_at.clone())
        .unwrap_or_else(|| now.to_string());
    let registered_by = existing
        .map(|e| e.registered_by.clone())
        .unwrap_or_default();

    let (status, last_verified, projects) = if verify.verified {
        (
            "active".to_string(),
            Some(now.to_string()),
            verify.projects.clone(),
        )
    } else {
        // Do not downgrade an already-verified entry on a transient clone/verify
        // miss; only brand-new unverifiable entries become `pending`.
        let status = match existing.map(|e| e.status.as_str()) {
            Some("active") => "active".to_string(),
            Some(other) if other == "yanked" || other == "disputed" => other.to_string(),
            _ => "pending".to_string(),
        };
        let last_verified = existing.and_then(|e| e.last_verified.clone());
        let projects = if verify.projects.is_empty() {
            existing.map(|e| e.projects.clone()).unwrap_or_default()
        } else {
            verify.projects.clone()
        };
        (status, last_verified, projects)
    };

    Entry {
        repo: sub.repo.clone(),
        projects,
        status,
        registered_at,
        registered_by,
        last_verified,
    }
}

/// Whether the entry changed in a way that warrants a moderation PR. `last_verified`
/// (and the preserved `registered_*`) are excluded, so re-registering an unchanged
/// project — which only refreshes the verification timestamp — is a no-op.
fn material_changed(prev: Option<&Entry>, entry: &Entry) -> bool {
    match prev {
        None => true,
        Some(p) => p.status != entry.status || p.projects != entry.projects || p.repo != entry.repo,
    }
}

/// On-disk entry path `registry/<host>/<seg1>/.../<lastseg>.json`. Every segment
/// was whitelisted by `validate_repo`, so none can escape `registry/`.
fn entry_path(index_root: &Path, host: &str, segments: &[&str]) -> PathBuf {
    let (last, rest) = segments.split_last().expect("validated: >= 2 segments");
    let mut path = index_root.join("registry").join(host);
    for seg in rest {
        path = path.join(seg);
    }
    path.join(format!("{last}.json"))
}

fn load_existing(path: &Path) -> Option<Entry> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn read_submission(args: &ApplyArgs) -> Result<Submission> {
    let event_path = args
        .event
        .clone()
        .or_else(|| std::env::var("GITHUB_EVENT_PATH").ok().map(PathBuf::from))
        .ok_or_else(|| anyhow!("no --event and no GITHUB_EVENT_PATH"))?;
    let text = fs::read_to_string(&event_path)
        .with_context(|| format!("reading event {}", event_path.display()))?;
    let event: Event = serde_json::from_str(&text).context("parsing event payload")?;
    Ok(event.client_payload)
}

#[derive(serde::Deserialize)]
struct Event {
    client_payload: Submission,
}

fn clone_repo(repo: &str, dir: &Path) -> bool {
    let url = format!("https://{repo}");
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth", "1", &url]).arg(dir);
    crawl::run_bounded(cmd, CLONE_TIMEOUT)
}

/// Walk a cloned repository, collecting every Veryl project name and checking
/// whether the named project has published at least one release (its sibling
/// `Veryl.pub` has a non-empty `releases`). That release is the author's opt-in
/// proof: a third party cannot fabricate it in someone else's repository.
fn inspect_repo(dir: &Path, name: &str) -> Verify {
    let mut projects = Vec::new();
    let mut verified = false;
    let mut opted_out = false;

    for entry in WalkDir::new(dir).into_iter().flatten() {
        if entry.file_name() != "Veryl.toml" {
            continue;
        }
        let Some((project_name, register)) = model::read_name_and_register(entry.path()) else {
            continue;
        };
        // A project name becomes a filesystem path segment in `crawl`; ignore
        // anything the registry would not accept as a name.
        if !registry_common::is_valid_project_name(&project_name) {
            continue;
        }
        if project_name == name {
            if register == Some(false) {
                opted_out = true;
            }
            if !model::read_releases(&entry.path().with_file_name("Veryl.pub")).is_empty() {
                verified = true;
            }
        }
        projects.push(project_name);
    }

    projects.sort();
    projects.dedup();
    Verify {
        verified,
        projects,
        opted_out,
    }
}

fn github_output(key: &str, value: &str) {
    if let Ok(path) = std::env::var("GITHUB_OUTPUT")
        && let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(f, "{key}={value}");
    }
}

fn github_summary(markdown: &str) {
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = write!(f, "{markdown}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(repo: &str, name: &str) -> Submission {
        Submission {
            repo: repo.into(),
            name: name.into(),
            version: None,
        }
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("veryl-registry-test-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn verified_when_named_project_has_a_release() {
        let dir = fixture("verified");
        write(
            &dir,
            "Veryl.toml",
            "[project]\nname = \"fifo\"\nversion = \"1.0.0\"\n",
        );
        write(
            &dir,
            "Veryl.pub",
            "[[releases]]\nversion = \"1.0.0\"\nrevision = \"abc\"\n",
        );
        let v = inspect_repo(&dir, "fifo");
        assert!(v.verified);
        assert_eq!(v.projects, vec!["fifo".to_string()]);
    }

    #[test]
    fn pending_when_pub_is_missing_or_empty() {
        let dir = fixture("nopub");
        write(&dir, "Veryl.toml", "[project]\nname = \"fifo\"\n");
        let v = inspect_repo(&dir, "fifo");
        assert!(!v.verified);
        assert_eq!(v.projects, vec!["fifo".to_string()]);
    }

    #[test]
    fn collects_multiple_projects_in_a_monorepo() {
        let dir = fixture("mono");
        write(&dir, "a/Veryl.toml", "[project]\nname = \"aaa\"\n");
        write(&dir, "b/Veryl.toml", "[project]\nname = \"bbb\"\n");
        write(
            &dir,
            "b/Veryl.pub",
            "[[releases]]\nversion=\"0.1.0\"\nrevision=\"r\"\n",
        );
        let v = inspect_repo(&dir, "bbb");
        assert!(v.verified);
        assert_eq!(v.projects, vec!["aaa".to_string(), "bbb".to_string()]);
    }

    #[test]
    fn detects_explicit_opt_out() {
        let dir = fixture("optout");
        write(
            &dir,
            "Veryl.toml",
            "[project]\nname = \"fifo\"\n[publish]\nregister = false\n",
        );
        assert!(inspect_repo(&dir, "fifo").opted_out);
    }

    #[test]
    fn no_opt_out_when_register_true_or_unset() {
        let on = fixture("optin");
        write(
            &on,
            "Veryl.toml",
            "[project]\nname = \"fifo\"\n[publish]\nregister = true\n",
        );
        assert!(!inspect_repo(&on, "fifo").opted_out);

        let unset = fixture("unset");
        write(&unset, "Veryl.toml", "[project]\nname = \"fifo\"\n");
        assert!(!inspect_repo(&unset, "fifo").opted_out);
    }

    #[test]
    fn new_unverified_submission_is_pending() {
        let e = build_entry(&sub("github.com/a/b", "b"), None, &Verify::default(), "T0");
        assert_eq!(e.status, "pending");
        assert_eq!(e.registered_at, "T0");
        assert_eq!(e.last_verified, None);
    }

    #[test]
    fn verification_activates_and_preserves_registered_at() {
        let existing = Entry {
            repo: "github.com/a/b".into(),
            projects: vec![],
            status: "pending".into(),
            registered_at: "T0".into(),
            registered_by: "github:a".into(),
            last_verified: None,
        };
        let v = Verify {
            verified: true,
            projects: vec!["b".into()],
            opted_out: false,
        };
        let e = build_entry(&sub("github.com/a/b", "b"), Some(&existing), &v, "T1");
        assert_eq!(e.status, "active");
        assert_eq!(e.registered_at, "T0"); // preserved
        assert_eq!(e.registered_by, "github:a"); // preserved
        assert_eq!(e.last_verified.as_deref(), Some("T1"));
    }

    #[test]
    fn active_entry_not_downgraded_on_transient_miss() {
        let existing = Entry {
            repo: "github.com/a/b".into(),
            projects: vec!["b".into()],
            status: "active".into(),
            registered_at: "T0".into(),
            registered_by: String::new(),
            last_verified: Some("T1".into()),
        };
        let e = build_entry(
            &sub("github.com/a/b", "b"),
            Some(&existing),
            &Verify::default(),
            "T2",
        );
        assert_eq!(e.status, "active"); // kept
        assert_eq!(e.last_verified.as_deref(), Some("T1")); // kept
        assert_eq!(e.projects, vec!["b".to_string()]); // kept
    }

    #[test]
    fn reregister_with_no_material_change_is_noop() {
        let prev = Entry {
            repo: "github.com/a/b".into(),
            projects: vec!["b".into()],
            status: "active".into(),
            registered_at: "T0".into(),
            registered_by: "github:a".into(),
            last_verified: Some("T1".into()),
        };
        let v = Verify {
            verified: true,
            projects: vec!["b".into()],
            opted_out: false,
        };
        // Re-verified at T2: the built entry has a fresh timestamp...
        let entry = build_entry(&sub("github.com/a/b", "b"), Some(&prev), &v, "T2");
        assert_eq!(entry.last_verified.as_deref(), Some("T2"));
        // ...but status/projects/repo are unchanged, so no PR is opened.
        assert!(!material_changed(Some(&prev), &entry));
    }

    #[test]
    fn status_or_project_change_is_material() {
        let prev = Entry {
            repo: "github.com/a/b".into(),
            projects: vec![],
            status: "pending".into(),
            registered_at: "T0".into(),
            registered_by: String::new(),
            last_verified: None,
        };
        let v = Verify {
            verified: true,
            projects: vec!["b".into()],
            opted_out: false,
        };
        // pending -> active and projects gained: a real change.
        let entry = build_entry(&sub("github.com/a/b", "b"), Some(&prev), &v, "T1");
        assert!(material_changed(Some(&prev), &entry));
        // A brand-new entry (no prior) always counts as changed.
        assert!(material_changed(None, &entry));
    }

    #[test]
    fn entry_path_nests_host_and_all_segments() {
        let root = Path::new("/idx");
        assert_eq!(
            entry_path(root, "github.com", &["alice", "fifo"]),
            root.join("registry/github.com/alice/fifo.json")
        );
        assert_eq!(
            entry_path(root, "gitlab.com", &["group", "sub", "proj"]),
            root.join("registry/gitlab.com/group/sub/proj.json")
        );
    }
}
