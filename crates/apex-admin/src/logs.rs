//! Lecture des journaux du moteur, directement sur disque : disponible même moteur arrêté.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Préfixe des journaux quotidiens et nom du fichier des erreurs de démarrage (voir le moteur).
const LOG_PREFIX: &str = "apex-scada";
pub const STARTUP_ERROR_FILE: &str = "apex-scada-startup.log";

#[derive(Debug, Clone, PartialEq)]
pub struct LogFile {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Journaux disponibles, du plus récent au plus ancien.
pub fn list(log_dir: &Path) -> Result<Vec<LogFile>, String> {
    let rd = std::fs::read_dir(log_dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "le dossier des journaux « {} » n'existe pas encore (le moteur n'a jamais démarré ?)",
            log_dir.display()
        ),
        std::io::ErrorKind::PermissionDenied => {
            format!(
                "lecture du dossier des journaux « {} » refusée",
                log_dir.display()
            )
        }
        _ => format!("lecture de « {} » impossible : {e}", log_dir.display()),
    })?;
    let mut files: Vec<LogFile> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with(LOG_PREFIX) && name.ends_with(".log")) {
                return None;
            }
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| LogFile {
                path: e.path(),
                name,
                size: meta.len(),
                modified: meta.modified().ok(),
            })
        })
        .collect();
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.name.cmp(&a.name))
    });
    Ok(files)
}

/// Dernier journal quotidien (hors fichier des erreurs de démarrage).
pub fn latest_daily(files: &[LogFile]) -> Option<&LogFile> {
    files.iter().find(|f| f.name != STARTUP_ERROR_FILE)
}

/// Fin d'un fichier (au plus `max_bytes`), en commençant à une ligne complète.
pub fn tail(path: &Path, max_bytes: u64) -> Result<String, String> {
    let mut f = File::open(path)
        .map_err(|e| format!("ouverture de « {} » impossible : {e}", path.display()))?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or(text)
    } else {
        text
    })
}

/// Indique si une ligne de journal est une erreur ou un avertissement.
pub fn is_problem(line: &str) -> bool {
    line.contains(" ERROR ") || line.contains(" WARN ") || line.contains(" ERREUR ")
}

/// Dernières erreurs et avertissements : journal du jour et erreurs de démarrage, les plus
/// récents en dernier.
pub fn recent_problems(log_dir: &Path, max: usize) -> Vec<String> {
    let Ok(files) = list(log_dir) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = Vec::new();
    if let Some(f) = files.iter().find(|f| f.name == STARTUP_ERROR_FILE) {
        if let Ok(t) = tail(&f.path, 16 * 1024) {
            lines.extend(
                t.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string),
            );
        }
    }
    if let Some(f) = latest_daily(&files) {
        if let Ok(t) = tail(&f.path, 256 * 1024) {
            lines.extend(t.lines().filter(|l| is_problem(l)).map(str::to_string));
        }
    }
    // Tri chronologique : les deux formats commencent par un horodatage triable
    lines.sort();
    let skip = lines.len().saturating_sub(max);
    lines.into_iter().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_tails_and_filters_problems() {
        let dir = std::env::temp_dir().join(format!("apex-admin-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("apex-scada.2026-10-15.log"),
            "2026-10-15T08:00:00Z  INFO a: démarrage\n2026-10-15T08:00:01Z  WARN a: port lent\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(STARTUP_ERROR_FILE),
            "2026-10-14 07:00:00 ERREUR configuration invalide\n",
        )
        .unwrap();
        std::fs::write(dir.join("autre.txt"), "ignoré").unwrap();

        let files = list(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            latest_daily(&files).unwrap().name,
            "apex-scada.2026-10-15.log"
        );

        let problems = recent_problems(&dir, 10);
        assert_eq!(problems.len(), 2);
        assert!(problems[0].contains("ERREUR configuration"));
        assert!(problems[1].contains("port lent"));

        let long: String = (0..200).map(|i| format!("ligne {i}\n")).collect();
        std::fs::write(dir.join("apex-scada.2026-10-16.log"), &long).unwrap();
        let t = tail(&dir.join("apex-scada.2026-10-16.log"), 50).unwrap();
        assert!(
            t.starts_with("ligne"),
            "commence par une ligne complète : {t:?}"
        );
        assert!(t.ends_with("ligne 199\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_explained() {
        let err = list(Path::new("/nonexistent/apex-logs")).unwrap_err();
        assert!(err.contains("n'existe pas encore"), "{err}");
    }
}
