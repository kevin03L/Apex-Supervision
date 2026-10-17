//! # Module `security` — Primitives de sécurité de Apex Supervision
//!
//! - Hachage des mots de passe **Argon2id** (format PHC `$argon2id$...`).
//! - Génération de jetons de session aléatoires (256 bits, CSPRNG du système).
//! - Validation des identifiants métier (tags, drivers, synoptiques, flux...).
//! - Assainissement des symboles SVG importés : analyse XML réelle puis reconstruction à partir
//!   d'une liste blanche d'éléments et d'attributs de dessin.

use std::{collections::HashMap, sync::OnceLock};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use quick_xml::{
    escape::resolve_predefined_entity,
    events::{BytesStart, Event},
    Reader, XmlVersion,
};
use rand::RngCore;

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

/// Hachage factice, calculé une seule fois avec les mêmes paramètres Argon2 que les vrais
/// comptes, à partir d'un secret aléatoire jamais conservé : aucun mot de passe ne le vérifie.
fn dummy_password_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(generate_token().as_bytes(), &salt)
            .map(|h| h.to_string())
            .unwrap_or_default()
    })
}

/// Précalcule le hachage factice (au démarrage du moteur) pour que la première connexion sur un
/// identifiant inconnu ne coûte pas plus cher que les suivantes.
pub fn prepare_dummy_password_hash() {
    let _ = dummy_password_hash();
}

/// Vérifie un mot de passe pour un compte qui peut ne pas exister, à **coût constant** : sans
/// hash exploitable (identifiant inconnu, hash illisible), la vérification Argon2 est faite
/// contre le hachage factice et échoue toujours. Un identifiant inconnu coûte ainsi autant qu'un
/// mot de passe faux : le temps de réponse ne permet pas d'énumérer les comptes.
pub fn verify_password_constant_cost(plain: &str, stored_hash: Option<&str>) -> bool {
    if let Some(parsed) = stored_hash.and_then(|h| PasswordHash::new(h).ok()) {
        return Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok();
    }
    if let Ok(parsed) = PasswordHash::new(dummy_password_hash()) {
        let _ = Argon2::default().verify_password(plain.as_bytes(), &parsed);
    }
    false
}

/// Indique si la valeur stockée est déjà un hash PHC (et non un mot de passe en clair hérité).
pub fn is_password_hash(stored: &str) -> bool {
    stored.starts_with("$argon2")
}

pub fn validate_new_password(plain: &str) -> Result<(), ScadaError> {
    if plain.len() > 256 {
        return Err(ScadaError::BadRequest(
            "Le mot de passe ne doit pas dépasser 256 octets".into(),
        ));
    }
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

/// Clé d'une session : empreinte SHA-256 (hexadécimal) du jeton.
///
/// Les sessions sont indexées et conservées en base sous cette empreinte : une copie de la base
/// ne permet pas de reprendre une session, et le jeton n'est jamais écrit sur disque. Un jeton
/// de 256 bits aléatoires n'a pas besoin d'un hachage lent.
pub fn session_key(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
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
//
// Le SVG est analysé comme un document XML (quick-xml) puis **reconstruit** : seuls les éléments
// et les attributs de dessin de la liste blanche sont recopiés, avec des valeurs ré-échappées.
// Tout le reste disparaît : script, style, foreignObject, liens, gestionnaires `on*`, liens
// externes, commentaires, instructions de traitement, sections CDATA, DOCTYPE et entités non
// prédéfinies. Un document mal formé est refusé : on ne tente jamais de le « réparer », car
// c'est précisément l'écart entre deux interprétations d'un balisage ambigu qui permet de
// contourner un filtre. Les listes sont reprises à l'identique par `sanitizeSvg` (static/app.js).

/// Espace de noms SVG, ajouté sur l'élément racine reconstruit.
const SVG_NAMESPACE: &str = "http://www.w3.org/2000/svg";
/// Profondeur d'imbrication maximale acceptée.
const SVG_MAX_DEPTH: usize = 64;

/// Éléments de dessin autorisés (noms sensibles à la casse, comme en XML).
const SVG_ELEMENTS: &[&str] = &[
    "svg",
    "g",
    "defs",
    "symbol",
    "use",
    "title",
    "desc",
    "path",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "text",
    "tspan",
    "textPath",
    "linearGradient",
    "radialGradient",
    "stop",
    "pattern",
    "clipPath",
    "mask",
    "marker",
    "image",
    "filter",
    "feBlend",
    "feColorMatrix",
    "feComponentTransfer",
    "feFuncR",
    "feFuncG",
    "feFuncB",
    "feFuncA",
    "feComposite",
    "feDropShadow",
    "feFlood",
    "feGaussianBlur",
    "feMerge",
    "feMergeNode",
    "feMorphology",
    "feOffset",
    "feTurbulence",
    "feDiffuseLighting",
    "feSpecularLighting",
    "feDistantLight",
    "fePointLight",
    "feSpotLight",
    "feDisplacementMap",
    "feTile",
    "feConvolveMatrix",
    // Structure des exports d'éditeurs (draw.io, Inkscape) ; <a> n'accepte qu'un lien interne
    "switch",
    "a",
    "animate",
    "animateMotion",
    "animateTransform",
    "set",
];

/// Animations SMIL : leur attribut cible (`attributeName`) est contrôlé en plus.
const SVG_ANIMATIONS: &[&str] = &["animate", "animateMotion", "animateTransform", "set"];

/// Éléments sur lesquels un `href` (forcément interne, `#id`) a un sens.
const SVG_HREF_ELEMENTS: &[&str] = &[
    "a",
    "use",
    "linearGradient",
    "radialGradient",
    "pattern",
    "textPath",
    "image",
    "filter",
    "animate",
    "animateMotion",
    "animateTransform",
    "set",
];

/// Attributs de dessin autorisés (hors `href`, traité à part).
const SVG_ATTRIBUTES: &[&str] = &[
    // Structure
    "id",
    "class",
    "style",
    "transform",
    "viewBox",
    "preserveAspectRatio",
    "version",
    // Géométrie
    "x",
    "y",
    "width",
    "height",
    "x1",
    "y1",
    "x2",
    "y2",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "fx",
    "fy",
    "fr",
    "d",
    "points",
    "pathLength",
    "dx",
    "dy",
    "rotate",
    "textLength",
    "lengthAdjust",
    "startOffset",
    "method",
    "spacing",
    // Présentation
    "fill",
    "fill-opacity",
    "fill-rule",
    "stroke",
    "stroke-width",
    "stroke-opacity",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-dasharray",
    "stroke-dashoffset",
    "opacity",
    "color",
    "display",
    "visibility",
    "overflow",
    "clip-path",
    "clip-rule",
    "mask",
    "filter",
    "marker-start",
    "marker-mid",
    "marker-end",
    "stop-color",
    "stop-opacity",
    "flood-color",
    "flood-opacity",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "font-variant",
    "font-stretch",
    "text-anchor",
    "dominant-baseline",
    "alignment-baseline",
    "baseline-shift",
    "letter-spacing",
    "word-spacing",
    "text-decoration",
    "writing-mode",
    "direction",
    "vector-effect",
    "shape-rendering",
    "text-rendering",
    "image-rendering",
    "color-interpolation",
    "color-interpolation-filters",
    "paint-order",
    "mix-blend-mode",
    "isolation",
    "transform-origin",
    // Dégradés, motifs, découpes, masques, marqueurs
    "offset",
    "gradientUnits",
    "gradientTransform",
    "spreadMethod",
    "patternUnits",
    "patternContentUnits",
    "patternTransform",
    "clipPathUnits",
    "maskUnits",
    "maskContentUnits",
    "markerWidth",
    "markerHeight",
    "markerUnits",
    "refX",
    "refY",
    "orient",
    // Filtres
    "filterUnits",
    "primitiveUnits",
    "in",
    "in2",
    "result",
    "mode",
    "operator",
    "k1",
    "k2",
    "k3",
    "k4",
    "values",
    "type",
    "stdDeviation",
    "edgeMode",
    "radius",
    "tableValues",
    "slope",
    "intercept",
    "amplitude",
    "exponent",
    "baseFrequency",
    "numOctaves",
    "seed",
    "stitchTiles",
    "surfaceScale",
    "specularConstant",
    "specularExponent",
    "diffuseConstant",
    "kernelUnitLength",
    "lighting-color",
    "azimuth",
    "elevation",
    "z",
    "pointsAtX",
    "pointsAtY",
    "pointsAtZ",
    "limitingConeAngle",
    "scale",
    "xChannelSelector",
    "yChannelSelector",
    "order",
    "kernelMatrix",
    "divisor",
    "bias",
    "targetX",
    "targetY",
    "preserveAlpha",
    // Animations SMIL
    "attributeName",
    "attributeType",
    "from",
    "to",
    "by",
    "begin",
    "dur",
    "end",
    "min",
    "max",
    "restart",
    "repeatCount",
    "repeatDur",
    "calcMode",
    "keyTimes",
    "keySplines",
    "keyPoints",
    "additive",
    "accumulate",
    "path",
];

/// Attributs qu'une animation SMIL ne peut pas cibler (`href` n'est de toute façon pas dans la
/// liste blanche des attributs et ne peut donc jamais être animé).
const SVG_ANIMATION_FORBIDDEN_TARGETS: &[&str] = &["id", "style", "attributeName"];

/// Fragments interdits dans toute valeur d'attribut (recherchés sans blancs ni casse).
const SVG_FORBIDDEN_VALUE_FRAGMENTS: &[&str] = &[
    "javascript:",
    "vbscript:",
    "livescript:",
    "expression(",
    "-moz-binding",
    "behavior:",
    "@import",
    // Échappements CSS (`u\72l(`) : ils permettraient de masquer les motifs ci-dessus
    "\\",
];

/// Images matricielles autorisées en `data:` (et uniquement dans `<image href>`).
const SVG_RASTER_DATA_PREFIXES: &[&str] = &[
    "data:image/png;base64,",
    "data:image/jpeg;base64,",
    "data:image/gif;base64,",
    "data:image/webp;base64,",
];

/// Attributs reconstruits d'un élément conservé.
type SvgAttributes = Vec<(&'static str, String)>;

/// Erreur d'assainissement présentée à l'utilisateur.
fn svg_refused(detail: impl std::fmt::Display) -> ScadaError {
    ScadaError::BadRequest(format!("SVG refusé : {}", detail))
}

/// Assainit un SVG : analyse XML réelle puis reconstruction à partir de la liste blanche.
///
/// Renvoie le SVG reconstruit (racine `<svg xmlns="http://www.w3.org/2000/svg">`), ou une
/// erreur si le document n'est pas du XML bien formé ou si sa racine n'est pas `<svg>`.
pub fn sanitize_svg(input: &str) -> Result<String, ScadaError> {
    // Les feuilles <style> sont supprimées, mais leurs règles de classe simples (.st0 {…},
    // exports Illustrator) sont reportées dans l'attribut style des éléments concernés.
    let class_styles = collect_class_styles(input);
    let mut reader = Reader::from_str(input);
    let mut out = String::with_capacity(input.len());
    // Éléments ouverts et conservés (pour fermer avec le même nom)
    let mut open: Vec<&'static str> = Vec::new();
    // Profondeur dans un sous-arbre supprimé (0 : aucun sous-arbre supprimé en cours)
    let mut skipped_depth = 0usize;
    let mut root_seen = false;
    let mut root_closed = false;

    loop {
        let event = reader.read_event().map_err(|e| {
            svg_refused(format!(
                "XML mal formé ({} à l'octet {})",
                e,
                reader.error_position()
            ))
        })?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let self_closing = matches!(event, Event::Empty(_));
                if skipped_depth > 0 {
                    if !self_closing {
                        skipped_depth += 1;
                    }
                    continue;
                }
                let is_root = !root_seen;
                let Some((name, attributes)) = rebuild_element(e, is_root, &class_styles)? else {
                    if is_root {
                        return Err(svg_refused("l'élément racine doit être <svg>"));
                    }
                    if !self_closing {
                        skipped_depth = 1;
                    }
                    continue;
                };
                if open.len() >= SVG_MAX_DEPTH {
                    return Err(svg_refused("imbrication des éléments trop profonde"));
                }
                root_seen = true;
                out.push('<');
                out.push_str(name);
                if is_root {
                    out.push_str(" xmlns=\"");
                    out.push_str(SVG_NAMESPACE);
                    out.push('"');
                }
                for (key, value) in &attributes {
                    out.push(' ');
                    out.push_str(key);
                    out.push_str("=\"");
                    push_escaped(&mut out, value, true);
                    out.push('"');
                }
                if self_closing {
                    out.push_str("/>");
                    if is_root {
                        root_closed = true;
                        break;
                    }
                } else {
                    out.push('>');
                    open.push(name);
                }
            }
            Event::End(_) => {
                if skipped_depth > 0 {
                    skipped_depth -= 1;
                    continue;
                }
                if let Some(name) = open.pop() {
                    out.push_str("</");
                    out.push_str(name);
                    out.push('>');
                    if open.is_empty() {
                        root_closed = true;
                        break;
                    }
                }
            }
            Event::Text(text) => {
                if skipped_depth == 0 && !open.is_empty() {
                    let content = text
                        .xml10_content()
                        .map_err(|e| svg_refused(format!("texte illisible ({})", e)))?;
                    push_escaped(&mut out, &content, false);
                }
            }
            Event::GeneralRef(reference) => {
                // Seules les références de caractère et les 5 entités prédéfinies sont
                // reprises ; une entité inconnue (déclarée dans un DOCTYPE) est supprimée.
                if skipped_depth == 0 && !open.is_empty() {
                    let resolved = match reference.resolve_char_ref() {
                        Ok(Some(c)) => Some(c.to_string()),
                        Ok(None) => reference
                            .decode()
                            .ok()
                            .and_then(|name| resolve_predefined_entity(&name))
                            .map(str::to_string),
                        Err(_) => None,
                    };
                    if let Some(s) = resolved {
                        push_escaped(&mut out, &s, false);
                    }
                }
            }
            // Ni élément ni texte : supprimés sans exception
            Event::Comment(_)
            | Event::CData(_)
            | Event::PI(_)
            | Event::Decl(_)
            | Event::DocType(_) => {}
            Event::Eof => break,
        }
    }

    if !root_seen {
        return Err(svg_refused("aucun élément <svg> trouvé"));
    }
    if !root_closed {
        return Err(svg_refused("document incomplet (élément non fermé)"));
    }
    Ok(out)
}

/// Reconstruit un élément de la liste blanche avec ses seuls attributs autorisés.
/// `None` : l'élément (et donc tout son sous-arbre) est supprimé.
fn rebuild_element(
    e: &BytesStart<'_>,
    is_root: bool,
    class_styles: &HashMap<String, String>,
) -> Result<Option<(&'static str, SvgAttributes)>, ScadaError> {
    let qname = e.name();
    let raw_name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
    let Some(name) = SVG_ELEMENTS.iter().copied().find(|n| *n == raw_name) else {
        return Ok(None);
    };
    if is_root && name != "svg" {
        return Ok(None);
    }
    let animation = SVG_ANIMATIONS.contains(&name);
    let mut attributes: SvgAttributes = Vec::new();

    for attr in e.attributes() {
        let attr = attr
            .map_err(|err| svg_refused(format!("attribut mal formé dans <{}> ({})", name, err)))?;
        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
        let Some(canonical) = canonical_attribute(name, key) else {
            continue;
        };
        // Pas de style sur la racine : il pourrait sortir le symbole de son cadre (position fixe)
        if is_root && canonical == "style" {
            continue;
        }
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .ok()
            .and_then(|v| sanitize_attribute_value(name, canonical, &v));
        match value {
            Some(v) => {
                // `href` et `xlink:href` ne donnent qu'un seul attribut
                if !attributes.iter().any(|(k, _)| *k == canonical) {
                    attributes.push((canonical, v));
                }
            }
            // Une animation dont la cible est refusée est supprimée entièrement
            None if animation && canonical == "attributeName" => return Ok(None),
            None => {}
        }
    }
    if !is_root && !class_styles.is_empty() {
        apply_class_styles(name, &mut attributes, class_styles);
    }
    Ok(Some((name, attributes)))
}

/// Règles `.classe { déclarations }` des éléments `<style>` du document. Les autres
/// sélecteurs et les règles `@…` sont ignorés. Un document illisible donne une table vide
/// (l'analyse principale signale alors l'erreur).
fn collect_class_styles(input: &str) -> HashMap<String, String> {
    let mut css = String::new();
    let mut reader = Reader::from_str(input);
    let mut in_style = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == b"style" => in_style += 1,
            Ok(Event::End(e)) if e.name().as_ref() == b"style" => {
                in_style = in_style.saturating_sub(1)
            }
            Ok(Event::Text(t)) if in_style > 0 => {
                if let Ok(content) = t.xml10_content() {
                    css.push_str(&content);
                }
            }
            Ok(Event::CData(t)) if in_style > 0 => {
                css.push_str(&String::from_utf8_lossy(&t));
            }
            Ok(Event::GeneralRef(r)) if in_style > 0 => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    css.push(c);
                } else if let Some(s) = r.decode().ok().and_then(|n| resolve_predefined_entity(&n))
                {
                    css.push_str(s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    parse_class_rules(&css)
}

fn parse_class_rules(css: &str) -> HashMap<String, String> {
    // Commentaires CSS retirés
    let mut text = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        text.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        rest = after.find("*/").map_or("", |end| &after[end + 2..]);
    }
    text.push_str(rest);

    let mut rules: HashMap<String, String> = HashMap::new();
    for block in text.split('}') {
        let Some((selectors, declarations)) = block.split_once('{') else {
            continue;
        };
        let declarations = declarations.trim().trim_end_matches(';').trim();
        if declarations.is_empty() || selectors.contains('@') || declarations.contains('{') {
            continue;
        }
        for selector in selectors.split(',') {
            let Some(class) = selector.trim().strip_prefix('.') else {
                continue;
            };
            if class.is_empty()
                || !class
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            {
                continue;
            }
            let entry = rules.entry(class.to_string()).or_default();
            if !entry.is_empty() {
                entry.push(';');
            }
            entry.push_str(declarations);
        }
    }
    rules
}

/// Reporte les règles de classe dans l'attribut `style` (le style propre à l'élément reste
/// prioritaire : il est placé après). Les déclarations passent par le même contrôle que tout
/// attribut `style`.
fn apply_class_styles(
    element: &str,
    attributes: &mut SvgAttributes,
    class_styles: &HashMap<String, String>,
) {
    let Some(classes) = attributes
        .iter()
        .find(|(k, _)| *k == "class")
        .map(|(_, v)| v.clone())
    else {
        return;
    };
    let inherited: Vec<&str> = classes
        .split_whitespace()
        .filter_map(|c| class_styles.get(c).map(String::as_str))
        .collect();
    if inherited.is_empty() {
        return;
    }
    let Some(inherited) = sanitize_attribute_value(element, "style", &inherited.join(";")) else {
        return;
    };
    match attributes.iter_mut().find(|(k, _)| *k == "style") {
        Some((_, own)) => *own = format!("{};{}", inherited, own),
        None => attributes.push(("style", inherited)),
    }
}

/// Nom d'attribut reconstruit, ou `None` s'il n'est pas dans la liste blanche.
/// `xlink:href` devient `href` (SVG 2) : aucun espace de noms secondaire n'est nécessaire.
fn canonical_attribute(element: &str, key: &str) -> Option<&'static str> {
    if key == "href" || key == "xlink:href" {
        return SVG_HREF_ELEMENTS.contains(&element).then_some("href");
    }
    SVG_ATTRIBUTES.iter().copied().find(|a| *a == key)
}

/// Contrôle la valeur d'un attribut autorisé ; `None` supprime l'attribut.
fn sanitize_attribute_value(element: &str, attr: &str, value: &str) -> Option<String> {
    // Forme compacte (sans blancs ni caractères de contrôle, en minuscules) pour la détection :
    // « java script: », « JaVaScRiPt: » ou « url ( » ne passent pas.
    let compact: String = value
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .flat_map(char::to_lowercase)
        .collect();
    if SVG_FORBIDDEN_VALUE_FRAGMENTS
        .iter()
        .any(|f| compact.contains(f))
    {
        return None;
    }
    if !css_urls_are_internal(&compact) {
        return None;
    }
    let raster_image = attr == "href" && element == "image";
    // id exclu : ses caractères sont déjà restreints (metadata:x est un identifiant valide)
    if compact.contains("data:") && !raster_image && attr != "id" {
        return None;
    }
    let value = value.trim();
    let ok = match attr {
        "href" => is_internal_reference(value) || (raster_image && is_raster_data_uri(value)),
        "style" => !compact.contains("/*") && !compact.contains('<'),
        "id" => is_safe_id(value),
        "attributeName" => {
            SVG_ATTRIBUTES.contains(&value) && !SVG_ANIMATION_FORBIDDEN_TARGETS.contains(&value)
        }
        _ => true,
    };
    ok.then(|| value.to_string())
}

/// Toute fonction `url(...)` doit désigner un élément interne (`url(#id)`).
fn css_urls_are_internal(compact: &str) -> bool {
    let mut rest = compact;
    while let Some(pos) = rest.find("url(") {
        rest = &rest[pos + 4..];
        if !rest.trim_start_matches(['"', '\'']).starts_with('#') {
            return false;
        }
    }
    true
}

/// Référence interne au document (`#id`).
fn is_internal_reference(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(is_safe_id)
}

/// Identifiant d'élément sûr (mêmes caractères que les identifiants métier).
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// Image matricielle embarquée en base64 (PNG, JPEG, GIF, WebP).
fn is_raster_data_uri(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    SVG_RASTER_DATA_PREFIXES.iter().any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|data| {
            data.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=') || c.is_ascii_whitespace()
            })
        })
    })
}

/// Recopie un texte ou une valeur d'attribut en l'échappant pour XML. Les caractères de
/// contrôle interdits en XML 1.0 sont supprimés.
fn push_escaped(out: &mut String, s: &str, attribute: bool) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            '\t' | '\n' | '\r' if attribute => out.push_str(&format!("&#{};", c as u32)),
            '\t' | '\n' | '\r' => out.push(c),
            c if c.is_control() || matches!(c, '\u{FFFE}' | '\u{FFFF}') => {}
            c => out.push(c),
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

/// Le schéma HTTP est insensible à la casse, le jeton reste strictement inchangé.
pub fn bearer_token(raw: &str) -> &str {
    let raw = raw.trim();
    match raw.split_once(' ') {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("Bearer") => token.trim(),
        _ => raw,
    }
}

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
    fn constant_cost_verification() {
        let h = hash_password("super123").unwrap();
        assert!(verify_password_constant_cost("super123", Some(&h)));
        assert!(!verify_password_constant_cost("faux", Some(&h)));
        // Compte inconnu ou hash illisible : toujours refusé (vérification contre le factice)
        assert!(!verify_password_constant_cost("super123", None));
        assert!(!verify_password_constant_cost("", None));
        assert!(!verify_password_constant_cost("admin123", Some("admin123")));
        // Le hachage factice a les mêmes paramètres que les vrais (donc le même coût)
        let dummy = PasswordHash::new(dummy_password_hash()).expect("hachage factice valide");
        let real = PasswordHash::new(&h).unwrap();
        assert_eq!(dummy.algorithm, real.algorithm);
        assert_eq!(
            dummy.params.iter().collect::<Vec<_>>(),
            real.params.iter().collect::<Vec<_>>()
        );
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

    // ------------------------------------------------------------------------
    // SVG
    // ------------------------------------------------------------------------

    /// Éléments (nom, nombre d'attributs) d'un document, dans l'ordre : sert à vérifier qu'un
    /// SVG légitime est reconstruit sans perte.
    fn structure(svg: &str) -> Vec<(String, usize)> {
        let mut reader = Reader::from_str(svg);
        let mut list = Vec::new();
        loop {
            match reader.read_event().expect("XML valide") {
                Event::Start(e) | Event::Empty(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                    let attrs = e
                        .attributes()
                        .filter(|a| a.as_ref().is_ok_and(|a| a.key.as_ref() != b"xmlns"))
                        .count();
                    list.push((name, attrs));
                }
                Event::Eof => break,
                _ => {}
            }
        }
        list
    }

    /// Le résultat ne contient plus aucun vecteur d'exécution connu.
    fn assert_inert(clean: &str) {
        let lower = clean.replace(SVG_NAMESPACE, "").to_lowercase();
        for needle in [
            "<script",
            "<style",
            "foreignobject",
            "javascript",
            "vbscript",
            "alert(",
            "onload",
            "onerror",
            "onclick",
            "onbegin",
            "<?",
            "<!",
            "<iframe",
            "<a ",
            "http:",
            "https:",
        ] {
            assert!(
                !lower.contains(needle),
                "« {needle} » subsiste dans {clean}"
            );
        }
    }

    /// Soit le SVG est refusé, soit il est rendu inerte.
    fn assert_refused_or_inert(dirty: &str) {
        if let Ok(clean) = sanitize_svg(dirty) {
            assert_inert(&clean);
        }
    }

    #[test]
    fn svg_sanitizer_removes_scripts_and_handlers() {
        let dirty = r#"<svg viewBox="0 0 10 10" onload="alert(1)"><script>alert(2)</script><circle cx="5" cy="5" r="4" onclick='steal()' fill="red"/><foreignObject><div>x</div></foreignObject><style>body{display:none}</style></svg>"#;
        let clean = sanitize_svg(dirty).unwrap();
        assert_inert(&clean);
        assert!(!clean.contains("steal"));
        assert!(clean.contains(r#"<circle cx="5" cy="5" r="4" fill="red"/>"#));
        assert!(clean.contains("viewBox=\"0 0 10 10\""));
    }

    #[test]
    fn svg_sanitizer_neutralizes_urls() {
        let dirty = r##"<svg xmlns:xlink="http://www.w3.org/1999/xlink"><a href="javascript:alert(1)"><text>x</text></a><use xlink:href="  JaVaScRiPt:alert(1)"/><use href="#local"/><use xlink:href="#autre"/><use href="https://exemple.invalid/s.svg#a"/><animate attributeName="href" values="javascript:alert(1)"/><set attributeName="xlink:href" to="javascript:alert(1)"/></svg>"##;
        let clean = sanitize_svg(dirty).unwrap();
        assert_inert(&clean);
        assert!(clean.contains(r##"<use href="#local"/>"##));
        assert!(clean.contains(r##"<use href="#autre"/>"##));
        assert!(!clean.contains("<animate"));
        assert!(!clean.contains("<set"));
        assert!(!clean.contains("xlink"));
    }

    /// Charges d'attaque relevées par l'audit (A1) et variantes classiques.
    #[test]
    fn svg_sanitizer_resists_audit_payloads() {
        let payloads = [
            // Attribut précédé de « / » au lieu d'un blanc (contournait l'ancien filtre)
            r#"<svg><image href="x"/onerror="alert(1)"/></svg>"#,
            r#"<svg/onload="alert(1)"><circle r="4"/></svg>"#,
            r#"<svg><circle r="4"/onclick=alert(1)/></svg>"#,
            // Instruction de traitement, commentaire, CDATA, DOCTYPE et entités
            r#"<svg><?xml-stylesheet href="javascript:alert(1)"?><circle r="4"/></svg>"#,
            r#"<svg><?x <img src=x onerror=alert(1)> ?><circle r="4"/></svg>"#,
            r#"<svg><!--<img src=x onerror=alert(1)>--><circle r="4"/></svg>"#,
            r#"<svg><text><![CDATA[<img src=x onerror=alert(1)>]]></text></svg>"#,
            r#"<!DOCTYPE svg [<!ENTITY x "<script>alert(1)</script>">]><svg><text>&x;</text></svg>"#,
            // Casse, espaces de noms et éléments actifs
            r#"<svg><SCRIPT>alert(1)</SCRIPT><Circle ONLOAD="alert(1)"/></svg>"#,
            r#"<svg xmlns:h="http://www.w3.org/1999/xhtml"><h:script>alert(1)</h:script><h:iframe src="javascript:alert(1)"/></svg>"#,
            r#"<svg><foreignObject><body xmlns="http://www.w3.org/1999/xhtml"><img src="x" onerror="alert(1)"/></body></foreignObject></svg>"#,
            r#"<svg><style>@import url(https://exemple.invalid/x.css);</style></svg>"#,
            // Schémas exécutables masqués (entités, blancs, casse)
            r##"<svg><use href="&#106;avascript:alert(1)"/></svg>"##,
            "<svg><use href=\"java\tscript:alert(1)\"/></svg>",
            r##"<svg><rect style="background:url(javascript:alert(1))" width="4"/></svg>"##,
            r##"<svg><rect style="width:expression(alert(1))"/></svg>"##,
            r##"<svg><rect style="fill:u\72l(https://exemple.invalid/x)"/></svg>"##,
            r##"<svg><rect fill="url(https://exemple.invalid/x)"/></svg>"##,
            r##"<svg><image href="data:image/svg+xml;base64,PHN2Zy8+"/></svg>"##,
            r##"<svg><image href="data:text/html,&lt;script&gt;alert(1)&lt;/script&gt;"/></svg>"##,
            // Animations qui réécrivent un lien ou un gestionnaire
            r##"<svg><a><animate attributeName="href" to="javascript:alert(1)"/><text>x</text></a></svg>"##,
            r##"<svg><set attributeName="onclick" to="alert(1)"/></svg>"##,
            r##"<svg><animate attributeName="fill" values="url(https://exemple.invalid/x)"/></svg>"##,
            // Imbrications et balisage HTML ambigu
            "<svg><scr<script>x</script>ipt>alert(1)</script></svg>",
            r#"<svg><title><img src=x onerror=alert(1)></title></svg>"#,
            r#"<svg></p><img src=x onerror=alert(1)></svg>"#,
        ];
        for dirty in payloads {
            assert_refused_or_inert(dirty);
        }
    }

    /// Exports courants d'éditeurs : le dessin n'est pas perdu.
    #[test]
    fn svg_sanitizer_keeps_editor_exports() {
        // Illustrator : couleurs dans une feuille <style> reportées sur les éléments
        let illustrator = r##"<svg viewBox="0 0 10 10"><defs><style>/* calque */ .st0{fill:#38BDF8;} .st1, .st2 { stroke:#000 ; } svg rect{fill:red} @media print{.st0{fill:blue}}</style></defs><circle class="st0" r="4"/><rect class="st1" style="opacity:.5" width="2" height="2"/></svg>"##;
        let clean = sanitize_svg(illustrator).unwrap();
        assert!(clean.contains(r#"style="fill:#38BDF8""#), "{clean}");
        assert!(
            clean.contains(r#"style="stroke:#000;opacity:.5""#),
            "{clean}"
        );
        assert!(!clean.contains("<style") && !clean.contains("red") && !clean.contains("blue"));
        // Une règle dangereuse n'est pas reportée
        let evil = r##"<svg><style>.x{fill:url(http://e/a)} .y{behavior:url(#a)}</style><rect class="x y"/></svg>"##;
        assert!(!sanitize_svg(evil).unwrap().contains("style="));
        // draw.io : <switch> et liens internes conservés avec leur contenu
        let drawio = r##"<svg><switch><g><text>P</text></g></switch><a href="#x"><rect id="x" width="1"/></a><a href="http://e"><circle r="1"/></a></svg>"##;
        let clean = sanitize_svg(drawio).unwrap();
        assert!(
            clean.contains("<switch><g><text>P</text></g></switch>"),
            "{clean}"
        );
        assert!(
            clean.contains(r##"<a href="#x"><rect id="x" width="1"/></a>"##),
            "{clean}"
        );
        assert!(
            clean.contains("<a><circle"),
            "lien externe retiré : {clean}"
        );
        // Filtres d'éclairage et de déplacement
        let filter = r##"<svg><filter id="b"><feSpecularLighting surfaceScale="5" specularExponent="20" lighting-color="#fff"><fePointLight x="1" y="2" z="3"/></feSpecularLighting><feDisplacementMap scale="4" xChannelSelector="R"/></filter></svg>"##;
        let clean = sanitize_svg(filter).unwrap();
        assert!(
            clean.contains(r#"<fePointLight x="1" y="2" z="3"/>"#),
            "{clean}"
        );
        assert!(clean.contains(r#"specularExponent="20""#) && clean.contains(r#"scale="4""#));
        // Identifiant contenant « data: »
        let clean = sanitize_svg(r#"<svg><g id="metadata:x"/></svg>"#).unwrap();
        assert!(clean.contains(r#"id="metadata:x""#), "{clean}");
        assert_inert(&clean);
    }

    #[test]
    fn svg_sanitizer_drops_non_element_nodes() {
        let dirty = r#"<?xml version="1.0"?><!DOCTYPE svg><!-- en-tête --><svg><?pi x?><!--c--><text>A<![CDATA[B]]>&amp;C &lt; D&#33;&inconnue;</text></svg>"#;
        let clean = sanitize_svg(dirty).unwrap();
        assert_eq!(
            clean,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><text>A&amp;C &lt; D!</text></svg>"#
        );
    }

    #[test]
    fn svg_sanitizer_refuses_malformed_or_foreign_documents() {
        for bad in [
            "",
            "pas du svg",
            "<html><svg/></html>",
            "<svg><g></svg>",
            "<svg>",
            r#"<svg width="1" width="2"/>"#,
            "<svg:svg xmlns:svg=\"http://www.w3.org/2000/svg\"/>",
        ] {
            assert!(sanitize_svg(bad).is_err(), "accepté à tort : {bad:?}");
        }
    }

    #[test]
    fn svg_sanitizer_keeps_internal_references_and_rasters() {
        let svg = r##"<svg viewBox="0 0 10 10"><defs><linearGradient id="g1"><stop offset="0" stop-color="#fff"/></linearGradient></defs><rect width="10" height="10" fill="url(#g1)" style="fill:url('#g1');stroke:#000"/><image width="2" height="2" href="data:image/png;base64,iVBORw0KGgo="/><rect id="bad id" width="1"/></svg>"##;
        let clean = sanitize_svg(svg).unwrap();
        assert!(clean.contains(r##"fill="url(#g1)""##));
        assert!(clean.contains("style=\"fill:url('#g1');stroke:#000\""));
        assert!(clean.contains(r#"href="data:image/png;base64,iVBORw0KGgo=""#));
        assert!(!clean.contains("bad id"), "identifiant non sûr conservé");
    }

    #[test]
    fn svg_sanitizer_keeps_legit_symbols() {
        let legit = r##"<svg viewBox="0 0 100 100" width="84" height="84"><g class="sym-rotatable" style="transform-origin:50px 50px;"><circle cx="50" cy="50" r="36" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/></g></svg>"##;
        let clean = sanitize_svg(legit).unwrap();
        assert_eq!(
            clean,
            legit.replacen("<svg ", "<svg xmlns=\"http://www.w3.org/2000/svg\" ", 1)
        );
        // Idempotent : un SVG déjà assaini est reconstruit à l'identique
        assert_eq!(sanitize_svg(&clean).unwrap(), clean);
    }

    /// Les symboles de la bibliothèque d'exemple livrée restent affichables sans perte.
    #[test]
    fn svg_sanitizer_keeps_example_library() {
        let pack: serde_json::Value =
            serde_json::from_str(include_str!("../../../bibliotheque_exemple_iso10628.json"))
                .expect("bibliothèque d'exemple JSON");
        let symbols = pack["symbols"].as_array().expect("tableau symbols");
        assert!(!symbols.is_empty());
        for sym in symbols {
            let svg = sym["svg_content"].as_str().expect("svg_content");
            let clean =
                sanitize_svg(svg).unwrap_or_else(|e| panic!("symbole {} refusé : {e}", sym["id"]));
            assert_eq!(structure(&clean), structure(svg), "symbole {}", sym["id"]);
            assert_eq!(sanitize_svg(&clean).unwrap(), clean);
        }
    }

    /// Un SVG d'éditeur (Inkscape) : métadonnées et attributs propriétaires retirés, dessin conservé.
    #[test]
    fn svg_sanitizer_handles_editor_exports() {
        let inkscape = r##"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<!-- Created with Inkscape -->
<svg xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100" version="1.1" inkscape:version="1.2">
  <metadata><dc:title>Pompe</dc:title></metadata>
  <defs><radialGradient id="rg" xlink:href="#lg"/><linearGradient id="lg"><stop offset="1" style="stop-color:#38bdf8"/></linearGradient></defs>
  <g inkscape:label="Calque 1" inkscape:groupmode="layer" transform="translate(0,0)">
    <circle cx="50" cy="50" r="40" style="fill:url(#rg);stroke:currentColor;stroke-width:3"/>
    <text x="50" y="55" xml:space="preserve" text-anchor="middle">P&amp;1</text>
  </g>
</svg>"##;
        let clean = sanitize_svg(inkscape).unwrap();
        assert!(clean.starts_with(r#"<svg xmlns="http://www.w3.org/2000/svg" width="100""#));
        assert!(!clean.contains("inkscape"));
        assert!(!clean.contains("metadata"));
        assert!(clean.contains(r##"<radialGradient id="rg" href="#lg"/>"##));
        assert!(clean.contains(r#"style="fill:url(#rg);stroke:currentColor;stroke-width:3""#));
        assert!(clean.contains("P&amp;1</text>"));
        assert_eq!(sanitize_svg(&clean).unwrap(), clean);
    }

    #[test]
    fn vague2_politique_mot_de_passe_et_bearer() {
        assert!(validate_new_password(&"a".repeat(256)).is_ok());
        assert!(validate_new_password(&"a".repeat(257)).is_err());
        assert!(validate_new_password(&"é".repeat(129)).is_err());
        for schema in ["Bearer", "bearer", "bEaReR"] {
            assert_eq!(bearer_token(&format!("{schema} Secret")), "Secret");
        }
    }
}
