# Règles communes aux agents (Claude, Antigravity)

Apex Supervision : supervision GTC/SCADA en Rust (workspace `crates/`) avec une IHM web statique
(`static/index.html`, `static/styles.css`, `static/synoptic.js`, `static/app.js`, sans framework ni build).

## Langue et style
- Textes de l'IHM, messages, commentaires et documentation : **en français**.
- Suivre le style du code voisin (densité de commentaires, nommage, idiomes).
- Pas d'émojis dans les libellés de l'IHM : icônes SVG en ligne.

## Règles impératives
- Ne jamais toucher à `rust_scada_flow.db` ni aux bases d'exploitation ; tester uniquement sur des
  instances portables temporaires, sur des ports autres que 3000 et 5502.
- Aucune commande vers un équipement industriel réel.
- Ne rien commiter, pousser ni créer de branche : c'est l'humain qui décide.
- IHM : toute donnée insérée dans le HTML passe par `esc()`, tout argument de gestionnaire `onclick`
  par `jsArg()`. Aucun SVG importé n'est inséré en HTML brut (voir `sanitizeSvg`).
- Le serveur reste la référence des droits : l'IHM masque, le serveur refuse.

## Vérifications
- JavaScript : `node --check static/*.js`.
- Rust : `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
