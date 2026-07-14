//! Crawl the index, build docs for each published version, and generate the
//! gallery. Intended to run on a schedule in GitHub Actions.
//!
//! Docs are immutable per `(owner/repo, project, version)` and land under
//! `<docs_out>/<owner>/<repo>/<project>/<version>/`. Already-built versions are
//! skipped, so pointing `--docs-out` at a persisted checkout (e.g. `gh-pages`)
//! makes crawling incremental.

use crate::model::{self, Entry};
use anyhow::{Context, Result};
use clap::Parser;
use registry_common::{is_valid_project_name, is_valid_segment, is_valid_version};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use wait_timeout::ChildExt;
use walkdir::WalkDir;

// One pathological repository must not stall the whole crawl. Each external
// command is bounded; on timeout the child is killed and the step counts as failed.
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(60);
const DOC_TIMEOUT: Duration = Duration::from_secs(300);

/// Run `cmd`, killing it and returning `false` if it exceeds `timeout` or fails.
pub(crate) fn run_bounded(mut cmd: Command, timeout: Duration) -> bool {
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    match child.wait_timeout(timeout) {
        Ok(Some(status)) => status.success(),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
        Err(_) => false,
    }
}

#[derive(Parser)]
pub struct CrawlArgs {
    /// Index root that holds `registry/` (defaults to the current directory).
    #[arg(long, default_value = ".")]
    index_root: PathBuf,

    /// Output directory for the generated docs site (e.g. a `gh-pages` checkout).
    #[arg(long)]
    docs_out: PathBuf,

    /// Path to the `veryl` binary used to build docs.
    #[arg(long, default_value = "veryl")]
    veryl: String,

    /// Rebuild every version even if already built, ignoring the chrome stamp.
    /// A manual escape hatch for chrome changes the fingerprint cannot detect.
    #[arg(long)]
    force: bool,
}

/// A Veryl project discovered inside a cloned repository.
struct Project {
    name: String,
    dir: PathBuf,
    description: Option<String>,
    license: Option<String>,
    authors: Vec<String>,
}

/// One project's documented versions + display metadata, for the gallery.
struct GalleryProject {
    repo: String,
    project: String,
    description: Option<String>,
    license: Option<String>,
    authors: Vec<String>,
    updated: Option<String>,
    versions: Vec<String>,
}

/// Sidecar persisted at `<docs_out>/<owner>/<repo>/<project>/registry-meta.json`
/// so the gallery (derived from the docs tree) keeps metadata across runs.
#[derive(Serialize, Deserialize, Default)]
struct ProjectMeta {
    repo: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    /// Commit date (YYYY-MM-DD) of the latest release's revision.
    #[serde(default)]
    updated: Option<String>,
}

const META_FILE: &str = "registry-meta.json";

pub fn run(args: CrawlArgs) -> Result<()> {
    let registry_dir = args.index_root.join("registry");
    // Fingerprint of the injected doc-page chrome (toolbar + analytics + CSS).
    // Docs are immutable per version, but the chrome is not: when the template
    // changes, this fingerprint changes, so already-built pages are rebuilt on the
    // next crawl instead of skipped — a design PR reflects on every doc page.
    let chrome_fp = doc_chrome_fingerprint();
    // Repos whose docs should stay (live pending/active entries). Anything in the
    // docs tree not in this set — yanked, disputed, or deleted — is reconciled away.
    let mut keep: HashSet<String> = HashSet::new();

    // The crawl is read-only on the index (main): it only derives docs + the
    // gallery for gh-pages. The canonical entry is changed solely by submission
    // and moderation PRs, so there is no bot push to a protected branch.
    for entry_path in entry_files(&registry_dir) {
        let Some(entry) = load_entry(&entry_path) else {
            continue;
        };
        if entry.status == "yanked" || entry.status == "disputed" {
            continue;
        }
        let Some((owner, repo)) = split_repo(&entry.repo) else {
            continue;
        };
        // Keep this repo's docs even if the clone below fails this run.
        keep.insert(format!("{owner}/{repo}"));

        // Full clone so historical release revisions can be checked out.
        let work = std::env::temp_dir()
            .join("veryl-registry-crawl")
            .join(format!("{owner}__{repo}"));
        let _ = fs::remove_dir_all(&work);
        if !clone(&entry.repo, &work) {
            eprintln!("clone failed: {}", entry.repo);
            continue;
        }

        let projects = discover_projects(&work);
        for project in &projects {
            // Versions become filesystem path segments; drop any that are not
            // valid semver before they reach `doc_dest`.
            let releases: Vec<_> = model::read_releases(&project.dir.join("Veryl.pub"))
                .into_iter()
                .filter(|r| {
                    let ok = is_valid_version(&r.version);
                    if !ok {
                        eprintln!(
                            "skipping unsafe version {:?} for {}/{}",
                            r.version, entry.repo, project.name
                        );
                    }
                    ok
                })
                .collect();
            let mut versions = Vec::new();
            for release in &releases {
                let dest = doc_dest(&args.docs_out, owner, repo, &project.name, &release.version);
                // Skip only if built with the current chrome (and not forced);
                // otherwise the template changed, so rebuild this version's pages.
                if dest.exists() {
                    if !args.force && doc_stamp_matches(&dest, &chrome_fp) {
                        versions.push(release.version.clone());
                        continue;
                    }
                    let _ = fs::remove_dir_all(&dest);
                }
                let meta = DocMeta {
                    owner,
                    repo,
                    project: &project.name,
                    version: &release.version,
                };
                match build_doc(
                    &args.veryl,
                    &work,
                    &project.dir,
                    &release.revision,
                    &dest,
                    &meta,
                ) {
                    Ok(()) => {
                        write_doc_stamp(&dest, &chrome_fp);
                        versions.push(release.version.clone());
                    }
                    Err(e) => eprintln!(
                        "doc build failed: {}/{} {}: {e}",
                        entry.repo, project.name, release.version
                    ),
                }
            }

            if !versions.is_empty() {
                // Persist metadata next to the docs so the gallery, which is
                // derived from the docs tree, survives clone failures.
                let updated = releases
                    .iter()
                    .max_by(|a, b| version_cmp(&a.version, &b.version))
                    .and_then(|r| latest_commit_date(&work, &r.revision));
                let project_meta = ProjectMeta {
                    repo: entry.repo.clone(),
                    description: project.description.clone(),
                    license: project.license.clone(),
                    authors: project.authors.clone(),
                    updated,
                };
                write_project_meta(&args.docs_out, owner, repo, &project.name, &project_meta);
            }
        }

        let _ = fs::remove_dir_all(&work);
    }

    fs::create_dir_all(&args.docs_out)
        .with_context(|| format!("creating {}", args.docs_out.display()))?;
    reconcile_docs(&args.docs_out, &keep);
    let gallery = scan_docs(&args.docs_out);
    fs::write(args.docs_out.join("index.html"), gallery_html(&gallery))
        .context("writing gallery index.html")?;
    println!("published {} documented project(s)", gallery.len());
    Ok(())
}

fn entry_files(registry_dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(registry_dir)
        .into_iter()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| e.path().to_path_buf())
        .collect()
}

fn load_entry(path: &Path) -> Option<Entry> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn split_repo(repo: &str) -> Option<(&str, &str)> {
    let rest = repo.strip_prefix("github.com/")?;
    let (owner, name) = rest.split_once('/')?;
    // `is_valid_segment` rejects empty, `.`/`..`, `/`, and out-of-charset names,
    // so a hand-edited entry cannot smuggle path traversal into the docs tree.
    (is_valid_segment(owner) && is_valid_segment(name)).then_some((owner, name))
}

fn clone(repo: &str, dir: &Path) -> bool {
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--quiet", &format!("https://{repo}")])
        .arg(dir);
    run_bounded(cmd, CLONE_TIMEOUT)
}

fn discover_projects(root: &Path) -> Vec<Project> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root).into_iter().flatten() {
        if entry.file_name() != "Veryl.toml" {
            continue;
        }
        if let Some(info) = model::read_project(entry.path()) {
            // The name becomes a filesystem path segment below; skip anything the
            // registry would not accept as a project name.
            if !is_valid_project_name(&info.name) {
                eprintln!("skipping project with unsafe name: {:?}", info.name);
                continue;
            }
            let dir = entry.path().parent().unwrap_or(root).to_path_buf();
            out.push(Project {
                name: info.name,
                dir,
                description: info.description,
                license: info.license,
                authors: info.authors,
            });
        }
    }
    out
}

fn doc_dest(docs_out: &Path, owner: &str, repo: &str, project: &str, version: &str) -> PathBuf {
    docs_out.join(owner).join(repo).join(project).join(version)
}

/// Identifies a documented version, for the injected doc toolbar.
struct DocMeta<'a> {
    owner: &'a str,
    repo: &'a str,
    project: &'a str,
    version: &'a str,
}

/// Check out `revision`, run `veryl doc` in `project_dir`, copy the generated
/// `doc/` into `dest`, and inject the shared registry toolbar into each page.
fn build_doc(
    veryl: &str,
    repo_root: &Path,
    project_dir: &Path,
    revision: &str,
    dest: &Path,
    meta: &DocMeta,
) -> Result<()> {
    let mut checkout = Command::new("git");
    checkout
        .arg("-C")
        .arg(repo_root)
        .args(["checkout", "--quiet", "--force", revision]);
    if !run_bounded(checkout, CHECKOUT_TIMEOUT) {
        anyhow::bail!("git checkout {revision} failed or timed out");
    }

    // `veryl doc` renders to `<project>/doc` (the default doc path).
    let doc_dir = project_dir.join("doc");
    let _ = fs::remove_dir_all(&doc_dir);

    let mut doc = Command::new(veryl);
    doc.arg("doc").current_dir(project_dir);
    if !run_bounded(doc, DOC_TIMEOUT) {
        anyhow::bail!("veryl doc failed or timed out");
    }
    if !doc_dir.is_dir() {
        anyhow::bail!("no doc output at {}", doc_dir.display());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_dir(&doc_dir, dest)?;
    inject_toolbar(dest, meta);
    Ok(())
}

// Toolbar chrome shared by every doc page (docs.rs-style). It is a sticky bar
// injected above the untouched `veryl doc` output; the two overrides keep
// mdbook's own sticky menu bar and fixed sidebar below it.
const TOOLBAR_CSS: &str = "\
:root{--vlr-h:44px}\n\
.vlr-bar{position:sticky;top:0;z-index:100000;height:var(--vlr-h);box-sizing:border-box;display:flex;align-items:center;gap:.6rem;padding:0 1rem;background:#24262a;color:#e6e8eb;font-family:\"Fira Sans\",system-ui,sans-serif;font-size:14px;line-height:1;border-bottom:2px solid #2baa59}\n\
.vlr-bar a{color:#e6e8eb;text-decoration:none}\n\
.vlr-bar a:hover{text-decoration:underline}\n\
.vlr-home{font-weight:600;color:#fff;display:inline-flex;align-items:center;gap:.45rem;white-space:nowrap}\n\
.vlr-dot{width:9px;height:9px;border-radius:50%;background:#2baa59}\n\
.vlr-pkg{color:#9aa3ad;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}\n\
.vlr-pkg a{color:#9aa3ad}\n\
.vlr-pkg b{color:#e6e8eb;font-weight:600}\n\
.vlr-ver{color:#5fd28a;font-weight:600;white-space:nowrap}\n\
.vlr-spacer{margin-left:auto}\n\
.vlr-ext{color:#5fd28a}\n\
#mdbook-menu-bar.sticky{top:var(--vlr-h)!important}\n\
#mdbook-sidebar{top:var(--vlr-h)!important;height:calc(100vh - var(--vlr-h))!important}\n\
html{scroll-padding-top:calc(var(--vlr-h) + 3rem)}\n";

fn toolbar_html(meta: &DocMeta) -> String {
    format!(
        "<div class=\"vlr-bar\">\
         <a class=\"vlr-home\" href=\"../../../../\"><span class=\"vlr-dot\"></span>Veryl registry</a>\
         <span class=\"vlr-pkg\"><a href=\"https://github.com/{o}/{r}\">{o}/{r}</a> &middot; <b>{p}</b></span>\
         <span class=\"vlr-ver\">v{v}</span>\
         <span class=\"vlr-spacer\"></span>\
         <a class=\"vlr-ext\" href=\"https://github.com/{o}/{r}\">Source &#8599;</a>\
         </div>",
        o = esc(meta.owner),
        r = esc(meta.repo),
        p = esc(meta.project),
        v = esc(meta.version)
    )
}

/// Inject the toolbar (style + bar) into every `*.html` under `dest`.
/// Best-effort: per-file failures are skipped rather than failing the build.
fn inject_toolbar(dest: &Path, meta: &DocMeta) {
    let head = format!("<style>\n{TOOLBAR_CSS}</style>\n{ANALYTICS}");
    let bar = toolbar_html(meta);
    for entry in WalkDir::new(dest).into_iter().flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let Ok(html) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let html = html.replacen("</head>", &format!("{head}</head>"), 1);
        let html = insert_after_body_open(&html, &bar);
        let _ = fs::write(entry.path(), html);
    }
}

/// Insert `snippet` immediately after the opening `<body ...>` tag.
fn insert_after_body_open(html: &str, snippet: &str) -> String {
    if let Some(start) = html.find("<body")
        && let Some(rel) = html[start..].find('>')
    {
        let pos = start + rel + 1;
        let mut out = String::with_capacity(html.len() + snippet.len() + 1);
        out.push_str(&html[..pos]);
        out.push('\n');
        out.push_str(snippet);
        out.push_str(&html[pos..]);
        return out;
    }
    html.to_string()
}

// Stamp written into each built version dir recording the chrome fingerprint it
// was built with. A dotfile: `subdirs`/`scan_docs` ignore it, so it never looks
// like a version, and it is harmless in the published gh-pages tree.
const BUILD_STAMP: &str = ".vlr-build";

/// FNV-1a 64-bit. Deterministic across runs and platforms (unlike `DefaultHasher`),
/// so a stamp written by one crawl compares correctly against the next.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Fingerprint of the doc-page chrome injected by `inject_toolbar`: the CSS, the
/// analytics snippet, and the toolbar structure (rendered with a fixed dummy so a
/// change to the template — not the per-page data — moves the fingerprint).
fn doc_chrome_fingerprint() -> String {
    let dummy = DocMeta {
        owner: "o",
        repo: "r",
        project: "p",
        version: "0",
    };
    let material = format!("{TOOLBAR_CSS}\u{0}{ANALYTICS}\u{0}{}", toolbar_html(&dummy));
    format!("{:016x}", fnv1a(material.as_bytes()))
}

/// True if `dest` was built with this chrome fingerprint (so it can be skipped).
/// Missing/mismatched stamp (e.g. built before stamping existed) → rebuild.
fn doc_stamp_matches(dest: &Path, fingerprint: &str) -> bool {
    fs::read_to_string(dest.join(BUILD_STAMP))
        .map(|s| s.trim() == fingerprint)
        .unwrap_or(false)
}

fn write_doc_stamp(dest: &Path, fingerprint: &str) {
    let _ = fs::write(dest.join(BUILD_STAMP), fingerprint);
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn write_project_meta(docs_out: &Path, owner: &str, repo: &str, project: &str, meta: &ProjectMeta) {
    let dir = docs_out.join(owner).join(repo).join(project);
    let _ = fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(meta) {
        let _ = fs::write(dir.join(META_FILE), json);
    }
}

/// Commit date (`YYYY-MM-DD`) of `revision` in the local clone, if resolvable.
fn latest_commit_date(repo_root: &Path, revision: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", "-s", "--format=%cs", revision])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let date = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!date.is_empty()).then_some(date)
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(va), Ok(vb)) => va.cmp(&vb),
        _ => a.cmp(b),
    }
}

fn read_project_meta(project_dir: &Path) -> Option<ProjectMeta> {
    let text = fs::read_to_string(project_dir.join(META_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Immediate subdirectories of `dir`, ignoring files and dotdirs. Skipping
/// dotdirs matters because `--docs-out` is a `gh-pages` checkout containing
/// `.git`; owner/repo/project/version names never begin with a dot.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect()
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Build the gallery from the persisted docs tree
/// (`<docs_out>/<owner>/<repo>/<project>/<version>/`) rather than the live crawl,
/// so a clone failure or GitHub outage never drops published docs from the listing.
fn scan_docs(docs_out: &Path) -> Vec<GalleryProject> {
    let mut out = Vec::new();
    for owner in subdirs(docs_out) {
        for repo in subdirs(&owner) {
            for project in subdirs(&repo) {
                let versions: Vec<String> = subdirs(&project).iter().map(|p| dir_name(p)).collect();
                if versions.is_empty() {
                    continue;
                }
                let meta = read_project_meta(&project).unwrap_or_default();
                let repo_full = if meta.repo.is_empty() {
                    format!("github.com/{}/{}", dir_name(&owner), dir_name(&repo))
                } else {
                    meta.repo.clone()
                };
                out.push(GalleryProject {
                    repo: repo_full,
                    project: dir_name(&project),
                    description: meta.description,
                    license: meta.license,
                    authors: meta.authors,
                    updated: meta.updated,
                    versions,
                });
            }
        }
    }
    out.sort_by(|a, b| a.repo.cmp(&b.repo).then_with(|| a.project.cmp(&b.project)));
    out
}

/// Delete docs for any `<owner>/<repo>` not in `keep` (yanked, disputed, or a
/// deleted entry), then drop emptied owner directories.
fn reconcile_docs(docs_out: &Path, keep: &HashSet<String>) {
    for owner_dir in subdirs(docs_out) {
        let owner = dir_name(&owner_dir);
        for repo_dir in subdirs(&owner_dir) {
            let slug = format!("{owner}/{}", dir_name(&repo_dir));
            if !keep.contains(&slug) {
                let _ = fs::remove_dir_all(&repo_dir);
            }
        }
        if subdirs(&owner_dir).is_empty() {
            let _ = fs::remove_dir_all(&owner_dir);
        }
    }
}

// GA4 mirrored from veryl-lang.org (`G-NXW2P6CCF3`), injected into the gallery and
// every doc page's <head>. The measurement ID is a public client-side value.
const ANALYTICS: &str = "\
<script async src=\"https://www.googletagmanager.com/gtag/js?id=G-NXW2P6CCF3\"></script>\n\
<script>window.dataLayer=window.dataLayer||[];function gtag(){dataLayer.push(arguments);}gtag('js',new Date());gtag('config','G-NXW2P6CCF3');</script>\n";

// Palette and typography mirror veryl-lang.org (Fira Sans, brand green #2baa59,
// orange links #d46e13, warm off-white ground), with a dark-scheme variant.
const GALLERY_CSS: &str = "\
:root{--bg:#fcfaf6;--fg:#24262a;--muted:#5f6b7a;--link:#d46e13;--brand:#2baa59;--border:#e4e7ec;--card:#fff;--shadow:rgba(228,231,236,.6)}\n\
@media (prefers-color-scheme:dark){:root{--bg:#161a17;--fg:#e6e8eb;--muted:#9aa3ad;--link:#5fd28a;--brand:#5fd28a;--border:#2a2f34;--card:#1e2621;--shadow:rgba(0,0,0,.3)}}\n\
*{box-sizing:border-box}\n\
body{font-family:\"Fira Sans\",system-ui,-apple-system,sans-serif;color:var(--fg);background:var(--bg);max-width:56rem;margin:0 auto;padding:2.2rem 1.25rem 4rem;line-height:1.6}\n\
.hero h1{font-size:1.9rem;margin:0;display:flex;align-items:center;gap:.55rem;font-weight:700}\n\
.hero .dot{width:.85rem;height:.85rem;border-radius:50%;background:var(--brand)}\n\
.hero .tagline{color:var(--muted);margin:.35rem 0 0}\n\
input#q{width:100%;padding:.6rem .8rem;margin:1.5rem 0 1.6rem;font:inherit;font-size:1rem;color:var(--fg);background:var(--card);border:1px solid var(--border);border-radius:8px}\n\
input#q:focus{outline:2px solid var(--brand);outline-offset:1px;border-color:var(--brand)}\n\
#noresult{color:var(--muted)}\n\
.repo{margin:0 0 1.6rem}\n\
.repo>h2{font-size:.82rem;font-weight:600;letter-spacing:.02em;margin:0 0 .5rem;color:var(--muted)}\n\
.repo>h2 a{color:var(--muted);text-decoration:none}\n\
.repo>h2 a:hover{color:var(--link)}\n\
.pkg{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:.85rem 1rem;margin:.5rem 0;box-shadow:0 1px 2px var(--shadow)}\n\
.pkg-head{display:flex;align-items:baseline;gap:.6rem;flex-wrap:wrap}\n\
.pkg-name{font-size:1.12rem;font-weight:600;color:var(--link);text-decoration:none}\n\
.pkg-name:hover{text-decoration:underline}\n\
.pkg-latest{font-size:.78rem;color:var(--brand);font-weight:600}\n\
.pkg-desc{margin:.3rem 0 .35rem;color:var(--fg)}\n\
.pkg-meta{font-size:.8rem;color:var(--muted);margin:.15rem 0 .5rem}\n\
.pkg-dep{display:flex;align-items:center;gap:.5rem;margin:.1rem 0 .6rem;background:var(--bg);border:1px solid var(--border);border-radius:6px;padding:.3rem .5rem}\n\
.pkg-dep code{flex:1;min-width:0;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.78rem;color:var(--fg);white-space:nowrap;overflow-x:auto}\n\
.pkg-dep .copy{flex:0 0 auto;font:inherit;font-size:.72rem;color:var(--muted);background:var(--card);border:1px solid var(--border);border-radius:5px;padding:.1rem .55rem;cursor:pointer}\n\
.pkg-dep .copy:hover{border-color:var(--brand);color:var(--brand)}\n\
.versions{display:flex;flex-wrap:wrap;gap:.4rem}\n\
.versions a{font-size:.76rem;color:var(--muted);text-decoration:none;border:1px solid var(--border);border-radius:999px;padding:.12rem .6rem}\n\
.versions a:hover{border-color:var(--brand);color:var(--brand)}\n\
.hidden{display:none}\n";

// Client-side package search: no backend, filters the rendered list in place.
const GALLERY_SCRIPT: &str = "\
const q=document.getElementById('q');\n\
const pkgs=[...document.querySelectorAll('.pkg')];\n\
const repos=[...document.querySelectorAll('.repo')];\n\
const noresult=document.getElementById('noresult');\n\
function filter(){\n\
  const t=q.value.trim().toLowerCase();let any=false;\n\
  for(const p of pkgs){const show=!t||p.dataset.search.includes(t);p.classList.toggle('hidden',!show);if(show)any=true;}\n\
  for(const r of repos){r.classList.toggle('hidden',!r.querySelector('.pkg:not(.hidden)'));}\n\
  if(noresult)noresult.classList.toggle('hidden',any||!t);\n\
}\n\
q.addEventListener('input',filter);\n\
document.querySelectorAll('.copy').forEach(function(b){b.addEventListener('click',function(){navigator.clipboard.writeText(b.previousElementSibling.textContent).then(function(){b.textContent='Copied';setTimeout(function(){b.textContent='Copy';},1200);});});});\n";

fn gallery_html(projects: &[GalleryProject]) -> String {
    let mut by_repo: BTreeMap<&str, Vec<&GalleryProject>> = BTreeMap::new();
    for p in projects {
        by_repo.entry(p.repo.as_str()).or_default().push(p);
    }

    let mut body = String::new();
    for (repo, projs) in &by_repo {
        let slug = repo.strip_prefix("github.com/").unwrap_or(repo);
        body.push_str(&format!(
            "<section class=\"repo\"><h2><a href=\"https://{}\">{}</a></h2>\n",
            esc(repo),
            esc(slug)
        ));
        for p in projs {
            let mut versions = p.versions.clone();
            sort_versions_desc(&mut versions);
            let latest = versions.first().cloned().unwrap_or_default();
            // Searchable text (name + repo + description + license + authors).
            let haystack = format!(
                "{} {} {} {} {}",
                p.project,
                slug,
                p.description.as_deref().unwrap_or(""),
                p.license.as_deref().unwrap_or(""),
                p.authors.join(" ")
            )
            .to_lowercase();
            body.push_str(&format!(
                "<div class=\"pkg\" data-search=\"{}\">\n",
                esc(&haystack)
            ));
            body.push_str(&format!(
                "<div class=\"pkg-head\"><a class=\"pkg-name\" href=\"{s}/{p}/{l}/\">{p}</a><span class=\"pkg-latest\">v{l}</span></div>\n",
                s = esc(slug),
                p = esc(&p.project),
                l = esc(&latest)
            ));
            if let Some(desc) = p.description.as_deref().filter(|d| !d.trim().is_empty()) {
                body.push_str(&format!("<p class=\"pkg-desc\">{}</p>\n", esc(desc)));
            }
            let mut meta_parts: Vec<String> = Vec::new();
            if let Some(lic) = p.license.as_deref().filter(|s| !s.trim().is_empty()) {
                meta_parts.push(esc(lic));
            }
            if let Some(up) = p.updated.as_deref().filter(|s| !s.trim().is_empty()) {
                meta_parts.push(format!("updated {}", esc(up)));
            }
            if !p.authors.is_empty() {
                meta_parts.push(esc(&p.authors.join(", ")));
            }
            if !meta_parts.is_empty() {
                body.push_str(&format!(
                    "<p class=\"pkg-meta\">{}</p>\n",
                    meta_parts.join(" · ")
                ));
            }
            // Copy-pasteable `[dependencies]` entry (key = project name; the
            // resolver finds the project by name inside the repo).
            let dep = format!(
                "{} = {{ github = \"{slug}\", version = \"{latest}\" }}",
                p.project
            );
            body.push_str(&format!(
                "<div class=\"pkg-dep\"><code>{}</code><button class=\"copy\">Copy</button></div>\n",
                esc(&dep)
            ));
            body.push_str("<div class=\"versions\">\n");
            for v in &versions {
                body.push_str(&format!(
                    "<a href=\"{s}/{p}/{v}/\">{v}</a>\n",
                    s = esc(slug),
                    p = esc(&p.project),
                    v = esc(v)
                ));
            }
            body.push_str("</div>\n</div>\n");
        }
        body.push_str("</section>\n");
    }

    if by_repo.is_empty() {
        body.push_str("<p id=\"empty\">No projects yet.</p>\n");
    }

    let mut html = String::new();
    html.push_str(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Veryl registry</title>\n<style>\n",
    );
    html.push_str(GALLERY_CSS);
    html.push_str("</style>\n");
    html.push_str(ANALYTICS);
    html.push_str("</head>\n<body>\n");
    html.push_str(
        "<header class=\"hero\"><h1><span class=\"dot\"></span>Veryl registry</h1>\
         <p class=\"tagline\">Published Veryl projects and their generated documentation.</p></header>\n",
    );
    html.push_str(
        "<input id=\"q\" type=\"search\" placeholder=\"Search packages…\" autocomplete=\"off\">\n",
    );
    html.push_str("<p id=\"noresult\" class=\"hidden\">No matching packages.</p>\n");
    html.push_str("<main>\n");
    html.push_str(&body);
    html.push_str("</main>\n<script>\n");
    html.push_str(GALLERY_SCRIPT);
    html.push_str("</script>\n</body>\n</html>\n");
    html
}

fn sort_versions_desc(versions: &mut [String]) {
    versions.sort_by(|a, b| version_cmp(b, a));
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_repo() {
        assert_eq!(split_repo("github.com/alice/fifo"), Some(("alice", "fifo")));
        assert_eq!(split_repo("gitlab.com/alice/fifo"), None);
        assert_eq!(split_repo("github.com/alice"), None);
    }

    #[test]
    fn split_repo_rejects_traversal() {
        assert_eq!(split_repo("github.com/../etc"), None);
        assert_eq!(split_repo("github.com/a/.."), None);
        assert_eq!(split_repo("github.com/a/b/c"), None);
    }

    #[test]
    fn discover_skips_unsafe_project_name() {
        let root = std::env::temp_dir().join("veryl-reg-discover-unsafe");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("good")).unwrap();
        fs::write(
            root.join("good").join("Veryl.toml"),
            "[project]\nname = \"good\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("bad")).unwrap();
        fs::write(
            root.join("bad").join("Veryl.toml"),
            "[project]\nname = \"../../evil\"\n",
        )
        .unwrap();
        let names: Vec<String> = discover_projects(&root)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["good".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn doc_dest_layout() {
        let p = doc_dest(Path::new("/out"), "alice", "fifo", "fifo", "1.2.0");
        assert!(p.ends_with("alice/fifo/fifo/1.2.0"));
    }

    #[test]
    fn versions_sort_by_semver_desc() {
        let mut v = vec!["1.9.0".into(), "1.10.0".into(), "1.2.0".into()];
        sort_versions_desc(&mut v);
        assert_eq!(v, vec!["1.10.0", "1.9.0", "1.2.0"]);
    }

    #[test]
    fn gallery_lists_projects_and_versions() {
        let g = vec![GalleryProject {
            repo: "github.com/alice/fifo".into(),
            project: "fifo".into(),
            description: Some("An async FIFO".into()),
            license: None,
            authors: vec![],
            updated: None,
            versions: vec!["1.0.0".into(), "1.2.0".into()],
        }];
        let html = gallery_html(&g);
        assert!(html.contains("https://github.com/alice/fifo"));
        assert!(html.contains("alice/fifo/fifo/1.2.0/"));
        // newest first
        let i12 = html.find(">1.2.0<").unwrap();
        let i10 = html.find(">1.0.0<").unwrap();
        assert!(i12 < i10);
        // copy-pasteable dependency snippet (github shorthand + latest version)
        assert!(html.contains("class=\"pkg-dep\""));
        assert!(html.contains("github = &quot;alice/fifo&quot;, version = &quot;1.2.0&quot;"));
    }

    #[test]
    fn gallery_has_search_and_matchable_description() {
        let g = vec![GalleryProject {
            repo: "github.com/alice/fifo".into(),
            project: "fifo".into(),
            description: Some("Cryptographic core".into()),
            license: None,
            authors: vec![],
            updated: None,
            versions: vec!["1.0.0".into()],
        }];
        let html = gallery_html(&g);
        assert!(html.contains("id=\"q\"")); // search box
        assert!(html.contains("data-search="));
        // description is searchable even though it is not displayed as text
        assert!(html.contains("cryptographic core"));
    }

    #[test]
    fn gallery_handles_empty() {
        assert!(gallery_html(&[]).contains("No projects yet"));
    }

    #[test]
    fn gallery_includes_analytics() {
        let html = gallery_html(&[]);
        assert!(html.contains("G-NXW2P6CCF3")); // GA4 (same as veryl-lang.org)
    }

    #[test]
    fn toolbar_inserts_after_body_and_links_out() {
        let meta = DocMeta {
            owner: "alice",
            repo: "fifo",
            project: "fifo",
            version: "1.2.0",
        };
        let bar = toolbar_html(&meta);
        assert!(bar.contains("href=\"../../../../\"")); // home = gallery root
        assert!(bar.contains("https://github.com/alice/fifo"));
        assert!(bar.contains("v1.2.0"));

        let out = insert_after_body_open("<html><head></head><body>\n<p>x</p></body>", &bar);
        // toolbar lands right after <body>, before the page content
        assert!(out.find("vlr-bar").unwrap() < out.find("<p>x").unwrap());
    }

    #[test]
    fn insert_handles_attributed_body() {
        let out = insert_after_body_open("<body class=\"light\">CONTENT", "BAR");
        assert!(out.starts_with("<body class=\"light\">"));
        assert!(out.find("BAR").unwrap() < out.find("CONTENT").unwrap());
    }

    #[test]
    fn scans_docs_tree_and_survives_stray_files() {
        let root = std::env::temp_dir().join("veryl-reg-scan-test");
        let _ = fs::remove_dir_all(&root);
        let proj = root.join("alice").join("fifo").join("fifo");
        fs::create_dir_all(proj.join("1.0.0")).unwrap();
        fs::create_dir_all(proj.join("1.2.0")).unwrap();
        write_project_meta(
            &root,
            "alice",
            "fifo",
            "fifo",
            &ProjectMeta {
                repo: "github.com/alice/fifo".into(),
                description: Some("An async FIFO".into()),
                license: Some("MIT OR Apache-2.0".into()),
                authors: vec!["alice".into()],
                updated: Some("2026-05-01".into()),
            },
        );
        // a stray root file (the gallery index) must be ignored, not treated as an owner
        fs::write(root.join("index.html"), "x").unwrap();
        // a `.git` dir (docs_out is a gh-pages checkout) must not become an owner
        fs::create_dir_all(root.join(".git").join("objects")).unwrap();

        let g = scan_docs(&root);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].repo, "github.com/alice/fifo");
        assert_eq!(g[0].project, "fifo");
        assert_eq!(g[0].description.as_deref(), Some("An async FIFO"));
        assert_eq!(g[0].license.as_deref(), Some("MIT OR Apache-2.0"));
        assert_eq!(g[0].updated.as_deref(), Some("2026-05-01"));
        assert_eq!(g[0].authors, vec!["alice".to_string()]);
        let mut v = g[0].versions.clone();
        v.sort();
        assert_eq!(v, vec!["1.0.0", "1.2.0"]);

        // and it renders into the gallery
        let html = gallery_html(&g);
        assert!(html.contains("An async FIFO"));
        assert!(html.contains("MIT OR Apache-2.0"));
        assert!(html.contains("updated 2026-05-01"));
        assert!(html.contains("alice/fifo/fifo/1.2.0/"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_removes_docs_absent_from_keep() {
        let root = std::env::temp_dir().join("veryl-reg-reconcile-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("alice").join("keep").join("keep").join("1.0.0")).unwrap();
        fs::create_dir_all(root.join("bob").join("gone").join("gone").join("1.0.0")).unwrap();
        fs::create_dir_all(root.join(".git").join("objects")).unwrap(); // must be left alone

        let mut keep = HashSet::new();
        keep.insert("alice/keep".to_string());
        reconcile_docs(&root, &keep);

        assert!(root.join("alice").join("keep").exists()); // kept
        assert!(!root.join("bob").join("gone").exists()); // removed (yanked/deleted)
        assert!(!root.join("bob").exists()); // emptied owner dir dropped
        assert!(root.join(".git").join("objects").exists()); // .git never touched

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn chrome_fingerprint_is_stable_and_stamps_roundtrip() {
        let fp = doc_chrome_fingerprint();
        assert_eq!(fp, doc_chrome_fingerprint()); // deterministic
        assert_eq!(fp.len(), 16);
        assert!(fp.bytes().all(|c| c.is_ascii_hexdigit()));

        let dir = std::env::temp_dir().join("veryl-reg-stamp-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(!doc_stamp_matches(&dir, &fp)); // no stamp yet -> rebuild
        write_doc_stamp(&dir, &fp);
        assert!(doc_stamp_matches(&dir, &fp)); // matches after stamping -> skip
        assert!(!doc_stamp_matches(&dir, "deadbeefdeadbeef")); // changed chrome -> rebuild
        let _ = fs::remove_dir_all(&dir);
    }
}
