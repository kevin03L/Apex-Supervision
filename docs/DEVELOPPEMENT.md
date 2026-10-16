# Apex Supervision — Guide du développeur

## 1. Architecture

Le moteur et le serveur web forment **un seul processus**. La séparation des responsabilités passe par les crates du workspace, et non par plusieurs processus (justification : [REFONTE_ALPHA.md](REFONTE_ALPHA.md) §2).

```
apex-admin (bin apex-scada-admin) ──► apex-config ◄── apex-server (bin apex-scada) ──► apex-core
      egui, sans moteur                 TOML, chemins       CLI, service, Axum          moteur métier
```

| Crate | Rôle | À savoir |
|---|---|---|
| `crates/apex-config` | Modèle de configuration v1, validation, migrations, écriture atomique sous verrou avec détection de conflit, sauvegardes, **couche OS** (`paths.rs` : seul endroit qui connaît les conventions Windows et Linux), version et codes de sortie | Sans Tokio ni SQLite : partagée par le moteur et l'utilitaire. |
| `crates/apex-core` | Moteur : tags, drivers, alarmes, flux et Rhai, planificateur, historique, audit, utilisateurs, sécurité | Indépendant d'Axum. `ScadaError::http_status()` donne le code HTTP, que le serveur applique. Le **mode sûr** est appliqué au point de passage unique des commandes (`write_tag`). |
| `crates/apex-server` | `main.rs` (CLI), `app.rs` (cycle de vie : démarrage contrôlé, arrêt propre), `http.rs` (routes, WebSocket), `service.rs` (SCM Windows), `instance.rs` (instance unique), `logging.rs`, `legacy.rs` (variables `SCADA_*`, reprise des bases 1.x), `db.rs` | Ordre de démarrage documenté en tête de `app.rs` : tout échec survient **avant** toute communication industrielle. |
| `crates/apex-admin` | Utilitaire desktop : `app.rs` (fenêtre, onglets), `wizard.rs` (assistant), `control.rs` (service, UAC, polkit, lancement manuel), `probe.rs` (client HTTP local minimal), `logs.rs`, `diag.rs`, `form.rs` | Ne dépend pas du moteur. Toute opération lente s'exécute dans un fil séparé. |
| `static/` | IHM web (HTML / CSS / JavaScript, sans framework ni étape de compilation) | Servie à la même origine que l'API. |
| `packaging/` | Installateur Windows (Inno Setup), paquet DEB (scripts de maintenance, unité systemd, règle polkit) | Les métadonnées `cargo-deb` sont dans `crates/apex-server/Cargo.toml`. |

Règles de conception :
- Le moteur **ne réécrit jamais** le fichier de configuration : il n'a qu'un seul rédacteur, l'utilitaire ou la CLI.
- Aucun secret dans le fichier de configuration.
- Aucun chemin ni aucune commande propre à un OS hors de `apex-config/src/paths.rs`, `apex-server/src/service.rs` et `apex-admin/src/control.rs`.
- Les commentaires et les messages destinés aux utilisateurs sont rédigés en français.

## 2. Compilation

```bash
cargo build                         # debug, tout le workspace
cargo build --release               # binaires distribués (LTO « thin », symboles retirés)
cargo run -p apex-server -- help    # aide de la CLI du moteur
```

- **Rust** : le moteur exige 1.89 (`File::try_lock`), l'utilitaire 1.95 (eframe 0.37).
- **Windows** : les chaînes `x86_64-pc-windows-msvc` (référence pour la distribution, utilisée par la CI) et `x86_64-pc-windows-gnu` conviennent toutes les deux.
- **Linux** : un compilateur C (`build-essential`), utilisé par SQLite et ring. Pour compiler l'utilitaire, voir la variable `LINUX_GUI_DEPS` dans `.github/workflows/ci.yml`.

La version est définie **une seule fois** dans `[workspace.package]` du `Cargo.toml` racine. Elle est propagée à tous les binaires, à `/api/health`, à l'IHM, aux installateurs et aux noms des artefacts. `build.rs` d'`apex-config` ajoute le commit Git et la cible de compilation (`apex-scada version`).

## 3. Tests

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

| Crate | Tests | Couverture principale |
|---|---|---|
| `apex-config` | 18 | Valeurs par défaut sûres, validation par champ, migrations (v0 → v1, format trop récent), écriture atomique, sauvegarde et restauration, conflit, verrou, erreurs lisibles, chemins système et portables |
| `apex-core` | 37 | Moteur : alarmes, bande morte, mise en sommeil, historique, sécurité, compatibilité des données |
| `apex-server` | 5 | Ligne de commande, instance unique |
| `apex-admin` | 16 | Client HTTP (réponses simples et découpées, serveur injoignable), journaux, diagnostics, formulaires, verrou d'instance, libellés |

**Scénarios manuels**, sur une instance portable dans un dossier temporaire et sur un autre port, **jamais sur une base d'exploitation** :

```bash
mkdir /tmp/essai && cd /tmp/essai
apex-scada config init --portable --simulation --safe-mode --port 3917
apex-scada run &                      # puis : 2e instance → code 4 ; même port ailleurs → code 5
curl http://127.0.0.1:3917/api/health
curl -X POST -H "x-scada-token: $(cat run/control.token)" http://127.0.0.1:3917/api/system/shutdown
```

L'utilitaire expose son interface à l'accessibilité (AccessKit). Il peut ainsi être piloté par UI Automation sous Windows pour des tests de bout en bout : assistant, démarrage, arrêt, erreurs de port.

## 4. Intégration continue

`.github/workflows/ci.yml` :
- **à chaque push et pull request** : `fmt`, puis `clippy -D warnings` et `test` sous Windows et Ubuntu 22.04 ;
- **sur un tag `v*`** (ou à la demande) : installateur Windows et paquet DEB, avec leurs empreintes SHA-256. Le paquet DEB est **installé, démarré, interrogé, arrêté et purgé** sur le runner.

## 5. Publication d'une version

1. Mettre à jour `version` dans `[workspace.package]`, en suivant la convention `0.4.0-alpha.N` → `0.5.0-beta.N` → `1.0.0`.
2. `cargo build` pour mettre à jour `Cargo.lock`, puis vérifier `cargo test --workspace`.
3. Commit, puis tag `v<version>` et push : la CI produit les artefacts.
4. Publier les artefacts **avec leurs fichiers `.sha256`**.

Construction locale :
- installateur Windows : `packaging\windows\build-installer.ps1` (Inno Setup 6 requis) ;
- paquet DEB : `cargo deb -p apex-server` (après `cargo build --release`).

**Signature** (prévue avant la bêta) : Authenticode pour les binaires et l'installateur (directive `SignTool` déjà préparée dans le script Inno Setup), et signature du paquet DEB ou d'un dépôt APT.

## 6. Ajouter un paramètre de configuration

1. `apex-config/src/model.rs` : champ, valeur par défaut sûre, validation dans `validate()`.
2. `apex-config/src/store.rs` : ligne commentée dans `render()`, puis test `render_roundtrip_is_lossless`.
3. Si le format change de façon incompatible : incrémenter `CONFIG_VERSION` et ajouter l'étape dans `migrate.rs`, avec son test.
4. Le transmettre au moteur (`EngineOptions` dans `apex-server/src/app.rs`).
5. L'exposer dans l'utilitaire : `app.rs` (onglet Configuration) et, s'il est essentiel, `wizard.rs`.
6. Le documenter dans [ADMINISTRATION.md](ADMINISTRATION.md) §2.
