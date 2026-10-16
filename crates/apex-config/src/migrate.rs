//! Migrations du format de configuration.
//!
//! Chaque étape transforme un fichier de version `n` en version `n + 1`. Les migrations
//! s'appliquent **en mémoire** à la lecture ; le fichier n'est réécrit (avec sauvegarde
//! préalable) que lors d'un enregistrement explicite (`apex-scada config migrate`, utilitaire).

use crate::{model::CONFIG_VERSION, store::ConfigError};

/// Applique les migrations nécessaires. Renvoie la valeur migrée et la version d'origine si
/// une migration a eu lieu.
pub fn migrate(mut value: toml::Value) -> Result<(toml::Value, Option<u32>), ConfigError> {
    let found = value
        .get("config_version")
        .and_then(|v| v.as_integer())
        .map(|v| v.clamp(0, u32::MAX as i64) as u32)
        .unwrap_or(0);

    if found > CONFIG_VERSION {
        return Err(ConfigError::TooNew {
            found,
            supported: CONFIG_VERSION,
        });
    }

    let mut version = found;
    while version < CONFIG_VERSION {
        value = match version {
            0 => v0_to_v1(value),
            _ => unreachable!("étape de migration manquante pour la version {version}"),
        };
        version += 1;
    }

    Ok((value, (found != CONFIG_VERSION).then_some(found)))
}

/// v0 → v1 : fichiers rédigés à la main sans `config_version`. Le contenu est conservé tel
/// quel ; seul le numéro de version est ajouté.
fn v0_to_v1(mut value: toml::Value) -> toml::Value {
    if let Some(table) = value.as_table_mut() {
        table.insert("config_version".into(), toml::Value::Integer(1));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_untouched() {
        let v: toml::Value = toml::from_str("config_version = 1\n[server]\nport = 3000\n").unwrap();
        let (out, from) = migrate(v.clone()).unwrap();
        assert_eq!(out, v);
        assert!(from.is_none());
    }

    #[test]
    fn unversioned_file_is_migrated_to_v1() {
        let v: toml::Value = toml::from_str("[server]\nport = 4000\n").unwrap();
        let (out, from) = migrate(v).unwrap();
        assert_eq!(from, Some(0));
        assert_eq!(out["config_version"].as_integer(), Some(1));
        assert_eq!(out["server"]["port"].as_integer(), Some(4000));
    }

    #[test]
    fn newer_format_is_rejected() {
        let v: toml::Value = toml::from_str("config_version = 99\n").unwrap();
        assert!(matches!(
            migrate(v),
            Err(ConfigError::TooNew { found: 99, .. })
        ));
    }
}
