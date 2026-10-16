//! Métadonnées de compilation : commit Git et cible, exposés à `version.rs`.

use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "inconnu".to_string());
    println!("cargo:rustc-env=APEX_GIT_HASH={}", hash);
    println!(
        "cargo:rustc-env=APEX_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "inconnue".to_string())
    );
    // Recompile quand le commit courant change
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
}
