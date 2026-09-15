//! Stamps the binary with the commit it was built from.
//!
//! A version number alone cannot answer "is the daemon I am talking to the
//! one I just built?", which is the question that actually comes up: the
//! workspace version changes rarely, but the binary changes every rebuild.

use std::process::Command;

fn main() {
    // Without this the stamp is captured once and then survives every
    // subsequent commit, which is worse than having no stamp at all.
    for path in [".git/HEAD", ".git/refs/heads"] {
        println!("cargo:rerun-if-changed=../../{path}");
    }

    println!("cargo:rustc-env=BOOMBOX_COMMIT={}", git(&["rev-parse", "--short=9", "HEAD"]));
    println!("cargo:rustc-env=BOOMBOX_BUILD={}", build_number());
    println!(
        "cargo:rustc-env=BOOMBOX_COMMIT_DATE={}",
        git(&["log", "-1", "--date=format:%Y-%m-%d", "--format=%cd"])
    );

    // A dirty tree means the binary does not correspond to any commit, so
    // the hash on its own would be a lie.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    println!("cargo:rustc-env=BOOMBOX_DIRTY={}", if dirty { "+" } else { "" });
}

/// How many commits lead to this one.
///
/// The commit count rather than a counter bumped on every `cargo build`:
/// a local counter differs on every machine, would make two builds of the
/// same source disagree, and -- since it has to be written somewhere --
/// would dirty the tree or force a rebuild each time. This is a property
/// of the commit, so everyone building it gets the same answer.
///
/// It is what makes two builds comparable. Hashes are not ordered, and the
/// date barely helps: this project has had two dozen commits share a
/// single day.
fn build_number() -> String {
    // A shallow clone can only see part of the history, so its count is a
    // confident wrong answer. No number beats a misleading one.
    if git(&["rev-parse", "--is-shallow-repository"]) == "true" {
        return String::new();
    }
    git(&["rev-list", "--count", "HEAD"])
}

/// Empty when git is missing or this is not a checkout -- a tarball build
/// should still compile, just without a stamp.
fn git(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}
