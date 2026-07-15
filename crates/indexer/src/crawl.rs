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
use maud::{DOCTYPE, Markup, PreEscaped, html};
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
    categories: Vec<String>,
}

/// One project's documented versions + display metadata, for the gallery.
struct GalleryProject {
    repo: String,
    project: String,
    description: Option<String>,
    license: Option<String>,
    authors: Vec<String>,
    categories: Vec<String>,
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
    /// Recognized category slugs (see `CATEGORIES`).
    #[serde(default)]
    categories: Vec<String>,
    /// Commit date (YYYY-MM-DD) of the latest release's revision.
    #[serde(default)]
    updated: Option<String>,
}

const META_FILE: &str = "registry-meta.json";

/// Canonical public origin, without a trailing slash.
const SITE_URL: &str = "https://registry.veryl-lang.org";

/// Meta description / social-card summary for the gallery home page.
const GALLERY_DESC: &str = "Browse published Veryl HDL projects and their generated documentation.";

/// Site favicon (the Veryl logo), published at the site root.
const FAVICON: &[u8] = include_bytes!("assets/favicon.png");

/// The controlled category vocabulary; anything outside it is dropped.
const CATEGORIES: &[(&str, &str)] = &[
    ("processor", "Processor"),
    ("peripheral", "Peripheral"),
    ("interconnect", "Interconnect / Bus"),
    ("interface", "Interface / Connectivity"),
    ("memory", "Memory"),
    ("arithmetic", "Arithmetic / Math"),
    ("crypto", "Crypto / Security"),
    ("verification", "Verification"),
    ("system", "System / Reference Design"),
    ("utility", "Utility"),
    ("tooling", "Tooling"),
];

/// Recognized slugs only, in canonical order so cards list them consistently.
fn normalize_categories(raw: &[String]) -> Vec<String> {
    let wanted: HashSet<String> = raw.iter().map(|c| c.trim().to_lowercase()).collect();
    CATEGORIES
        .iter()
        .filter(|(slug, _)| wanted.contains(*slug))
        .map(|(slug, _)| slug.to_string())
        .collect()
}

/// Display name for a category slug, or the slug itself if unknown.
fn category_display(slug: &str) -> &str {
    CATEGORIES
        .iter()
        .find(|(s, _)| *s == slug)
        .map_or(slug, |(_, d)| d)
}

/// The controlled vocabulary as a JSON array of `{slug, display}`, published at
/// the site root so clients can validate categories against it.
fn categories_json() -> String {
    let items: Vec<_> = CATEGORIES
        .iter()
        .map(|(slug, display)| serde_json::json!({ "slug": slug, "display": display }))
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

/// `sitemap.xml` for the gallery home and every documented version. Path
/// segments are charset-validated upstream, so no XML escaping is needed.
fn sitemap_xml(projects: &[GalleryProject]) -> String {
    let mut urls = format!("  <url><loc>{SITE_URL}/</loc></url>\n");
    for p in projects {
        let slug = p.repo.strip_prefix("github.com/").unwrap_or(&p.repo);
        // `updated` is the latest release's date, so only the newest version can
        // honestly carry it as <lastmod>.
        let latest_mod = p
            .updated
            .as_deref()
            .map(|d| format!("<lastmod>{d}</lastmod>"))
            .unwrap_or_default();
        let mut versions = p.versions.clone();
        sort_versions_desc(&mut versions);
        for (i, v) in versions.iter().enumerate() {
            let lastmod = if i == 0 { latest_mod.as_str() } else { "" };
            urls.push_str(&format!(
                "  <url><loc>{SITE_URL}/{slug}/{}/{v}/</loc>{lastmod}</url>\n",
                p.project
            ));
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n\
         {urls}</urlset>\n"
    )
}

fn robots_txt() -> String {
    format!("User-agent: *\nAllow: /\nSitemap: {SITE_URL}/sitemap.xml\n")
}

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
                    authors: sanitize_authors(&project.authors),
                    categories: normalize_categories(&project.categories),
                    updated,
                };
                write_project_meta(&args.docs_out, owner, repo, &project.name, &project_meta);

                // Immutable pages fetch this at runtime, so they still list versions
                // published after they were built.
                sort_versions_desc(&mut versions);
                write_versions_json(&args.docs_out, owner, repo, &project.name, &versions);
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
    // Publish the vocabulary so clients (e.g. `veryl register`) can warn about
    // categories this registry does not recognize before submitting.
    fs::write(args.docs_out.join("categories.json"), categories_json())
        .context("writing categories.json")?;
    fs::write(args.docs_out.join("favicon.png"), FAVICON).context("writing favicon.png")?;
    fs::write(args.docs_out.join("sitemap.xml"), sitemap_xml(&gallery))
        .context("writing sitemap.xml")?;
    fs::write(args.docs_out.join("robots.txt"), robots_txt()).context("writing robots.txt")?;
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
                categories: info.categories,
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
.vlr-ver{color:#5fd28a;font-weight:600;background:#2f3237;border:1px solid #3a3d42;border-radius:4px;padding:2px 4px;font-family:inherit;font-size:13px;cursor:pointer}\n\
.vlr-spacer{margin-left:auto}\n\
.vlr-ext{color:#5fd28a}\n\
#mdbook-menu-bar.sticky{top:var(--vlr-h)!important}\n\
#mdbook-sidebar{top:var(--vlr-h)!important;height:calc(100vh - var(--vlr-h))!important}\n\
html{scroll-padding-top:calc(var(--vlr-h) + 3rem)}\n";

// Version switcher. Absolute paths (`data-base`) work at any page depth; on change
// it keeps the current sub-page in the target version, falling back to that version's
// index. A const, not a format-string literal, so its braces need no escaping.
const VER_SWITCH_JS: &str = "<script>(function(){var s=document.querySelector('.vlr-ver');if(!s){return;}var base=s.dataset.base,cur=s.dataset.current;fetch(base+'versions.json').then(function(r){return r.ok?r.json():[];}).then(function(a){if(!a.length){return;}s.innerHTML='';a.forEach(function(v){var o=document.createElement('option');o.value=v;o.textContent='v'+v;if(v===cur){o.selected=true;}s.appendChild(o);});}).catch(function(){});s.addEventListener('change',function(){var v=s.value;if(!v||v===cur){return;}var prefix=base+cur+'/';var sub=location.pathname.indexOf(prefix)===0?location.pathname.slice(prefix.length):'';var t=base+v+'/'+sub;fetch(t,{method:'HEAD'}).then(function(r){location.assign(r.ok?t:base+v+'/');}).catch(function(){location.assign(base+v+'/');});});})();</script>";

fn toolbar_html(meta: &DocMeta) -> String {
    format!(
        "<div class=\"vlr-bar\">\
         <a class=\"vlr-home\" href=\"/\"><span class=\"vlr-dot\"></span>Veryl registry</a>\
         <span class=\"vlr-pkg\"><a href=\"https://github.com/{o}/{r}\">{o}/{r}</a> &middot; <b>{p}</b></span>\
         <select class=\"vlr-ver\" aria-label=\"Version\" data-current=\"{v}\" data-base=\"/{o}/{r}/{p}/\"><option value=\"{v}\">v{v}</option></select>\
         <span class=\"vlr-spacer\"></span>\
         <a class=\"vlr-ext\" href=\"https://github.com/{o}/{r}\">Source &#8599;</a>\
         {js}\
         </div>",
        o = esc(meta.owner),
        r = esc(meta.repo),
        p = esc(meta.project),
        v = esc(meta.version),
        js = VER_SWITCH_JS,
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

/// Drop emails so the registry never republishes contact addresses:
/// `"Name <email>"` keeps `"Name"`, a bare email is dropped.
fn sanitize_authors(authors: &[String]) -> Vec<String> {
    authors
        .iter()
        .filter_map(|a| {
            let name = a.split('<').next().unwrap_or("").trim();
            (!name.is_empty() && !name.contains('@')).then(|| name.to_string())
        })
        .collect()
}

fn write_project_meta(docs_out: &Path, owner: &str, repo: &str, project: &str, meta: &ProjectMeta) {
    let dir = docs_out.join(owner).join(repo).join(project);
    let _ = fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(meta) {
        let _ = fs::write(dir.join(META_FILE), json);
    }
}

const VERSIONS_FILE: &str = "versions.json";

/// The doc toolbar fetches this to populate its version dropdown.
fn write_versions_json(
    docs_out: &Path,
    owner: &str,
    repo: &str,
    project: &str,
    versions: &[String],
) {
    let dir = docs_out.join(owner).join(repo).join(project);
    let _ = fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(versions) {
        let _ = fs::write(dir.join(VERSIONS_FILE), json);
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
                    // Also on read: a sidecar a clone-failure crawl couldn't rewrite
                    // must not leak emails.
                    authors: sanitize_authors(&meta.authors),
                    // Re-normalize on read: never show a slug outside the vocabulary.
                    categories: normalize_categories(&meta.categories),
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
const GALLERY_CSS: &str = include_str!("assets/gallery.css");

// Client-side package search: no backend, filters the rendered list in place.
const GALLERY_SCRIPT: &str = include_str!("assets/gallery.js");

fn gallery_html(projects: &[GalleryProject]) -> String {
    let mut by_repo: BTreeMap<&str, Vec<&GalleryProject>> = BTreeMap::new();
    for p in projects {
        by_repo.entry(p.repo.as_str()).or_default().push(p);
    }

    // The facet bar shows the whole vocabulary; only slugs a project uses are
    // interactive filters.
    let in_use: HashSet<&str> = projects
        .iter()
        .flat_map(|p| p.categories.iter().map(String::as_str))
        .collect();

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Veryl registry" }
                meta name="description" content=(GALLERY_DESC);
                link rel="canonical" href=(format!("{SITE_URL}/"));
                link rel="icon" type="image/png" href="/favicon.png";
                meta property="og:type" content="website";
                meta property="og:site_name" content="Veryl registry";
                meta property="og:title" content="Veryl registry";
                meta property="og:description" content=(GALLERY_DESC);
                meta property="og:url" content=(format!("{SITE_URL}/"));
                meta property="og:image" content=(format!("{SITE_URL}/favicon.png"));
                meta name="twitter:card" content="summary";
                meta name="twitter:title" content="Veryl registry";
                meta name="twitter:description" content=(GALLERY_DESC);
                meta name="twitter:image" content=(format!("{SITE_URL}/favicon.png"));
                style { (PreEscaped(GALLERY_CSS)) }
                (PreEscaped(ANALYTICS))
                (gallery_jsonld(projects))
            }
            body {
                header.hero {
                    h1 { span.dot {} "Veryl registry" }
                    p.tagline { "Published Veryl projects and their generated documentation." }
                }
                input #q type="search" placeholder="Search packages…" autocomplete="off";
                div.catbar {
                    button.catf.active data-cat="" { "All" }
                    @for &(slug, disp) in CATEGORIES {
                        button.catf data-cat=(slug) disabled[!in_use.contains(slug)] { (disp) }
                    }
                }
                p #noresult.hidden { "No matching packages." }
                main {
                    @for (repo, projs) in &by_repo {
                        @let slug = repo.strip_prefix("github.com/").unwrap_or(repo);
                        section.repo {
                            h2 { a href=(format!("https://{repo}")) { (slug) } }
                            @for p in projs {
                                (project_card(p, slug))
                            }
                        }
                    }
                    @if by_repo.is_empty() {
                        p #empty { "No projects yet." }
                    }
                }
                script { (PreEscaped(GALLERY_SCRIPT)) }
            }
        }
    };
    markup.into_string()
}

/// schema.org `ItemList` of the documented projects, embedded as JSON-LD for
/// rich search results. `<` is escaped so a description cannot close the script.
fn gallery_jsonld(projects: &[GalleryProject]) -> Markup {
    let items: Vec<serde_json::Value> = projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let slug = p.repo.strip_prefix("github.com/").unwrap_or(&p.repo);
            let mut versions = p.versions.clone();
            sort_versions_desc(&mut versions);
            let latest = versions.first().cloned().unwrap_or_default();
            serde_json::json!({
                "@type": "ListItem",
                "position": i + 1,
                "item": {
                    "@type": "SoftwareSourceCode",
                    "name": p.project,
                    "description": p.description.clone().unwrap_or_default(),
                    "url": format!("{SITE_URL}/{slug}/{}/{latest}/", p.project),
                    "codeRepository": format!("https://{}", p.repo),
                    "programmingLanguage": "Veryl",
                }
            })
        })
        .collect();
    let doc = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "ItemList",
        "name": "Veryl registry",
        "itemListElement": items,
    });
    let json = serde_json::to_string(&doc)
        .unwrap_or_default()
        .replace('<', "\\u003c");
    html! {
        script type="application/ld+json" { (PreEscaped(json)) }
    }
}

/// One gallery card for a documented project.
fn project_card(p: &GalleryProject, slug: &str) -> Markup {
    let mut versions = p.versions.clone();
    sort_versions_desc(&mut versions);
    let latest = versions.first().cloned().unwrap_or_default();

    let cat_search = p
        .categories
        .iter()
        .map(|c| format!("{} {}", c, category_display(c)))
        .collect::<Vec<_>>()
        .join(" ");
    let haystack = format!(
        "{} {} {} {} {} {}",
        p.project,
        slug,
        p.description.as_deref().unwrap_or(""),
        p.license.as_deref().unwrap_or(""),
        p.authors.join(" "),
        cat_search,
    )
    .to_lowercase();

    let mut meta_parts: Vec<String> = Vec::new();
    if let Some(lic) = p.license.as_deref().filter(|s| !s.trim().is_empty()) {
        meta_parts.push(lic.to_string());
    }
    if let Some(up) = p.updated.as_deref().filter(|s| !s.trim().is_empty()) {
        meta_parts.push(format!("updated {up}"));
    }
    if !p.authors.is_empty() {
        meta_parts.push(p.authors.join(", "));
    }

    // Copy-pasteable `[dependencies]` entry (key = project name; the resolver
    // finds the project by name inside the repo).
    let dep = format!(
        "{} = {{ github = \"{slug}\", version = \"{latest}\" }}",
        p.project
    );
    let href = format!("{slug}/{}/{latest}/", p.project);

    html! {
        div.pkg data-search=(haystack) data-cats=(p.categories.join(" ")) {
            div.pkg-head {
                a.pkg-name href=(href) { (p.project) }
                span.pkg-latest { "v" (latest) }
            }
            @if let Some(desc) = p.description.as_deref().filter(|d| !d.trim().is_empty()) {
                p.pkg-desc { (desc) }
            }
            @if !meta_parts.is_empty() {
                p.pkg-meta { (meta_parts.join(" · ")) }
            }
            div.pkg-dep {
                code { (dep) }
                button.copy { "Copy" }
            }
            @if !p.categories.is_empty() {
                div.cats {
                    @for c in &p.categories {
                        button.cat data-cat=(c) { (category_display(c)) }
                    }
                }
            }
        }
    }
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
            categories: vec!["memory".into()],
            updated: None,
            versions: vec!["1.0.0".into(), "1.2.0".into()],
        }];
        let html = gallery_html(&g);
        assert!(html.contains("https://github.com/alice/fifo"));
        // the card links to the latest version's docs and shows it as the badge
        assert!(html.contains("alice/fifo/fifo/1.2.0/"));
        assert!(html.contains("v1.2.0"));
        // per-version chips were removed (switching moved to the doc toolbar), so
        // older versions are not linked from the card
        assert!(!html.contains("alice/fifo/fifo/1.0.0/"));
        // category chip is a clickable button carrying its slug, and searchable
        assert!(html.contains("class=\"cat\" data-cat=\"memory\">Memory<"));
        assert!(html.contains("data-search=") && html.to_lowercase().contains("memory"));
        // filter facet for the present category + card carries its slugs
        assert!(html.contains("class=\"catf\" data-cat=\"memory\">Memory<"));
        assert!(html.contains("data-cats=\"memory\""));
        // the whole vocabulary is shown; unused categories are disabled, not hidden
        assert!(html.contains("data-cat=\"processor\" disabled>Processor<"));
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
            categories: vec![],
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
    fn gallery_has_seo_metadata_and_jsonld() {
        let g = vec![GalleryProject {
            repo: "github.com/alice/fifo".into(),
            project: "fifo".into(),
            description: Some("A FIFO".into()),
            license: None,
            authors: vec![],
            categories: vec![],
            updated: None,
            versions: vec!["1.2.0".into()],
        }];
        let html = gallery_html(&g);
        assert!(html.contains("<meta name=\"description\""));
        assert!(html.contains("rel=\"canonical\" href=\"https://registry.veryl-lang.org/\""));
        assert!(html.contains("rel=\"icon\" type=\"image/png\" href=\"/favicon.png\""));
        assert!(html.contains("property=\"og:title\""));
        assert!(html.contains("name=\"twitter:card\""));
        // JSON-LD ItemList carrying the project as SoftwareSourceCode
        assert!(html.contains("application/ld+json"));
        assert!(html.contains("\"@type\":\"SoftwareSourceCode\""));
        assert!(html.contains("\"codeRepository\":\"https://github.com/alice/fifo\""));
        assert!(html.contains("registry.veryl-lang.org/alice/fifo/fifo/1.2.0/"));
    }

    #[test]
    fn sitemap_and_robots() {
        let g = vec![GalleryProject {
            repo: "github.com/alice/fifo".into(),
            project: "fifo".into(),
            description: None,
            license: None,
            authors: vec![],
            categories: vec![],
            updated: Some("2026-07-15".into()),
            versions: vec!["1.0.0".into(), "1.2.0".into()],
        }];
        let sm = sitemap_xml(&g);
        assert!(sm.contains("<loc>https://registry.veryl-lang.org/</loc>"));
        // lastmod only on the latest version (1.2.0); older 1.0.0 carries none
        assert!(sm.contains(
            "<loc>https://registry.veryl-lang.org/alice/fifo/fifo/1.2.0/</loc><lastmod>2026-07-15</lastmod></url>"
        ));
        assert!(
            sm.contains("<loc>https://registry.veryl-lang.org/alice/fifo/fifo/1.0.0/</loc></url>")
        );

        assert!(robots_txt().contains("Sitemap: https://registry.veryl-lang.org/sitemap.xml"));
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
        assert!(bar.contains("href=\"/\"")); // home = gallery root (absolute)
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
                categories: vec!["memory".into()],
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

    #[test]
    fn sanitize_authors_strips_emails() {
        assert_eq!(
            sanitize_authors(&[
                "Naoya Hatta <a@b.com>".into(),
                "a@b.com".into(),
                "Alice".into(),
            ]),
            vec!["Naoya Hatta".to_string(), "Alice".to_string()]
        );
        // authors given only as a bare email produce nothing (no address shown)
        assert!(sanitize_authors(&["dalance@gmail.com".into()]).is_empty());
    }

    #[test]
    fn toolbar_has_runtime_version_dropdown() {
        let meta = DocMeta {
            owner: "a",
            repo: "b",
            project: "p",
            version: "1.2.0",
        };
        let bar = toolbar_html(&meta);
        assert!(bar.contains("<select class=\"vlr-ver\""));
        assert!(bar.contains("data-current=\"1.2.0\"")); // current version marked
        assert!(bar.contains("data-base=\"/a/b/p/\"")); // absolute base for paths
        assert!(bar.contains("versions.json")); // dropdown populated at runtime
        assert!(bar.contains(">v1.2.0<")); // initial option works without JS
    }

    #[test]
    fn normalize_categories_keeps_known_canonical() {
        // case-insensitive, unknowns dropped, deduped, canonical (CATEGORIES) order
        let got = normalize_categories(&[
            "Memory".into(),
            "bogus".into(),
            "PROCESSOR".into(),
            "memory".into(),
        ]);
        assert_eq!(got, vec!["processor".to_string(), "memory".to_string()]);
        assert!(normalize_categories(&["not-a-category".into()]).is_empty());
        assert_eq!(category_display("memory"), "Memory");
    }

    #[test]
    fn categories_json_lists_every_slug() {
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(&categories_json()).expect("valid JSON");
        assert_eq!(parsed.len(), CATEGORIES.len());
        for ((slug, display), got) in CATEGORIES.iter().zip(&parsed) {
            assert_eq!(got["slug"], *slug);
            assert_eq!(got["display"], *display);
        }
    }
}
