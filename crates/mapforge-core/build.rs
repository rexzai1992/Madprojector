use std::{env, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        // `git status` would otherwise refresh the index, and a changed
        // index makes Cargo run this script again on the next build.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Asks Cargo to run this script again when the commit, branches, tags or
/// staged files change. Once a build script names any rerun trigger, Cargo
/// reruns it only for those, so without these a local build would keep an
/// old version and a stale "+local" flag. Edits that are not staged don't
/// touch the index, so the "+local" flag can still lag behind them.
fn watch_git(root: &Path) {
    let resolve = |args: &[&str]| git(root, args).map(|dir| root.join(dir));
    let (Some(git_dir), Some(common_dir)) = (
        resolve(&["rev-parse", "--git-dir"]),
        resolve(&["rev-parse", "--git-common-dir"]),
    ) else {
        return;
    };
    let watched = [
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common_dir.join("refs/heads"),
        common_dir.join("refs/tags"),
        common_dir.join("packed-refs"),
    ];
    // A missing path counts as always changed, so only existing ones are named.
    for path in watched.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=MAPFORGE_VERSION");

    // The release workflow sets MAPFORGE_VERSION. For local builds, use the
    // repository's latest tag so `cargo run` reports the version being worked
    // on instead of Cargo.toml's original 0.1.0 forever.
    let configured = env::var("MAPFORGE_VERSION").ok();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if configured.is_none() {
        watch_git(&root);
    }
    let version = configured.or_else(|| {
        let tag = git(&root, &["describe", "--tags", "--abbrev=0"])?;
        let version = tag.trim_start_matches('v');
        let dirty = git(&root, &["status", "--porcelain"])
            .is_some_and(|status| !status.is_empty());
        Some(if dirty {
            format!("{version}+local")
        } else {
            version.to_owned()
        })
    });

    if let Some(version) = version {
        println!("cargo:rustc-env=MAPFORGE_VERSION={version}");
    }
}
