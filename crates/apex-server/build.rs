//! Ressource Windows de `apex-scada.exe` : icône et VERSIONINFO (sans effet sur Linux).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    apex_build::ressources_windows(apex_build::Executable {
        nom: "apex-scada",
        description: "Apex Supervision — moteur de supervision GTC/SCADA",
    });
}
