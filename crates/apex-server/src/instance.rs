//! Instance unique : verrou exclusif sur `data_dir/apex-scada.lock`, pris avant toute
//! communication industrielle. Le verrou est libéré par le système si le processus s'arrête
//! brutalement : aucun fichier « orphelin » ne bloque le redémarrage.
//!
//! Le PID est écrit dans un fichier voisin (`apex-scada.pid`) et non dans le fichier verrouillé :
//! sous Windows, le verrou est obligatoire et empêche toute lecture du fichier par un autre
//! processus.

use std::{
    fs::{File, OpenOptions},
    path::Path,
};

#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

#[derive(Debug)]
pub enum LockError {
    AlreadyRunning { pid: Option<String> },
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::AlreadyRunning { pid } => write!(
                f,
                "une autre instance d'Apex Supervision utilise déjà ce répertoire de données{}",
                pid.as_deref()
                    .map(|p| format!(" (PID {p})"))
                    .unwrap_or_default()
            ),
            LockError::Io(e) => write!(f, "verrou d'instance impossible : {e}"),
        }
    }
}

impl InstanceLock {
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(LockError::Io)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(LockError::Io)?;
        let pid_path = path.with_extension("pid");
        if file.try_lock().is_err() {
            let pid = std::fs::read_to_string(&pid_path)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            return Err(LockError::AlreadyRunning { pid });
        }
        // PID informatif (le verrou, lui, est porté par le système)
        let _ = std::fs::write(&pid_path, std::process::id().to_string());
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_is_refused_then_allowed_after_release() {
        let path = std::env::temp_dir().join(format!("apex-lock-test-{}.lock", std::process::id()));
        let first = InstanceLock::acquire(&path).expect("premier verrou");
        match InstanceLock::acquire(&path) {
            Err(LockError::AlreadyRunning { pid }) => {
                assert_eq!(pid, Some(std::process::id().to_string()))
            }
            other => panic!("refus attendu, obtenu {other:?}"),
        }
        drop(first);
        assert!(
            InstanceLock::acquire(&path).is_ok(),
            "libéré à la fermeture"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("pid"));
    }
}
