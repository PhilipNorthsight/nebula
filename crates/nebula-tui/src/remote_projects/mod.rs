//! Opt-in mixed PROJECTS view. The prototype only speaks the existing PTY plane:
//! no local filesystem/git/forge actions are available for either kind of row.
//! Full local tooling remains in the ordinary `nebula` TUI.
pub mod transport;
mod view;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteProject {
    pub host: String,
    pub path: String,
}

impl RemoteProject {
    pub fn validate(&self) -> Result<()> {
        if self.host.is_empty()
            || self.host.starts_with('-')
            || !self
                .host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "@._-:[]".contains(c))
        {
            bail!("use an SSH alias or user@host, not options or shell text");
        }
        if !Path::new(&self.path).is_absolute() || self.path.chars().any(char::is_control) {
            bail!("remote checkout path must be absolute and contain no control characters");
        }
        Ok(())
    }
}

fn store() -> PathBuf {
    nebula_core::paths::data_dir().join("remote_projects.json")
}

pub fn load() -> Result<Vec<RemoteProject>> {
    let bytes = match std::fs::read(store()) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let projects: Vec<RemoteProject> =
        serde_json::from_slice(&bytes).context("read remote_projects.json")?;
    for project in &projects {
        project.validate()?;
    }
    Ok(projects)
}

pub fn save_project(host: String, path: String, remove: bool) -> Result<()> {
    let project = RemoteProject { host, path };
    project.validate()?;
    let mut entries = load()?;
    entries.retain(|p| p != &project);
    if !remove {
        entries.push(project);
    }
    nebula_core::settings::write_json(&store(), &serde_json::to_value(entries)?)?;
    Ok(())
}

pub fn list() -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&load()?)?);
    Ok(())
}

pub fn run() -> Result<()> {
    let entries = load()?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(view::run(entries))
}
