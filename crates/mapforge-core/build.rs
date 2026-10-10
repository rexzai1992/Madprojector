use std::{env, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=MAPFORGE_VERSION");

    // The release workflow sets MAPFORGE_VERSION. For local builds, use the
    // repository's latest tag so `cargo run` reports the version being worked
    // on instead of Cargo.toml's original 0.1.0 forever.
    let configured = env::var("MAPFORGE_VERSION").ok();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
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
