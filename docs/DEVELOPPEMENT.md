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
| `apex-build` | 4 | Version de fichier Windows dérivée de la version du paquet |
| `apex-config` | 24 | Valeurs par défaut sûres, validation par champ, migrations (v0 → v1, format trop récent), écriture atomique, sauvegarde et restauration, conflit, verrou, erreurs lisibles, chemins système et portables |
| `apex-core` | 67 | Moteur : alarmes et escalade, bande morte, mise en sommeil, historique, sécurité (assainissement SVG, verrouillage et courses de connexion, sessions), drivers Modbus (encodage, plages), flux, compatibilité des données |
| `apex-server` | 13 + 21 | Unitaires : ligne de commande, instance unique, reprise 1.x. Intégration : `boite_noire.rs` (14), `drivers_automatismes.rs` (4), `exploitation.rs` (3), tableau ci-dessous |
| `apex-admin` | 17 | Client HTTP (réponses simples et découpées, serveur injoignable), journaux, diagnostics, formulaires, verrou d'instance, libellés |

**Tests d'intégration du serveur** (`crates/apex-server/tests/boite_noire.rs`, outillage dans `tests/common/mod.rs`). Chaque test lance le binaire `apex-scada` compilé (`CARGO_BIN_EXE_apex-scada`) :
- dans un dossier neuf et unique sous `target/tmp/apex-it/`, en mode portable, avec un fichier `--config` écrit par le test ;
- avec la simulation et le **mode sûr** actifs, sur des ports libres choisis dynamiquement (jamais 3000 ni 5502), le dossier de test comme dossier courant et un environnement nettoyé (`RUST_LOG`, `APEX_SCADA_CONFIG`, `SCADA_*`).

Le dialogue passe uniquement par HTTP et WebSocket, avec un client minimal écrit sur `std::net` : **aucune dépendance ajoutée**. `serde_json` est déjà une dépendance du serveur, et la poignée de main WebSocket est vérifiée avec la clé d'exemple de la RFC 6455, ce qui évite d'avoir à implémenter SHA-1. Des gardes `Drop` tuent le processus puis suppriment le dossier, même quand une assertion échoue. Dans ce cas, les journaux du moteur sont recopiés sur la sortie d'erreur du test.

| Test | Vérifie |
|---|---|
| `health_est_public_et_detaille_en_boucle_locale` | `GET /api/health` sans jeton : aucun détail d'exécution, même en boucle locale (ni avec un jeton inventé) ; avec le jeton de contrôle local, `runtime` (PID, mode `manual`, mode sûr, simulation, fichier de configuration, empreinte), `started_at` et `total_drivers` ; en-têtes de sécurité et `Cache-Control: no-store` ; détails absents dès qu'un en-tête de proxy (`X-Forwarded-For`, `Forwarded`, `X-Real-IP`) est présent |
| `authentification_jeton_obligatoire` | 401 sans jeton ou avec un jeton inconnu ; échec de connexion (même message pour un mot de passe faux et un compte inconnu) ; connexion `admin`/`admin123`, rôle `Admin`, hachage non publié ; `X-Scada-Token` et `Authorization: Bearer` ; jeton invalidé par la déconnexion |
| `verrouillage_resiste_aux_tentatives_concurrentes` | 30 connexions simultanées avec un mot de passe faux : exactement 5 réponses 401, 25 réponses 403 ; le bon mot de passe est refusé pendant le verrouillage, les autres comptes restent utilisables |
| `reinitialisation_du_mot_de_passe_ferme_les_sessions` | Nouveau mot de passe donné par l'administrateur : sessions du compte fermées ; sur son propre compte, l'administrateur garde sa session courante ; modification sans mot de passe : sessions conservées |
| `rbac_visualisation_refuse_les_actions_admin` | Compte `visu` : lecture autorisée ; 403 sur `POST /api/system/shutdown`, sur la création d'utilisateur, sur `GET /api/users` et sur une consigne ; le serveur reste en marche et l'utilisateur n'est pas créé |
| `visualisation_consulte_et_acquitte_une_alarme_mais_pas_plus` | Compte `visu` : liste et journal des alarmes lisibles, permissions `can_view_alarms` et `can_ack_alarms` mais pas `can_manage_alarms` ; acquittement unitaire autorisé (404 sur une alarme inconnue, pas 403) ; 403 sur l'acquittement global, la purge, la mise en sommeil et l'historique d'une variable absente des courbes de tendance |
| `les_sessions_survivent_au_redemarrage_mais_pas_aux_revocations` | Après un arrêt brutal et une relance : sessions ouvertes reprises, session fermée par déconnexion refusée ; compte désactivé puis relance : sa session est refusée ; aucun jeton en clair dans les fichiers de la base (`.db`, `.db-wal`) |
| `mode_sur_refuse_les_consignes_vers_un_driver_reel` | Consigne sur `MODBUS.GF01.Consigne_Eau_Glacee` (driver `MODBUS_GF01`) : 403 « Mode sûr » ; refusée aussi avec le jeton de contrôle local (réservé à l'arrêt) ; une consigne sur un driver simulé (`GTB.CTA01.Consigne_Temp`) reste acceptée ; planning et flux visant le driver réel signalés suspendus (`suspended_reason`, `blocked_outputs`), planning visant le simulateur non ; avertissement présent dans le journal |
| `websocket_exige_un_jeton_puis_diffuse_les_donnees` | `/ws/realtime` : 401 sans jeton ou avec un jeton inconnu ; avec jeton, `InitialSnapshot` puis au moins un `TagUpdateBatch` ; profil Visualisation : instantané sans drivers, flux ni plannings |
| `code_3_configuration_invalide` | Port 0, TOML illisible, paramètre inconnu, format trop récent : code 3, sans base créée, et erreur consignée dans `logs/apex-scada-startup.log` |
| `code_4_deuxieme_instance_meme_dossier` | Deuxième processus sur le même dossier : code 4, et la première instance continue de répondre |
| `code_5_port_occupe` | Port web tenu par le test : code 5, détecté avant l'ouverture de la base |
| `jeton_de_controle_local_reserve_a_l_arret_propre` | Jeton `run/control.token` : `GET /api/system/status` accepté ; 403 sur les utilisateurs, les tags, l'export, le redémarrage et le WebSocket ; 401 dès qu'un en-tête de proxy est présent ; aucun compte créé |
| `auto_decouverte_refusee_sur_un_driver_reel` (drivers_automatismes) | 400 pour Modbus TCP et MQTT, aucune variable créée |
| `variable_modbus_inscriptible_en_lecture_seule_refusee` | Variable inscriptible sur `DI`/`IR` refusée à l'enregistrement |
| `flux_avec_script_rhai_invalide_refuse` | Script Rhai vide ou invalide : 400, flux non enregistré |
| `import_sans_simulation_ecarte_les_drivers_simules` | Import d'un projet sans simulation : drivers simulés et variables écartés (`ignored_drivers`, `ignored_tags`) |
| `arret_propre_reporte_le_journal_wal_dans_la_base` (exploitation) | Après un arrêt propre, les données sont dans le fichier principal de la base |
| `migrate_legacy_force_sauvegarde_une_base_restee_en_wal` | Base en place encore en WAL : refus sans `--force`, sauvegarde complète (`.bak-<date>`) avec `--force` |
| `imports_volumineux_acceptes_jusqu_a_leur_limite` | 3 Mo acceptés, limite + 1 octet → 413 (projet 64 Mo, symboles 16 Mo, autres routes 2 Mo) ; compte Visualisation refusé (403) avant l'envoi du corps |
| `arret_propre_par_jeton_de_controle_local` | `POST /api/system/shutdown` : 401 sans jeton ; avec `run/control.token`, processus terminé avec le code 0, « arrêté proprement » dans la sortie et dans le journal fichier, jeton supprimé, port libéré ; arrêt accepté avec le compte `admin` désactivé ; après relance, l'arrêt figure au journal d'audit sous l'acteur « Utilitaire local » |

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

### Signature de code (SignPath Foundation)

Apex Supervision s'appuie sur le programme gratuit pour projets open source de **SignPath Foundation** pour signer numériquement ses exécutables Windows (`apex-scada.exe`, `apex-scada-admin.exe`, l'installateur et le désinstalleur) dans la chaîne d'intégration continue GitHub Actions. L'éditeur affiché par Windows est « SignPath Foundation ».

La signature est effectuée **exclusivement par la CI**, sur des étiquettes de version (`v*`), après validation manuelle du propriétaire. Aucune clé privée n'est manipulée sur les postes de travail ni stockée dans le dépôt.

#### Étapes de mise en service pour le propriétaire

1. **Rendre le dépôt public** :
   Dans les paramètres GitHub du dépôt (`Settings` → `General` → `Danger Zone` → `Change repository visibility`), basculer le dépôt en public (SignPath Foundation exige un dépôt public et sous licence libre approuvée par l'OSI).
2. **Publier une première version alpha (non signée)** :
   Poser une étiquette Git (par exemple `v0.4.0-alpha.1`) et la pousser sur GitHub. Le workflow de CI produira une première GitHub Release avec les binaires, l'installateur Windows et le paquet DEB non signés.
3. **Déposer la candidature sur signpath.org** :
   Créer un compte sur [signpath.org](https://signpath.org) et soumettre le projet Apex Supervision en indiquant l'adresse du dépôt public et la licence GPL-3.0.
4. **Configurer SignPath et GitHub** :
   Une fois la candidature validée par l'équipe de SignPath Foundation :
   - Installer l'application GitHub de SignPath sur l'organisation ou le compte propriétaire ;
   - Créer le projet SignPath (par défaut slug : `apex-supervision`) et la politique de signature (par défaut slug : `release-signing`) ;
   - Dans le projet SignPath, configurer les trois définitions d'artefacts en collant le contenu des fichiers versionnés dans le dépôt :
     - `.signpath/artifact-configurations/binaires.xml` (slug : `binaires`) ;
     - `.signpath/artifact-configurations/desinstalleur.xml` (slug : `desinstalleur`) ;
     - `.signpath/artifact-configurations/installateur.xml` (slug : `installateur`) ;
   - Définir dans les paramètres GitHub du dépôt (`Settings` → `Secrets and variables` → `Actions`) :
     - **Secret** `SIGNPATH_API_TOKEN` : jeton d'API SignPath autorisant la soumission des requêtes de signature ;
     - **Variable** `SIGNPATH_ORGANIZATION_ID` : identifiant de l'organisation SignPath ;
     - **Variables optionnelles** (si différentes des valeurs par défaut) : `SIGNPATH_PROJECT_SLUG` (`apex-supervision`) et `SIGNPATH_SIGNING_POLICY_SLUG` (`release-signing`).
5. **Étiqueter une version et approuver la signature** :
   Créer et pousser l'étiquette de publication (ex. `v0.4.0-alpha.2`). Le job CI soumettra la demande de signature. Le propriétaire reçoit une notification et approuve manuellement la signature dans l'interface SignPath. Les binaires signés sont ensuite intégrés à l'installateur et publiés dans la GitHub Release.

> **Remarque : signature locale ou certificat OV / EV alternatif**
> La chaîne de construction conserve la prise en charge de la signature Authenticode locale via `packaging/windows/signer.ps1` et `build-installer.ps1 -Signer` :
> - Certificat PFX : variables `APEX_SIGN_PFX` et `APEX_SIGN_PFX_PASSWORD` (ou secrets CI `APEX_SIGN_PFX_BASE64` et `APEX_SIGN_PFX_PASSWORD`) ;
> - Certificat magasin / clé matérielle : `APEX_SIGN_THUMBPRINT` ;
> - Azure Artifact Signing : `APEX_SIGN_ARTIFACT_METADATA`, `APEX_SIGNTOOL` et `APEX_SIGN_DLIB`.
> En mode local ou avec un certificat d'essai auto-signé, définir `APEX_SIGN_TEST=1`.
> Remarque sur SmartScreen : même avec un binaire signé, un nouvel éditeur peut devoir bâtir sa réputation auprès de Microsoft SmartScreen avant que les avertissements de premier téléchargement ne disparaissent totalement.

## 6. Ajouter un paramètre de configuration

1. `apex-config/src/model.rs` : champ, valeur par défaut sûre, validation dans `validate()`.
2. `apex-config/src/store.rs` : ligne commentée dans `render()`, puis test `render_roundtrip_is_lossless`.
3. Si le format change de façon incompatible : incrémenter `CONFIG_VERSION` et ajouter l'étape dans `migrate.rs`, avec son test.
4. Le transmettre au moteur (`EngineOptions` dans `apex-server/src/app.rs`).
5. L'exposer dans l'utilitaire : `app.rs` (onglet Configuration) et, s'il est essentiel, `wizard.rs`.
6. Le documenter dans [ADMINISTRATION.md](ADMINISTRATION.md) §2.
