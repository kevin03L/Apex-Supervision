//! # Module `security` — Primitives de sécurité de Apex Supervision
//!
//! - Hachage des mots de passe **Argon2id** (format PHC `$argon2id$...`).
//! - Génération de jetons de session aléatoires (256 bits, CSPRNG du système).
//! - Validation des identifiants métier (tags, drivers, synoptiques, flux...).
//! - Assainissement des symboles SVG importés (suppression des vecteurs XSS).

use std::sync::OnceLock;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::RngCore;
use regex::Regex;

use crate::models::ScadaError;

/// Longueur minimale imposée aux nouveaux mots de passe.
pub const MIN_PASSWORD_LEN: usize = 6;

// ============================================================================
// MOTS DE PASSE
// ============================================================================

pub fn hash_password(plain: &str) -> Result<String, ScadaError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ScadaError::BadRequest(format!("Hachage du mot de passe impossible : {}", e)))
}

/// Vérifie un mot de passe contre un hash PHC. Un hash invalide (ou un ancien mot de passe
/// en clair non migré) est toujours refusé.
pub fn verify_password(plain: &str, stored_hash: &str) -> bool {
    match PasswordHash::new(stored_hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Indique si la valeur stockée est déjà un hash PHC (et non un mot de passe en clair hérité).
pub fn is_password_hash(stored: &str) -> bool {
    stored.starts_with("$argon2")
}

pub fn validate_new_password(plain: &str) -> Result<(), ScadaError> {
    if plain.chars().count() < MIN_PASSWORD_LEN {
        return Err(ScadaError::BadRequest(format!(
            "Le mot de passe doit contenir au moins {} caractères",
            MIN_PASSWORD_LEN
        )));
    }
    Ok(())
}

// ============================================================================
// JETONS DE SESSION
// ============================================================================

/// Jeton aléatoire de 256 bits encodé en hexadécimal (64 caractères).
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ============================================================================
// VALIDATION DES IDENTIFIANTS
// ============================================================================

/// Un identifiant métier ne contient que `[A-Za-z0-9_.:-]` (1 à 128 caractères).
/// Cela garantit qu'il peut être injecté sans risque dans une URL, un attribut HTML ou un sélecteur.
pub fn validate_id(kind: &str, id: &str) -> Result<(), ScadaError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'));
    if ok {
        Ok(())
    } else {
        Err(ScadaError::BadRequest(format!(
            "Identifiant de {} invalide '{}' : 1 à 128 caractères parmi A-Z a-z 0-9 _ . : -",
            kind, id
        )))
    }
}

/// Transforme un texte libre en identifiant valide (utilisé pour les symboles importés).
pub fn slugify_id(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(96)
        .collect();
    if s.is_empty() {
        "sym".to_string()
    } else {
        s
    }
}

// ============================================================================
// ASSAINISSEMENT SVG
// ============================================================================

struct SvgFilters {
    paired_tags: Regex,
    single_tags: Regex,
    href_animation: Regex,
    event_attrs: Regex,
    script_values: Regex,
    url_attrs: Regex,
}

fn svg_filters() -> &'static SvgFilters {
    static FILTERS: OnceLock<SvgFilters> = OnceLock::new();
    FILTERS.get_or_init(|| SvgFilters {
        // Éléments actifs avec leur contenu
        paired_tags: Regex::new(
            r"(?is)<\s*(script|style|foreignobject|iframe|object|embed|handler|listener)\b.*?<\s*/\s*(script|style|foreignobject|iframe|object|embed|handler|listener)\s*>",
        )
        .expect("regex paired_tags"),
        // Balises isolées ou non fermées restantes
        single_tags: Regex::new(
            r"(?is)<\s*/?\s*(script|style|foreignobject|iframe|object|embed|handler|listener|link|meta|base)\b[^>]*>",
        )
        .expect("regex single_tags"),
        // <animate>/<set> qui réécrivent un attribut href (vecteur XSS SVG connu)
        href_animation: Regex::new(
            r#"(?is)<\s*(animate|set)\b[^>]*attributename\s*=\s*["']?\s*(xlink:)?href[^>]*>"#,
        )
        .expect("regex href_animation"),
        // Gestionnaires d'événements on*="..."
        event_attrs: Regex::new(r#"(?is)\s+on[a-z]+\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)"#)
            .expect("regex event_attrs"),
        // Toute valeur d'attribut contenant un schéma exécutable
        script_values: Regex::new(
            r#"(?is)\s+[a-z_:][a-z0-9_:.-]*\s*=\s*("[^"]*(javascript|vbscript)\s*:[^"]*"|'[^']*(javascript|vbscript)\s*:[^']*')"#,
        )
        .expect("regex script_values"),
        // Attributs d'URL : seules les ancres locales et les images data: sont autorisées
        url_attrs: Regex::new(
            r#"(?is)\s(href|xlink:href|src|action|formaction)\s*=\s*("([^"]*)"|'([^']*)'|([^\s>]+))"#,
        )
        .expect("regex url_attrs"),
    })
}

/// Supprime d'un SVG tous les éléments et attributs capables d'exécuter du code dans le navigateur.
pub fn sanitize_svg(input: &str) -> String {
    let f = svg_filters();
    let mut out = input.to_string();
    // Plusieurs passes : neutralise les imbrications du type <scr<script>ipt>
    for _ in 0..3 {
        let before = out.clone();
        out = f.paired_tags.replace_all(&out, "").into_owned();
        out = f.single_tags.replace_all(&out, "").into_owned();
        out = f.href_animation.replace_all(&out, "").into_owned();
        out = f.event_attrs.replace_all(&out, "").into_owned();
        out = f.script_values.replace_all(&out, "").into_owned();
        if out == before {
            break;
        }
    }
    f.url_attrs
        .replace_all(&out, |caps: &regex::Captures| {
            let attr = &caps[1];
            let value = caps
                .get(3)
                .or_else(|| caps.get(4))
                .or_else(|| caps.get(5))
                .map(|m| m.as_str().trim())
                .unwrap_or("");
            let lower = value.to_ascii_lowercase();
            if value.starts_with('#')
                || lower.starts_with("data:image/png")
                || lower.starts_with("data:image/jpeg")
                || lower.starts_with("data:image/gif")
            {
                format!(" {}=\"{}\"", attr, value.replace('"', "&quot;"))
            } else {
                format!(" {}=\"#\"", attr)
            }
        })
        .into_owned()
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let h = hash_password("admin123").unwrap();
        assert!(is_password_hash(&h));
        assert!(verify_password("admin123", &h));
        assert!(!verify_password("admin124", &h));
    }

    #[test]
    fn plaintext_legacy_is_never_accepted() {
        assert!(!is_password_hash("admin123"));
        assert!(!verify_password("admin123", "admin123"));
    }

    #[test]
    fn new_password_min_length() {
        assert!(validate_new_password("12345").is_err());
        assert!(validate_new_password("123456").is_ok());
    }

    #[test]
    fn tokens_are_random_and_long() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn id_validation() {
        assert!(validate_id("tag", "GTB.CTA01.Temp_Soufflage").is_ok());
        assert!(validate_id("tag", "MODBUS_GF01").is_ok());
        assert!(validate_id("tag", "sched-cta01:confort").is_ok());
        assert!(validate_id("tag", "").is_err());
        assert!(validate_id("tag", "x'); alert(1); //").is_err());
        assert!(validate_id("tag", "a b").is_err());
        assert!(validate_id("tag", &"a".repeat(129)).is_err());
    }

    #[test]
    fn slugify_produces_valid_ids() {
        let s = slugify_id("Pompe <b>\"P1\"</b> é");
        assert!(validate_id("symbole", &s).is_ok());
        assert_eq!(slugify_id(""), "sym");
    }

    #[test]
    fn svg_sanitizer_removes_scripts_and_handlers() {
        let dirty = r#"<svg viewBox="0 0 10 10" onload="alert(1)"><script>alert(2)</script><circle cx="5" cy="5" r="4" onclick='steal()' fill="red"/><foreignObject><div>x</div></foreignObject><style>body{display:none}</style></svg>"#;
        let clean = sanitize_svg(dirty);
        let lower = clean.to_lowercase();
        assert!(!lower.contains("script"));
        assert!(
            !lower.contains("<style"),
            "un <style> SVG s'appliquerait à toute la page"
        );
        assert!(!lower.contains("onload"));
        assert!(!lower.contains("onclick"));
        assert!(!lower.contains("foreignobject"));
        assert!(clean.contains("<circle"));
        assert!(clean.contains("viewBox=\"0 0 10 10\""));
    }

    #[test]
    fn svg_sanitizer_neutralizes_urls() {
        let dirty = r##"<svg><a href="javascript:alert(1)"><text>x</text></a><a xlink:href="  JaVaScRiPt:alert(1)">y</a><use href="#local"/><animate attributeName="href" values="javascript:alert(1)"/></svg>"##;
        let clean = sanitize_svg(dirty);
        assert!(!clean.to_lowercase().contains("javascript"));
        assert!(clean.contains(r##"href="#local""##));
        assert!(!clean.contains("<animate"));
    }

    #[test]
    fn svg_sanitizer_keeps_legit_symbols() {
        let legit = r##"<svg viewBox="0 0 100 100" width="84" height="84"><g class="sym-rotatable" style="transform-origin:50px 50px;"><circle cx="50" cy="50" r="36" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/></g></svg>"##;
        assert_eq!(sanitize_svg(legit), legit);
    }

    #[test]
    fn svg_sanitizer_handles_nested_tricks() {
        let dirty = "<svg><scr<script>x</script>ipt>alert(1)</script></svg>";
        let clean = sanitize_svg(dirty).to_lowercase();
        assert!(!clean.contains("<script"));
    }
}
