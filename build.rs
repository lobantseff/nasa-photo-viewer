//! Resolves the version shown in the app from git tags.
//!
//! The version is derived from `git describe` rather than `Cargo.toml`, so a
//! build always reports the commit it came from and an untagged build cannot
//! quietly claim to be a release.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Used when the source tree has no git history, e.g. an unpacked tarball.
const UNKNOWN: &str = "v0.0.0+unknown";

/// Length of the abbreviated commit hash in a development version.
///
/// Passed as a single `--abbrev=N` argument: split across two, git reads the
/// number as a commit-ish and refuses to run alongside `--dirty`.
const ABBREV: &str = "--abbrev=8";

fn main() {
    let version = describe().unwrap_or_else(|| UNKNOWN.to_string());
    println!("cargo:rustc-env=NPV_VERSION={version}");

    // Rebuild when the checked-out commit, the tags, or the staged state
    // change; ordinary source edits already trigger a rebuild by themselves.
    if let Some(git) = git_dir() {
        for path in ["HEAD", "index", "packed-refs"] {
            let path = git.join(path);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        let tags = git.join("refs/tags");
        if tags.exists() {
            println!("cargo:rerun-if-changed={}", tags.display());
        }
    }
}

fn git_dir() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if dir.is_empty() {
        return None;
    }
    Some(Path::new(&dir).to_path_buf())
}

fn describe() -> Option<String> {
    let out = Command::new("git")
        .args([
            "describe",
            "--tags",
            "--long",
            "--always",
            ABBREV,
            "--dirty=.dirty",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let raw = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some(format_version(&raw))
}

// Shared with the library so the rules are stated once and their tests run
// under `cargo test`, which never executes build scripts.
include!("src/version_format.rs");
