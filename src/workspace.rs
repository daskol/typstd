//! Typst workspace managing.
//!
//! This module contains basic methods to search and load workspaces and
//! copilation targets.

use std::fs;
use std::path::{Path, PathBuf};
use std::result::Result;

use log::warn;
use serde::Deserialize;

/// Filename of descriptor file (documents, packages, etc).
pub static FILENAME: &str = "typst.toml";

#[derive(Debug, Deserialize)]
pub struct TypstDocument {
    pub entrypoint: String,
    pub root_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TypstPackage {
    pub entrypoint: String,
}

/// TypstProject type represents a configuration file deserialized from
/// `typst.toml` which describes a list of documents to compile or package(s).
#[derive(Debug, Deserialize)]
pub struct TypstProject {
    #[serde(rename = "document")]
    pub documents: Vec<TypstDocument>,
    pub package: Option<TypstPackage>,
}

/// Target represents a compilation target for a particular main file located
/// at specific root directory.
pub struct Target {
    pub root_dir: PathBuf,
    pub main_file: PathBuf,
}

pub fn load_targets(root_dir: &Path) -> Result<Vec<Target>, String> {
    let path = root_dir.join(FILENAME);
    let bytes = fs::read(&path)
        .map_err(|err| format!("failed to read {path:?}: {err}"))?;
    let runes = std::str::from_utf8(&bytes)
        .map_err(|err| format!("failed to decode utf-8 at {path:?}: {err}"))?;
    let config = toml::from_str::<TypstProject>(runes)
        .map_err(|err| format!("failed to parse toml at {path:?}: {err}"))?;

    let targets = config
        .documents
        .iter()
        .map(|doc| Target {
            root_dir: doc.root_dir.clone().map_or_else(
                || root_dir.to_path_buf(),
                |dir| PathBuf::from(dir),
            ),
            main_file: root_dir.join(&doc.entrypoint),
        })
        .collect();

    Ok(targets)
}

// Search `typst.toml` files in specified directories and load targets from
// them (entrypoint + root directory).
pub fn search_targets(root_dirs: Vec<&Path>) -> Vec<Target> {
    let mut targets = Vec::<Target>::new();
    for root_dir in root_dirs.iter() {
        match load_targets(root_dir) {
            Ok(loaded) => targets.extend(loaded),
            Err(err) => {
                warn!("failed to load targets from {:?}: {}", root_dir, err);
            }
        };
    }
    targets
}

// Search workspace which is determined by `typst.toml` file.
pub fn search_workspace(start_dir: &Path) -> Option<&Path> {
    let mut root_dir = start_dir;
    let path = root_dir.join(FILENAME);
    if path.exists() && path.is_file() {
        return Some(root_dir);
    }
    while let Some(parent_dir) = root_dir.parent() {
        let path = root_dir.join(FILENAME);
        if path.exists() && path.is_file() {
            return Some(root_dir);
        }
        root_dir = parent_dir;
    }
    None
}
