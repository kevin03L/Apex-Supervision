//! Métadonnées de compilation : commit Git et cible, exposés à `version.rs`.

use std::{path::PathBuf, process::Command};

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    let hash = git(&["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "inconnu".to_string());
    println!("cargo:rustc-env=APEX_GIT_HASH={}", hash);
    println!(
        "cargo:rustc-env=APEX_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "inconnue".to_string())
    );
    // Recompile quand le commit courant change. Le dossier Git est demandé à git (dans un
    // worktree, `.git` est un fichier) ; un chemin inexistant relancerait ce script à chaque
    // compilation, il n'est donc déclaré que s'il existe.
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(git_dir) = git(&["rev-parse", "--git-dir"]).map(PathBuf::from) {
        for watched in [git_dir.join("HEAD"), git_dir.join("refs").join("heads")] {
            if watched.exists() {
                println!("cargo:rerun-if-changed={}", watched.display());
            }
        }
    }
}
