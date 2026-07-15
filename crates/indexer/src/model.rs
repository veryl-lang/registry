//! Shared data model for the index: the on-disk entry and minimal lax views of
//! `Veryl.toml` / `Veryl.pub` used by both `apply` and `crawl`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// An entry file `registry/<host>/<path>.json` (the path nests for GitLab subgroups).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub repo: String,
    #[serde(default)]
    pub projects: Vec<String>,
    pub status: String,
    pub registered_at: String,
    #[serde(default)]
    pub registered_by: String,
    #[serde(default)]
    pub last_verified: Option<String>,
}

/// Minimal lax view of `Veryl.toml` (unknown fields ignored).
#[derive(Deserialize)]
struct VerylToml {
    project: VerylProject,
    #[serde(default)]
    publish: Option<VerylPublish>,
}

#[derive(Deserialize)]
struct VerylProject {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
}

#[derive(Deserialize)]
struct VerylPublish {
    register: Option<bool>,
}

/// The subset of `Veryl.toml` `[project]` the registry surfaces.
pub struct ProjectInfo {
    pub name: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub authors: Vec<String>,
    pub categories: Vec<String>,
}

/// Minimal lax view of `Veryl.pub`.
#[derive(Deserialize, Default)]
struct Pubfile {
    #[serde(default)]
    releases: Vec<Release>,
}

/// A single published release recorded in `Veryl.pub`.
#[derive(Deserialize, Clone, Debug)]
pub struct Release {
    pub version: String,
    pub revision: String,
}

/// Read `[project]` (name + description) from a `Veryl.toml`, or `None` if it
/// can't be parsed.
pub fn read_project(veryl_toml: &Path) -> Option<ProjectInfo> {
    let text = fs::read_to_string(veryl_toml).ok()?;
    let parsed: VerylToml = toml::from_str(&text).ok()?;
    Some(ProjectInfo {
        name: parsed.project.name,
        description: parsed.project.description,
        license: parsed.project.license,
        authors: parsed.project.authors,
        categories: parsed.project.categories,
    })
}

/// `[project].name` and `[publish].register` from one `Veryl.toml` parse.
/// `register` is `Some(false)` only for an explicit opt-out.
pub fn read_name_and_register(veryl_toml: &Path) -> Option<(String, Option<bool>)> {
    let text = fs::read_to_string(veryl_toml).ok()?;
    let parsed: VerylToml = toml::from_str(&text).ok()?;
    let register = parsed.publish.and_then(|p| p.register);
    Some((parsed.project.name, register))
}

/// Read the releases from a `Veryl.pub`, or an empty vector if missing/unparsable.
pub fn read_releases(veryl_pub: &Path) -> Vec<Release> {
    fs::read_to_string(veryl_pub)
        .ok()
        .and_then(|t| toml::from_str::<Pubfile>(&t).ok())
        .map(|p| p.releases)
        .unwrap_or_default()
}
