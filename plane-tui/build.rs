//! Embed the git commit and build date into the binary so `pti --version`
//! tells you exactly which build is running — the missing piece that made
//! staleness (installed binary vs. source) hard to spot.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn main() {
    let sha = git(&["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    // Dirty check scoped to this crate so unrelated monorepo WIP doesn't taint it.
    let dirty = git(&["status", "--porcelain", "--untracked-files=no", "--", "."])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let sha = if dirty { format!("{sha}-dirty") } else { sha };

    let date = Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();

    println!("cargo:rustc-env=PTI_GIT_SHA={sha}");
    println!("cargo:rustc-env=PTI_BUILD_DATE={date}");

    // Rebuild when HEAD moves so the embedded SHA stays honest.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/logs/HEAD");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
