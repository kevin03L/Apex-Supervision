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
| `crates/apex-build` | Dépendance de compilation (`build.rs`) des deux binaires : icône et `VERSIONINFO` Windows | Rien n'est lié aux exécutables ; sans effet pour une cible Linux. |
| `static/` | IHM web (HTML / CSS / JavaScript, sans framework ni étape de compilation) | Servie à la même origine que l'API. |
| `packaging/` | Installateur Windows (Inno Setup), paquet DEB (scripts de maintenance, unité systemd, règle polkit), icône (`icons/`) | Les métadonnées `cargo-deb` sont dans `crates/apex-server/Cargo.toml`. |

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
- **Windows** : les chaînes `x86_64-pc-windows-msvc` (référence pour la distribution, utilisée par la CI) et `x86_64-pc-windows-gnu` conviennent toutes les deux. L'icône et la `VERSIONINFO` des `.exe` sont compilées par `rc.exe` (Windows SDK, chaîne MSVC) ou par `windres` (binutils MinGW, par exemple WinLibs, chaîne GNU). Sans compilateur de ressources, la compilation aboutit avec un avertissement, mais `build-installer.ps1` refuse ces binaires.
- **Linux** : un compilateur C (`build-essential`), utilisé par SQLite et ring. Pour compiler l'utilitaire, voir la variable `LINUX_GUI_DEPS` dans `.github/workflows/ci.yml`.

La version est définie **une seule fois** dans `[workspace.package]` du `Cargo.toml` racine. Elle est propagée à tous les binaires, à `/api/health`, à l'IHM, aux installateurs et aux noms des artefacts. `build.rs` d'`apex-config` ajoute le commit Git et la cible de compilation (`apex-scada version`). Sous Windows, les propriétés des `.exe` en dérivent : version de fichier numérique `0.4.0.1` pour `0.4.0-alpha.1` (même règle que l'installateur), version du produit `0.4.0-alpha.1`. Contrôle : `(Get-Item target\release\apex-scada.exe).VersionInfo`.

**Icône** : `packaging/icons/generer-icones.ps1` (PowerShell 5.1, System.Drawing) dessine l'icône par code et produit `apex-supervision.svg`, `apex-supervision-256.png` (fenêtre de l'utilitaire, paquet Linux) et `apex-supervision.ico` (16 à 256 px : exécutables, installateur). Ces fichiers sont versionnés ; relancer le script après une modification du dessin, puis recompiler.

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
| `apex-server` | 5 + 9 | Unitaires : ligne de commande, instance unique. Intégration (boîte noire, tableau ci-dessous) |
| `apex-admin` | 16 | Client HTTP (réponses simples et découpées, serveur injoignable), journaux, diagnostics, formulaires, verrou d'instance, libellés |

**Tests d'intégration du serveur** (`crates/apex-server/tests/boite_noire.rs`, outillage dans `tests/common/mod.rs`). Chaque test lance le binaire `apex-scada` compilé (`CARGO_BIN_EXE_apex-scada`) :
- dans un dossier neuf et unique sous `target/tmp/apex-it/`, en mode portable, avec un fichier `--config` écrit par le test ;
- avec la simulation et le **mode sûr** actifs, sur des ports libres choisis dynamiquement (jamais 3000 ni 5502), le dossier de test comme dossier courant et un environnement nettoyé (`RUST_LOG`, `APEX_SCADA_CONFIG`, `SCADA_*`).

Le dialogue passe uniquement par HTTP et WebSocket, avec un client minimal écrit sur `std::net` : **aucune dépendance ajoutée**. `serde_json` est déjà une dépendance du serveur, et la poignée de main WebSocket est vérifiée avec la clé d'exemple de la RFC 6455, ce qui évite d'avoir à implémenter SHA-1. Des gardes `Drop` tuent le processus puis suppriment le dossier, même quand une assertion échoue. Dans ce cas, les journaux du moteur sont recopiés sur la sortie d'erreur du test.

| Test | Vérifie |
|---|---|
| `health_est_public_et_detaille_en_boucle_locale` | `GET /api/health` sans jeton ; en boucle locale, `runtime` (PID, mode `manual`, mode sûr, simulation, fichier de configuration, empreinte), `started_at` et `total_drivers` ; en-têtes de sécurité et `Cache-Control: no-store` |
| `authentification_jeton_obligatoire` | 401 sans jeton ou avec un jeton inconnu ; échec de connexion (même message pour un mot de passe faux et un compte inconnu) ; connexion `admin`/`admin123`, rôle `Admin`, hachage non publié ; `X-Scada-Token` et `Authorization: Bearer` ; jeton invalidé par la déconnexion |
| `rbac_visualisation_refuse_les_actions_admin` | Compte `visu` : lecture autorisée ; 403 sur `POST /api/system/shutdown`, sur la création d'utilisateur, sur `GET /api/users` et sur une consigne ; le serveur reste en marche et l'utilisateur n'est pas créé |
| `mode_sur_refuse_les_consignes_vers_un_driver_reel` | Consigne sur `MODBUS.GF01.Consigne_Eau_Glacee` (driver `MODBUS_GF01`) : 403 « Mode sûr », y compris avec le jeton de contrôle local ; une consigne sur un driver simulé (`GTB.CTA01.Consigne_Temp`) reste acceptée ; avertissement présent dans le journal |
| `websocket_exige_un_jeton_puis_diffuse_les_donnees` | `/ws/realtime` : 401 sans jeton ou avec un jeton inconnu ; avec jeton, `InitialSnapshot` puis au moins un `TagUpdateBatch` ; profil Visualisation : instantané sans drivers, flux ni plannings |
| `code_3_configuration_invalide` | Port 0, TOML illisible, paramètre inconnu, format trop récent : code 3, sans base créée, et erreur consignée dans `logs/apex-scada-startup.log` |
| `code_4_deuxieme_instance_meme_dossier` | Deuxième processus sur le même dossier : code 4, et la première instance continue de répondre |
| `code_5_port_occupe` | Port web tenu par le test : code 5, détecté avant l'ouverture de la base |
| `arret_propre_par_jeton_de_controle_local` | `POST /api/system/shutdown` : 401 sans jeton ; avec `run/control.token`, processus terminé avec le code 0, « arrêté proprement » dans la sortie et dans le journal fichier, jeton supprimé, port libéré |

Ce que ces tests ne couvrent pas : l'absence des détails `runtime` pour un client distant (il faudrait une seconde interface réseau), le service Windows et l'unité systemd, ainsi que tout équipement réel.

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
- **sur un tag `v*`** (ou à la demande) : installateur Windows et paquet DEB, avec leurs empreintes SHA-256. Le paquet DEB est **installé, démarré, interrogé, arrêté, redémarré, mis à jour (réinstallation), désinstallé puis purgé** sur le runner.

## 5. Publication d'une version

1. Mettre à jour `version` dans `[workspace.package]`, en suivant la convention `0.4.0-alpha.N` → `0.5.0-beta.N` → `1.0.0`.
2. `cargo build` pour mettre à jour `Cargo.lock`, puis vérifier `cargo test --workspace`.
3. Commit, puis tag `v<version>` et push : la CI produit les artefacts.
4. Publier les artefacts **avec leurs fichiers `.sha256`**.

Construction locale :
- installateur Windows : `packaging\windows\build-installer.ps1` (Inno Setup 6 requis) ;
- paquet DEB (sous Linux, `cargo install cargo-deb --locked`) : `cargo build --release --locked -p apex-server -p apex-admin`, puis `cargo deb -p apex-server --no-build --output dist/`. Le paquet contient les deux programmes : les deux doivent être compilés avant.

Depuis un poste Windows, la compilation et le paquet se font dans un conteneur `ubuntu:22.04` (glibc 2.35, comme la CI) avec `build-essential`, `curl`, `pkg-config`, les paquets de `LINUX_GUI_DEPS` et rustup. Placez `CARGO_HOME` et `CARGO_TARGET_DIR` dans des volumes Docker nommés : un dossier Windows monté est très lent. Pour tester le paquet, il faut un conteneur avec systemd comme PID 1 (`--privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw`, commande `/sbin/init`). Supprimez `/usr/sbin/policy-rc.d` des images Ubuntu : ce script empêche `apt` de démarrer, d'arrêter et de redémarrer les services, ce qui fausse le test.

**Signature** (prévue avant la bêta) : Authenticode pour les binaires et l'installateur (directive `SignTool` déjà préparée dans le script Inno Setup), et signature du paquet DEB ou d'un dépôt APT.

## 6. Ajouter un paramètre de configuration

1. `apex-config/src/model.rs` : champ, valeur par défaut sûre, validation dans `validate()`.
2. `apex-config/src/store.rs` : ligne commentée dans `render()`, puis test `render_roundtrip_is_lossless`.
3. Si le format change de façon incompatible : incrémenter `CONFIG_VERSION` et ajouter l'étape dans `migrate.rs`, avec son test.
4. Le transmettre au moteur (`EngineOptions` dans `apex-server/src/app.rs`).
5. L'exposer dans l'utilitaire : `app.rs` (onglet Configuration) et, s'il est essentiel, `wizard.rs`.
6. Le documenter dans [ADMINISTRATION.md](ADMINISTRATION.md) §2.
