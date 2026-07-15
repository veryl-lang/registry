//! Crawl the index, build docs for each published version, and generate the
//! gallery. Intended to run on a schedule in GitHub Actions.
//!
//! Docs are immutable per `(<host>/<path>, project, version)` and land under
//! `<docs_out>/<host>/<path>/<project>/<version>/`. Already-built versions are
//! skipped, so pointing `--docs-out` at a persisted checkout (e.g. `gh-pages`)
//! makes crawling incremental.

use crate::model::{self, Entry};
use anyhow::{Context, Result};
use clap::Parser;
use maud::{DOCTYPE, Markup, PreEscaped, html};
use registry_common::{is_valid_project_name, is_valid_version, split_key};
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

/// Sidecar persisted at `<docs_out>/<host>/<path>/<project>/registry-meta.json`
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
                "  <url><loc>{SITE_URL}/{}/{}/{v}/</loc>{lastmod}</url>\n",
                p.repo, p.project
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
        let Some((host, segments)) = split_key(&entry.repo) else {
            continue;
        };
        let path = segments.join("/");
        // Keep this repo's docs even if the clone below fails this run.
        keep.insert(entry.repo.clone());

        // Full clone so historical release revisions can be checked out.
        let work = std::env::temp_dir()
            .join("veryl-registry-crawl")
            .join(host)
            .join(segments.join("__"));
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
                let dest = doc_dest(
                    &args.docs_out,
                    host,
                    &segments,
                    &project.name,
                    &release.version,
                );
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
                    repo: &entry.repo,
                    host,
                    path: &path,
                    project: &project.name,
                    version: &release.version,
                    description: project.description.as_deref(),
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
                write_project_meta(
                    &args.docs_out,
                    host,
                    &segments,
                    &project.name,
                    &project_meta,
                );

                // Immutable pages fetch this at runtime, so they still list versions
                // published after they were built.
                sort_versions_desc(&mut versions);
                write_versions_json(&args.docs_out, host, &segments, &project.name, &versions);
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

/// Docs land at `<docs_out>/<host>/<seg…>/<project>/<version>/`. Every segment
/// was validated by `split_key`, so none can escape `docs_out`.
fn doc_dest(
    docs_out: &Path,
    host: &str,
    segments: &[&str],
    project: &str,
    version: &str,
) -> PathBuf {
    let mut dir = docs_out.join(host);
    for seg in segments {
        dir = dir.join(seg);
    }
    dir.join(project).join(version)
}

/// Identifies a documented version, for the injected doc toolbar and SEO head.
/// `host`/`path` are `repo` split at the first `/` — kept apart because the toolbar
/// picks an icon by host and shows the path as the slug.
struct DocMeta<'a> {
    repo: &'a str,
    host: &'a str,
    path: &'a str,
    project: &'a str,
    version: &'a str,
    description: Option<&'a str>,
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
.vlr-pkg svg{width:14px;height:14px;vertical-align:-2px;margin-right:.3rem}\n\
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
         <span class=\"vlr-pkg\"><a href=\"https://{repo}\">{icon}{path}</a> &middot; <b>{p}</b></span>\
         <select class=\"vlr-ver\" aria-label=\"Version\" data-current=\"{v}\" data-base=\"/{repo}/{p}/\"><option value=\"{v}\">v{v}</option></select>\
         <span class=\"vlr-spacer\"></span>\
         <a class=\"vlr-ext\" href=\"https://{repo}\">Source &#8599;</a>\
         {js}\
         </div>",
        repo = esc(meta.repo),
        icon = host_icon(meta.host),
        path = esc(meta.path),
        p = esc(meta.project),
        v = esc(meta.version),
        js = VER_SWITCH_JS,
    )
}

// GitHub is the only host Veryl special-cases (the `github` dep shorthand), so it
// alone gets a brand mark; every other host — GitLab included, since Veryl.toml
// treats it as a plain `git` source — shows a neutral git glyph, sidestepping other
// hosts' logo trademarks. Inline (no external requests), single-color via `currentColor`.
const ICON_GITHUB: &str = "<svg viewBox=\"0 0 16 16\" width=\"16\" height=\"16\" aria-hidden=\"true\"><path fill=\"currentColor\" d=\"M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0016 8c0-4.42-3.58-8-8-8z\"/></svg>";
const ICON_GIT: &str = "<svg viewBox=\"0 0 24 24\" width=\"16\" height=\"16\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\"><circle cx=\"6\" cy=\"6\" r=\"2.4\"/><circle cx=\"6\" cy=\"18\" r=\"2.4\"/><circle cx=\"18\" cy=\"9\" r=\"2.4\"/><path d=\"M6 8.4v7.2M8.4 6H13a3 3 0 0 1 3 3\"/></svg>";

/// GitHub's mark for github.com (the one host Veryl special-cases); a neutral git
/// glyph for every other host.
fn host_icon(host: &str) -> &'static str {
    if host == "github.com" {
        ICON_GITHUB
    } else {
        ICON_GIT
    }
}

/// Appended to each doc page's `<title>` for clearer search results.
const DOC_TITLE_SUFFIX: &str = " · Veryl registry";

/// schema.org `SoftwareSourceCode` for one documented version, as JSON-LD.
fn doc_jsonld(meta: &DocMeta, description: &str, page_url: &str, repo_url: &str) -> String {
    let doc = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "SoftwareSourceCode",
        "name": meta.project,
        "description": description,
        "version": meta.version,
        "url": page_url,
        "codeRepository": repo_url,
        "programmingLanguage": "Veryl",
    });
    let json = serde_json::to_string(&doc)
        .unwrap_or_default()
        .replace('<', "\\u003c");
    format!("<script type=\"application/ld+json\">{json}</script>\n")
}

/// Chrome for a doc page's `<head>`: toolbar CSS, analytics, and per-page SEO.
/// `page_url` is this page's own URL, used as its self-canonical.
fn doc_head(meta: &DocMeta, page_url: &str) -> String {
    let description = meta
        .description
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} — Veryl HDL documentation", meta.project));
    let title = format!("{} {}{DOC_TITLE_SUFFIX}", meta.project, meta.version);
    let repo_url = format!("https://{}", meta.repo);
    let jsonld = doc_jsonld(meta, &description, page_url, &repo_url);
    format!(
        "<style>\n{TOOLBAR_CSS}</style>\n{ANALYTICS}\
         <meta name=\"description\" content=\"{d}\">\n\
         <link rel=\"canonical\" href=\"{u}\">\n\
         <link rel=\"icon\" type=\"image/png\" href=\"/favicon.png\">\n\
         <meta property=\"og:type\" content=\"website\">\n\
         <meta property=\"og:site_name\" content=\"Veryl registry\">\n\
         <meta property=\"og:title\" content=\"{t}\">\n\
         <meta property=\"og:description\" content=\"{d}\">\n\
         <meta property=\"og:url\" content=\"{u}\">\n\
         <meta property=\"og:image\" content=\"{SITE_URL}/favicon.png\">\n\
         <meta name=\"twitter:card\" content=\"summary\">\n\
         {jsonld}",
        d = esc(&description),
        t = esc(&title),
        u = esc(page_url),
    )
}

/// Inject the registry chrome (SEO `<head>` + toolbar bar) into every `*.html`
/// under `dest`. Best-effort: per-file failures are skipped, not fatal.
fn inject_toolbar(dest: &Path, meta: &DocMeta) {
    let bar = toolbar_html(meta);
    let base = format!(
        "{SITE_URL}/{}/{}/{}/",
        meta.repo, meta.project, meta.version
    );
    for entry in WalkDir::new(dest).into_iter().flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let Ok(html) = fs::read_to_string(entry.path()) else {
            continue;
        };
        // index.html is served at the version root, other pages at their path.
        let rel = entry
            .path()
            .strip_prefix(dest)
            .ok()
            .and_then(|r| r.to_str())
            .unwrap_or("");
        let page_url = if rel.is_empty() || rel == "index.html" {
            base.clone()
        } else {
            format!("{base}{rel}")
        };
        // Drop mdbook's own (empty) description and favicon so ours win.
        let html = html.replacen("</title>", &format!("{DOC_TITLE_SUFFIX}</title>"), 1);
        let html = strip_tag(&html, "name=\"description\"");
        let html = strip_tag(&html, "rel=\"shortcut icon\"");
        let html = html.replacen(
            "</head>",
            &format!("{}</head>", doc_head(meta, &page_url)),
            1,
        );
        let html = insert_after_body_open(&html, &bar);
        let _ = fs::write(entry.path(), html);
    }
}

/// Remove the first tag whose opening contains `needle`. The head precedes the
/// body, so the first hit is the head tag we mean to replace; tolerant of
/// attribute order and content (unlike an exact-string match).
fn strip_tag(html: &str, needle: &str) -> String {
    let Some(at) = html.find(needle) else {
        return html.to_string();
    };
    let Some(lt) = html[..at].rfind('<') else {
        return html.to_string();
    };
    let Some(gt) = html[lt..].find('>') else {
        return html.to_string();
    };
    let mut out = html.to_string();
    out.replace_range(lt..lt + gt + 1, "");
    out
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
        repo: "h.example/o/r",
        host: "h.example",
        path: "o/r",
        project: "p",
        version: "0",
        description: None,
    };
    // Cover the whole injected chrome (SEO head, toolbar bar, title suffix) so
    // any template change bumps the stamp and rebuilds already-built doc pages.
    let material = format!(
        "{}\u{0}{}\u{0}{DOC_TITLE_SUFFIX}",
        doc_head(&dummy, "u"),
        toolbar_html(&dummy),
    );
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

/// Project dir `<docs_out>/<host>/<seg…>/<project>`; segments validated by `split_key`.
fn project_dir(docs_out: &Path, host: &str, segments: &[&str], project: &str) -> PathBuf {
    let mut dir = docs_out.join(host);
    for seg in segments {
        dir = dir.join(seg);
    }
    dir.join(project)
}

fn write_project_meta(
    docs_out: &Path,
    host: &str,
    segments: &[&str],
    project: &str,
    meta: &ProjectMeta,
) {
    let dir = project_dir(docs_out, host, segments, project);
    let _ = fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(meta) {
        let _ = fs::write(dir.join(META_FILE), json);
    }
}

const VERSIONS_FILE: &str = "versions.json";

/// The doc toolbar fetches this to populate its version dropdown.
fn write_versions_json(
    docs_out: &Path,
    host: &str,
    segments: &[&str],
    project: &str,
    versions: &[String],
) {
    let dir = project_dir(docs_out, host, segments, project);
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
/// `.git`; host/path/project/version names never begin with a dot.
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
/// (`<docs_out>/<host>/<path>/<project>/<version>/`) rather than the live crawl,
/// so a clone failure or GitHub outage never drops published docs from the listing.
///
/// A project dir is the parent of a `registry-meta.json`, so the repo path can be
/// any depth (GitLab subgroups); the sidecar's `repo` field carries the full key.
fn scan_docs(docs_out: &Path) -> Vec<GalleryProject> {
    let mut out = Vec::new();
    for entry in meta_files(docs_out) {
        let Some(project) = entry.parent() else {
            continue;
        };
        let versions: Vec<String> = subdirs(project).iter().map(|p| dir_name(p)).collect();
        if versions.is_empty() {
            continue;
        }
        let meta = read_project_meta(project).unwrap_or_default();
        if meta.repo.is_empty() {
            continue;
        }
        out.push(GalleryProject {
            repo: meta.repo,
            project: dir_name(project),
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
    out.sort_by(|a, b| a.repo.cmp(&b.repo).then_with(|| a.project.cmp(&b.project)));
    out
}

/// Every `registry-meta.json` in the docs tree, skipping dotdirs (the `gh-pages`
/// checkout's `.git`). Each marks a `<project>` dir at the end of a repo path.
fn meta_files(docs_out: &Path) -> Vec<PathBuf> {
    WalkDir::new(docs_out)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .flatten()
        .filter(|e| e.file_name() == META_FILE)
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// Remove a project's docs when its repo is no longer live, or when they sit at a
/// stale on-disk location (a pre-migration layout) that no longer matches where the
/// sidecar's key maps to — otherwise a relocated tree double-lists. An unreadable
/// sidecar is left in place: a healthy crawl rewrites it, so a transient read miss
/// must not drop a live repo's docs.
fn reconcile_docs(docs_out: &Path, keep: &HashSet<String>) {
    for meta_path in meta_files(docs_out) {
        let Some(project) = meta_path.parent() else {
            continue;
        };
        let Some(meta) = read_project_meta(project) else {
            continue;
        };
        let stale = !keep.contains(&meta.repo);
        let misplaced = match split_key(&meta.repo) {
            Some((host, segments)) => {
                project != project_dir(docs_out, host, &segments, &dir_name(project))
            }
            None => true, // an unparseable key is not a canonical entry
        };
        if stale || misplaced {
            let _ = fs::remove_dir_all(project);
        }
    }
    prune_empty_dirs(docs_out);
}

/// Remove now-empty directories bottom-up (repo paths left behind by reconcile),
/// never touching `docs_out` itself or dotdirs. `remove_dir` no-ops on non-empty.
///
/// `filter_entry` can't prune the `.git` subtree here: with `contents_first` a
/// directory is yielded after its children, so `.git/objects` would be visited
/// before `.git` is filtered. Instead every path component is checked for a
/// leading dot.
fn prune_empty_dirs(docs_out: &Path) {
    for entry in WalkDir::new(docs_out)
        .contents_first(true)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_dir() || entry.path() == docs_out {
            continue;
        }
        let rel = entry.path().strip_prefix(docs_out).unwrap_or(entry.path());
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let _ = fs::remove_dir(entry.path());
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
                        @let (host, path) = repo.split_once('/').unwrap_or(("", repo));
                        section.repo {
                            h2 {
                                a.repo-link href=(format!("https://{repo}")) {
                                    span.repo-host title=(host) { (PreEscaped(host_icon(host))) }
                                    (path)
                                }
                            }
                            @for p in projs {
                                (project_card(p))
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
                    "url": format!("{SITE_URL}/{}/{}/{latest}/", p.repo, p.project),
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
fn project_card(p: &GalleryProject) -> Markup {
    let mut versions = p.versions.clone();
    sort_versions_desc(&mut versions);
    let latest = versions.first().cloned().unwrap_or_default();
    let (host, path) = p.repo.split_once('/').unwrap_or(("", p.repo.as_str()));

    let cat_search = p
        .categories
        .iter()
        .map(|c| format!("{} {}", c, category_display(c)))
        .collect::<Vec<_>>()
        .join(" ");
    let haystack = format!(
        "{} {} {} {} {} {}",
        p.project,
        p.repo,
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

    // The dep key is the project name (the resolver finds it by name in the repo).
    // Veryl's `github` shorthand for github.com, else a full `git` URL.
    let dep = if host == "github.com" {
        format!(
            "{} = {{ github = \"{path}\", version = \"{latest}\" }}",
            p.project
        )
    } else {
        format!(
            "{} = {{ git = \"https://{}\", version = \"{latest}\" }}",
            p.project, p.repo
        )
    };
    let href = format!("{}/{}/{latest}/", p.repo, p.project);

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
        let p = doc_dest(
            Path::new("/out"),
            "github.com",
            &["alice", "fifo"],
            "fifo",
            "1.2.0",
        );
        assert!(p.ends_with("github.com/alice/fifo/fifo/1.2.0"));
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
        // the section header shows the host icon and the path (host stripped)
        assert!(html.contains("class=\"repo-host\""));
        assert!(html.contains(">alice/fifo<"));
        // the card links to the latest version's docs (host-qualified) + badge
        assert!(html.contains("github.com/alice/fifo/fifo/1.2.0/"));
        assert!(html.contains("v1.2.0"));
        // per-version chips were removed (switching moved to the doc toolbar), so
        // older versions are not linked from the card
        assert!(!html.contains("github.com/alice/fifo/fifo/1.0.0/"));
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
    fn gallery_renders_non_github_host() {
        let g = vec![GalleryProject {
            repo: "gitlab.com/acme/group/fifo".into(),
            project: "fifo".into(),
            description: None,
            license: None,
            authors: vec![],
            categories: vec![],
            updated: None,
            versions: vec!["1.2.0".into()],
        }];
        let html = gallery_html(&g);
        // host-qualified docs URL preserves the full subgroup path
        assert!(html.contains("gitlab.com/acme/group/fifo/fifo/1.2.0/"));
        // section header strips the host, keeps the full path
        assert!(html.contains(">acme/group/fifo<"));
        // non-github hosts use the full `git` URL, not the `github` shorthand
        assert!(html.contains(
            "git = &quot;https://gitlab.com/acme/group/fifo&quot;, version = &quot;1.2.0&quot;"
        ));
        assert!(!html.contains("github ="));
    }

    #[test]
    fn host_icon_brands_only_github() {
        assert_eq!(host_icon("github.com"), ICON_GITHUB);
        // every other host — gitlab.com included — uses the neutral git glyph
        assert_eq!(host_icon("gitlab.com"), ICON_GIT);
        assert_eq!(host_icon("git.example.com"), ICON_GIT);
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
        assert!(html.contains("registry.veryl-lang.org/github.com/alice/fifo/fifo/1.2.0/"));
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
            "<loc>https://registry.veryl-lang.org/github.com/alice/fifo/fifo/1.2.0/</loc><lastmod>2026-07-15</lastmod></url>"
        ));
        assert!(sm.contains(
            "<loc>https://registry.veryl-lang.org/github.com/alice/fifo/fifo/1.0.0/</loc></url>"
        ));

        assert!(robots_txt().contains("Sitemap: https://registry.veryl-lang.org/sitemap.xml"));
    }

    #[test]
    fn doc_head_has_seo() {
        let meta = DocMeta {
            repo: "github.com/alice/fifo",
            host: "github.com",
            path: "alice/fifo",
            project: "fifo",
            version: "1.2.0",
            description: Some("A FIFO core"),
        };
        let url = "https://registry.veryl-lang.org/github.com/alice/fifo/fifo/1.2.0/";
        let head = doc_head(&meta, url);
        assert!(head.contains("<meta name=\"description\" content=\"A FIFO core\">"));
        assert!(head.contains(&format!("<link rel=\"canonical\" href=\"{url}\">")));
        assert!(head.contains(&format!("content=\"{url}\""))); // og:url
        assert!(head.contains("\"@type\":\"SoftwareSourceCode\""));
        assert!(head.contains("\"version\":\"1.2.0\""));

        // falls back to a generated description when the project has none
        let none = DocMeta {
            description: None,
            ..meta
        };
        assert!(doc_head(&none, url).contains("fifo — Veryl HDL documentation"));
    }

    #[test]
    fn inject_adds_seo_and_drops_empty_description() {
        let dir = std::env::temp_dir().join("vlr-inject-seo-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("index.html"),
            "<html><head><title>fifo</title>\
             <meta name=\"description\" content=\"\">\
             <link rel=\"shortcut icon\" href=\"favicon-abc123.png\">\
             </head><body><p>x</p></body></html>",
        )
        .unwrap();
        let meta = DocMeta {
            repo: "github.com/alice/fifo",
            host: "github.com",
            path: "alice/fifo",
            project: "fifo",
            version: "1.2.0",
            description: Some("A FIFO"),
        };
        inject_toolbar(&dir, &meta);
        let out = fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(!out.contains("content=\"\"")); // mdbook's empty description dropped
        assert!(!out.contains("favicon-abc123.png")); // mdbook's default favicon dropped
        assert!(out.contains("<meta name=\"description\" content=\"A FIFO\">"));
        assert!(out.contains("<link rel=\"icon\" type=\"image/png\" href=\"/favicon.png\">"));
        assert!(out.contains("<title>fifo · Veryl registry</title>"));
        assert!(out.contains(
            "rel=\"canonical\" href=\"https://registry.veryl-lang.org/github.com/alice/fifo/fifo/1.2.0/\""
        ));
        assert!(out.contains("class=\"vlr-bar\"")); // toolbar still injected
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn toolbar_inserts_after_body_and_links_out() {
        let meta = DocMeta {
            repo: "github.com/alice/fifo",
            host: "github.com",
            path: "alice/fifo",
            project: "fifo",
            version: "1.2.0",
            description: None,
        };
        let bar = toolbar_html(&meta);
        assert!(bar.contains("href=\"/\"")); // home = gallery root (absolute)
        assert!(bar.contains("https://github.com/alice/fifo"));
        assert!(bar.contains(ICON_GITHUB)); // host mark next to the path
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
        let proj = root
            .join("github.com")
            .join("alice")
            .join("fifo")
            .join("fifo");
        fs::create_dir_all(proj.join("1.0.0")).unwrap();
        fs::create_dir_all(proj.join("1.2.0")).unwrap();
        write_project_meta(
            &root,
            "github.com",
            &["alice", "fifo"],
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
        // a stray root file (the gallery index) must be ignored, not treated as a project
        fs::write(root.join("index.html"), "x").unwrap();
        // a `.git` dir (docs_out is a gh-pages checkout) must not be scanned
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
        assert!(html.contains("github.com/alice/fifo/fifo/1.2.0/"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_removes_docs_absent_from_keep() {
        let root = std::env::temp_dir().join("veryl-reg-reconcile-test");
        let _ = fs::remove_dir_all(&root);
        // Two projects; reconcile keys each off its sidecar's full `<host>/<path>`.
        let keep_dir = root
            .join("github.com")
            .join("alice")
            .join("keep")
            .join("keep");
        fs::create_dir_all(keep_dir.join("1.0.0")).unwrap();
        fs::write(keep_dir.join("1.0.0").join("index.html"), "x").unwrap();
        write_project_meta(
            &root,
            "github.com",
            &["alice", "keep"],
            "keep",
            &ProjectMeta {
                repo: "github.com/alice/keep".into(),
                ..Default::default()
            },
        );
        let gone_dir = root
            .join("gitlab.com")
            .join("bob")
            .join("gone")
            .join("gone");
        fs::create_dir_all(gone_dir.join("1.0.0")).unwrap();
        write_project_meta(
            &root,
            "gitlab.com",
            &["bob", "gone"],
            "gone",
            &ProjectMeta {
                repo: "gitlab.com/bob/gone".into(),
                ..Default::default()
            },
        );
        fs::create_dir_all(root.join(".git").join("objects")).unwrap(); // must be left alone

        let mut keep = HashSet::new();
        keep.insert("github.com/alice/keep".to_string());
        reconcile_docs(&root, &keep);

        assert!(root.join("github.com").join("alice").join("keep").exists()); // kept
        assert!(!gone_dir.exists()); // removed (yanked/deleted)
        assert!(!root.join("gitlab.com").exists()); // emptied host path pruned
        assert!(root.join(".git").join("objects").exists()); // .git never touched

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_removes_stale_layout_of_live_repo() {
        let root = std::env::temp_dir().join("veryl-reg-reconcile-migrate");
        let _ = fs::remove_dir_all(&root);
        // Pre-migration layout `<owner>/<repo>/<project>/` whose sidecar maps to
        // `<host>/<owner>/<repo>`: even though the repo is live, this stale copy
        // must go so it can't double-list next to the canonical location.
        let stale = root.join("veryl-lang").join("vip").join("vip");
        fs::create_dir_all(stale.join("0.1.0")).unwrap();
        fs::write(stale.join("0.1.0").join("index.html"), "x").unwrap();
        let meta = ProjectMeta {
            repo: "github.com/veryl-lang/vip".into(),
            ..Default::default()
        };
        fs::write(stale.join(META_FILE), serde_json::to_string(&meta).unwrap()).unwrap();
        // The canonical new-layout copy of the same live repo.
        let canonical = root
            .join("github.com")
            .join("veryl-lang")
            .join("vip")
            .join("vip");
        fs::create_dir_all(canonical.join("0.1.0")).unwrap();
        fs::write(canonical.join("0.1.0").join("index.html"), "x").unwrap();
        write_project_meta(&root, "github.com", &["veryl-lang", "vip"], "vip", &meta);

        let mut keep = HashSet::new();
        keep.insert("github.com/veryl-lang/vip".to_string());
        reconcile_docs(&root, &keep);

        assert!(!root.join("veryl-lang").exists()); // stale layout removed + pruned
        assert!(canonical.exists()); // canonical layout kept
        // and the gallery lists the project exactly once (no duplicate card)
        assert_eq!(scan_docs(&root).len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_keeps_docs_with_unreadable_sidecar() {
        let root = std::env::temp_dir().join("veryl-reg-reconcile-corrupt");
        let _ = fs::remove_dir_all(&root);
        // A live repo whose sidecar is corrupt this run (e.g. a partial write):
        // it can't be identified, so a transient miss must not delete its docs.
        let proj = root
            .join("github.com")
            .join("alice")
            .join("fifo")
            .join("fifo");
        fs::create_dir_all(proj.join("1.0.0")).unwrap();
        fs::write(proj.join(META_FILE), "not json").unwrap();

        let mut keep = HashSet::new();
        keep.insert("github.com/alice/fifo".to_string());
        reconcile_docs(&root, &keep);

        assert!(proj.exists()); // unreadable sidecar left in place
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
            repo: "github.com/a/b",
            host: "github.com",
            path: "a/b",
            project: "p",
            version: "1.2.0",
            description: None,
        };
        let bar = toolbar_html(&meta);
        assert!(bar.contains("<select class=\"vlr-ver\""));
        assert!(bar.contains("data-current=\"1.2.0\"")); // current version marked
        assert!(bar.contains("data-base=\"/github.com/a/b/p/\"")); // absolute base for paths
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
