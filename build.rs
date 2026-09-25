//! Bakes the git revision into the binary for `--version`, like `git describe`:
//! nothing on a tagged commit, `<commit>` otherwise, plus `-dirty` for
//! uncommitted changes to the files the binary is built from (edits to docs do
//! not change the binary, and watching them would rebuild on every doc edit).
//! Nix builds have no .git and pass JELLYSYNC_GIT_REV.

use std::process::Command;

/// Files the binary is built from.
const BUILD_INPUTS: [&str; 4] = ["src", "Cargo.toml", "Cargo.lock", "build.rs"];

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn revision() -> String {
    if let Ok(revision) = std::env::var("JELLYSYNC_GIT_REV") {
        return revision;
    }
    let Some(commit) = git(&["rev-parse", "--short", "HEAD"]) else {
        return String::new();
    };
    let mut status = vec!["status", "--porcelain", "--"];
    status.extend(BUILD_INPUTS);
    let dirty = git(&status).is_some_and(|status| !status.is_empty());
    let tagged = git(&["describe", "--tags", "--exact-match", "HEAD"]).is_some();
    match (tagged, dirty) {
        (true, false) => String::new(),
        (_, true) => format!("{commit}-dirty"),
        (false, false) => commit,
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=JELLYSYNC_GIT_REV");
    for path in [".git/HEAD", ".git/index", ".git/refs/tags"]
        .into_iter()
        .chain(BUILD_INPUTS)
    {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rustc-env=JELLYSYNC_REVISION={}", revision());
}
