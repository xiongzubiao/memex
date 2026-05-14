// Capture short git commit at build time and expose it as $GIT_COMMIT in the
// compiled binary. Used by `memex --version` to display "0.1.0 (abc1234)".
//
// Falls back to "unknown" when:
//   - Not in a git checkout (e.g. building from a tarball).
//   - `git` is missing on PATH.
//   - The repo has no commits yet.
// Build is never failed; --version just reports "unknown" instead of a hash.

use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT={commit}");
    // Re-run if HEAD or the checked-out ref moves (commit/tag/branch switch).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/refs/tags");
}
