# Refonte « installable » — Audit, architecture cible et plan de migration

- **Date** : 16/10/2026
- **État de départ** : dépôt `kevin03L/Apex-Supervision`, commit `dc78b28` (v1.3.0).
- **Périmètre** : Windows 10/11 et Linux. **macOS hors périmètre.**
- **Statut** : alpha (voir §9 pour le versionnement).

Ce document répond aux étapes 1 (audit) et 2 (architecture) du cahier des charges. Il fixe le plan de migration avant toute refactorisation importante.

---

## 1. État des lieux synthétique

### 1.1 Organisation actuelle

Le projet tient en **une seule crate binaire** `rust-scada-flow` (édition 2021), d'environ 7 000 lignes de Rust et 4 000 lignes de HTML/JS/CSS. Elle compte 265 dépendances transitives et produit un binaire `release` de 25 Mo.

| Module | Rôle | Couplage notable |
|---|---|---|
| `main.rs` | Point d'entrée unique : configuration, base SQLite, routes Axum, WebSocket, arrêt et redémarrage | Mélange démarrage, serveur HTTP et cycle de vie |
| `core.rs` (≈3 900 l.) | `ScadaCore` : tags, drivers, alarmes, planificateur, historique, audit, utilisateurs, symboles, réglages | Centre fonctionnel, sain |
| `drivers/` | Simulateur GTB, Modbus TCP maître (lecture par blocs), MQTT, et OPC-UA / BACnet / S7 **simulés** ; automate Modbus esclave embarqué | Les protocoles simulés ne sont pas signalés comme tels dans l'IHM |
| `flow_engine.rs` | Moteur de flux et scripts Rhai | Écrit des consignes dès le démarrage |
| `models.rs` | Types partagés. `ScadaError` implémente `IntoResponse` d'Axum | Fait dépendre les modèles du framework web |
| `security.rs` | Argon2id, jetons, validation des identifiants, assainissement SVG | Réutilisable |
| `config.rs` | 7 variables d'environnement `SCADA_*` | Aucun fichier de configuration, aucun répertoire système |
| `static/` | IHM web (HTML/JS/CSS sans framework ni build) | À conserver telle quelle |

- **Serveur web** : Axum 0.7 et Tokio, avec IHM statique servie à la même origine et WebSocket `/ws/realtime`.
- **Données** : SQLite en WAL, un seul fichier (configuration métier, historique, audit, comptes, état des alarmes).
- **Protocoles réels** : Modbus TCP (`tokio-modbus`) et MQTT (`rumqttc`, TLS via rustls).
- **Tests** : 37 tests unitaires (`cargo test`), `clippy` propre, aucune intégration continue.

### 1.2 Problèmes qui empêchent une installation propre

| # | Problème | Conséquence |
|---|---|---|
| P1 | Tous les chemins sont **relatifs au dossier courant** : base `rust_scada_flow.db`, jeton `.scada_local_token`, dossier `static/` (résolu automatiquement depuis la 1.2). | Impossible de placer le programme dans `Program Files` ou `/usr` et les données ailleurs. |
| P2 | **Pas de fichier de configuration**, seulement des variables d'environnement. | Rien à éditer pour un assistant ou un utilitaire desktop ; aucune validation persistante. |
| P3 | **Aucune gestion des signaux d'arrêt** : seul l'appel API `/api/system/shutdown` déclenche l'arrêt propre. | Un arrêt par systemd (`SIGTERM`), Ctrl+C ou le gestionnaire de services Windows tue le processus sans vider l'historique ni fermer les drivers. |
| P4 | **Aucun mode service** : pas d'intégration au gestionnaire de services Windows (SCM), pas d'unité systemd. | Impossible de fonctionner en arrière-plan sans session ouverte. |
| P5 | **Le « redémarrage complet » relance un nouveau processus** avec `Command::new(current_exe)`. | En mode service, cela crée un processus orphelin hors du contrôle du gestionnaire de services. |
| P6 | **Aucun verrou d'instance unique.** | Deux instances peuvent ouvrir la même base. La seconde échoue sur le port, mais seulement après avoir démarré ses drivers. |
| P7 | **Journaux uniquement sur la sortie standard.** | Rien à consulter pour un service Windows ou un utilitaire desktop. |
| P8 | Le jeton de contrôle local est écrit dans le dossier courant, sans permissions dédiées. | Inadapté à une installation multi-utilisateur. |
| P9 | **Effets physiques au démarrage** : le planificateur applique ses consignes et les flux écrivent leurs sorties dès le lancement. L'automate embarqué et le simulateur démarrent toujours. | Les tests de démarrage et de configuration pourraient piloter de vrais équipements. Aucun « mode sûr » n'existe pour la mise en service. |
| P10 | **Les drivers OPC-UA, BACnet/IP et Siemens S7 sont simulés** sans que l'IHM le dise. | Contraire à l'exigence « aucune fonctionnalité simulée présentée comme opérationnelle ». |
| P11 | **Versionnement en 1.x** (fixé lors des itérations précédentes) alors que le projet est en alpha. | Incohérent avec SemVer (voir §9). |
| P12 | **Pas d'intégration continue**, pas de build Linux vérifié. | Aucune plateforme réellement validée hormis Windows en `windows-gnu`. |

### 1.3 Composants à conserver, modifier ou isoler

| Composant | Décision | Justification |
|---|---|---|
| `core.rs`, `drivers/`, `flow_engine.rs`, `security.rs` | **Conserver** et déplacer dans une crate bibliothèque | Logique métier testée. Aucune réécriture n'est nécessaire. |
| `models.rs` | **Modifier** : retirer la dépendance à Axum (`IntoResponse` déplacée côté serveur) | Le moteur ne doit pas dépendre du framework web. |
| `config.rs` | **Remplacer** par un gestionnaire de configuration versionné et partagé | Il est lu par le moteur et par l'utilitaire desktop. |
| `main.rs` | **Découper** : démarrage (CLI, journaux, instance unique, service) et routes HTTP | Séparation des responsabilités. |
| `static/` | **Conserver** | L'interface web reste l'outil des opérateurs. |
| Scripts `.bat` | **Remplacer** par le service, l'utilitaire desktop et la CLI | Ils ne fonctionnent qu'en mode « dossier courant ». |
| Automate embarqué et simulateur | **Isoler** derrière une option `simulation` | Utiles en démonstration, interdits par défaut en production. |

---

## 2. Architecture cible

### 2.1 Principe

Le **moteur et le serveur web restent dans un seul processus**. Les séparer en deux processus imposerait un protocole interne pour chaque appel de l'IHM, sans bénéfice pour l'exploitation : ils démarrent, s'arrêtent et se configurent ensemble. La séparation se fait donc **par crates**, pas par processus.

```
┌──────────────────────────── Poste / serveur ─────────────────────────────┐
│                                                                          │
│  apex-scada (service Windows / unité systemd)     apex-scada-admin (GUI) │
│  ┌──────────────────────────────────────────┐    ┌─────────────────────┐ │
│  │ apex-server  : CLI, service, journaux,   │    │ egui : config,      │ │
│  │                instance unique, Axum,    │◄───┤ état, journaux,     │ │
│  │                IHM web + WebSocket       │HTTP│ diagnostics,        │ │
│  │ apex-core    : moteur GTC/SCADA (tags,   │127.│ assistant initial   │ │
│  │                drivers, alarmes, flux,   │0.0.1└──────┬──────────────┘ │
│  │                planning, historique)     │           │                │
│  └───────────────┬──────────────────────────┘           │ lecture /      │
│                  │ lit au démarrage                     │ écriture       │
│                  ▼                                      ▼ atomique       │
│        ┌──────────────────────────────────────────────────────────┐      │
│        │ apex-config : fichier TOML versionné, validation,        │      │
│        │ migrations, sauvegardes, chemins système (Windows/Linux) │      │
│        └──────────────────────────────────────────────────────────┘      │
│  Navigateur opérateur ──HTTP(S)──► IHM web (inchangée)                   │
└──────────────────────────────────────────────────────────────────────────┘
```

### 2.2 Espace de travail Cargo (workspace)

| Crate | Type | Contenu | Dépendances principales |
|---|---|---|---|
| `crates/apex-config` | bibliothèque | Modèle de configuration v1, valeurs par défaut sûres, validation, migrations, écriture atomique, sauvegardes, **couche OS** (chemins, permissions), métadonnées de version | `serde`, `toml`, `thiserror` (légère, sans Tokio) |
| `crates/apex-core` | bibliothèque | Moteur existant : `core`, `drivers`, `flow_engine`, `models`, `security` | inchangées (sans Axum) |
| `crates/apex-server` | binaire `apex-scada` | Démarrage, CLI (`run`, `config check/show/init`, `service install/uninstall`), journaux, instance unique, signaux, service Windows, routes Axum | `apex-core`, `apex-config`, `axum`, `windows-service` (Windows uniquement) |
| `crates/apex-admin` | binaire `apex-scada-admin` | Utilitaire desktop et assistant de configuration initiale | `apex-config`, `eframe/egui`, client HTTP local minimal |

L'utilitaire desktop **ne dépend pas du moteur** (ni SQLite, ni Rhai, ni Modbus) : il reste léger et fonctionne même si le moteur ne démarre pas.

### 2.3 Communication entre l'utilitaire et le moteur

| Besoin | Mécanisme retenu | Pourquoi |
|---|---|---|
| Lire et modifier la configuration | **Fichier TOML partagé** via `apex-config` : validation, écriture atomique (fichier temporaire puis `rename`), sauvegarde horodatée, verrou d'écriture | Fonctionne moteur arrêté ou port erroné. Le moteur ne réécrit jamais ce fichier : il n'a qu'**un seul rédacteur**. |
| Savoir si un redémarrage est nécessaire | Le moteur publie l'empreinte de la configuration chargée (`/api/health`, en boucle locale). L'utilitaire la compare au fichier sur disque. | Détection fiable des changements non appliqués. |
| État du moteur | Gestionnaire de services (SCM / `systemctl`) et `GET /api/health` en boucle locale | Distingue « service arrêté », « service démarré mais web injoignable » et « en ligne ». |
| Démarrer, arrêter, redémarrer | Gestionnaire de services (mode service) ou processus détaché (mode manuel), plus arrêt propre via le **jeton de contrôle local** | Aucun port d'administration supplémentaire n'est exposé. |
| Journaux | Lecture directe des fichiers du répertoire de journaux | Disponible même moteur arrêté. |

Aucune interface d'administration n'est exposée sur le réseau : le jeton de contrôle local n'est accepté que depuis `127.0.0.1` / `::1`, et son fichier est protégé par les permissions du système.

### 2.4 Cycle de vie

| Mode | Lancement | Arrêt propre | Redémarrage |
|---|---|---|---|
| **Service** (par défaut après installation) | SCM Windows (`apex-scada service run`) ou systemd | Arrêt SCM, `SIGTERM` ou API | Par le gestionnaire de services. Le moteur ne relance **jamais** lui-même un processus en mode service. |
| **Manuel / portable** (développement, démonstration) | `apex-scada run`, ou bouton de l'utilitaire (processus détaché) | Ctrl+C, `SIGTERM`, API | L'utilitaire arrête puis relance le processus. |

Mécanismes transverses :
- **Instance unique** : verrou exclusif sur `data_dir/apex-scada.lock` (`File::try_lock` de la bibliothèque standard), pris **avant** le démarrage des drivers.
- **Échec de démarrage détecté tôt** : configuration invalide, port occupé ou base inaccessible arrêtent le processus avec un code de sortie dédié et un message explicite dans les journaux, **avant** toute communication industrielle.
- **Signaux** : Ctrl+C, `SIGTERM` et arrêt SCM déclenchent le même arrêt propre (drivers, dernier lot d'historique, jeton local).

---

## 3. Choix technologiques

### 3.1 Utilitaire desktop

| Critère | **egui / eframe** (retenu) | Tauri 2 | Slint |
|---|---|---|---|
| Langage de l'interface | Rust | HTML/JS + Rust | DSL `.slint` + Rust |
| Dépendances Windows | Aucune (rendu wgpu : DirectX 12, Vulkan ou WARP logiciel) | WebView2 (présent sur Win 10/11, absent de certaines images LTSC) | Aucune |
| Dépendances Linux | X11 ou Wayland, `libxkbcommon`, pilote GL ou Vulkan (ou rendu logiciel) | **WebKitGTK** : versions très différentes selon les distributions, paquets lourds | X11 ou Wayland, fontconfig |
| Taille ajoutée | ≈ 5 à 8 Mo | ≈ 3 Mo, plus la WebView du système | ≈ 4 à 6 Mo |
| Chaîne de build | `cargo` seul (compatible `windows-gnu`) | `cargo` + outillage frontend + CLI Tauri | `cargo` + compilateur Slint |
| Sécurité | Pas de moteur web embarqué | Surface d'une WebView (CSP, IPC à durcir) | Pas de moteur web |
| Licence | MIT / Apache-2.0 | MIT / Apache-2.0 | GPLv3, licence libre de redevance (attribution) ou commerciale |
| Adéquation | Outil d'administration technique (formulaires, tableaux, journaux) : cas d'usage typique d'egui | Pertinent pour réutiliser l'IHM web, ce qui n'est **pas** le but ici | Bon rendu, mais DSL et licence à gérer |

**Décision : egui/eframe avec le rendu wgpu.** C'est du Rust pur, sans moteur web ni chaîne frontend, la même base de code sert Windows et Linux, et le rendu logiciel de secours couvre les PC industriels et machines virtuelles sans GPU.

### 3.2 Installateur Windows

| Critère | **Inno Setup** (retenu en alpha) | WiX (MSI) |
|---|---|---|
| Assistant avec pages personnalisées | Simple (Pascal Script) | Laborieux (dialogues MSI) |
| Déploiement en masse (GPO, Intune) | Possible en mode silencieux | Natif |
| Outillage | `ISCC.exe` (≈ 6 Mo) | WiX v4+ demande le SDK .NET |
| Mises à jour en place | Oui (`AppId` stable) | Oui (`UpgradeCode`) |

**Décision : Inno Setup (`.exe`) pour l'alpha.** Le MSI pourra s'ajouter avant la bêta si un déploiement d'entreprise est visé. Le fichier de packaging restera indépendant du code.

### 3.3 Linux

- **Paquet DEB** construit avec `cargo-deb`, unité systemd, utilisateur système `apex-scada` sans shell, scripts `postinst` / `prerm` / `postrm`.
- **Distributions cibles proposées** : Debian 12, Ubuntu 22.04 LTS et Ubuntu 24.04 LTS. Le build se fait sur Ubuntu 22.04 (glibc 2.35) pour couvrir les trois.
- **RPM** (RHEL / Rocky / Alma 9) : reporté, sauf besoin exprimé. Le moteur pourrait alors être compilé en musl statique.
- **Archive `.tar.gz`** : non prévue en alpha (aucun besoin identifié).

### 3.4 Autres choix

| Besoin | Choix | Justification |
|---|---|---|
| Format de configuration | **TOML** | Lisible, commentable, natif dans l'écosystème Rust (`toml`). |
| Journaux fichiers | `tracing-appender` (rotation quotidienne, rétention configurable) | Prolonge `tracing`, déjà utilisé. |
| Service Windows | crate `windows-service` | Dispatcher SCM officiel en Rust, compatible `windows-gnu`. |
| Signaux | `tokio::signal` | Déjà présent via Tokio, aucune dépendance ajoutée. |

---

## 4. Arborescence d'installation

| Élément | Windows | Linux (DEB) | Mode portable (dev) |
|---|---|---|---|
| Binaires | `C:\Program Files\Apex Supervision\` | `/usr/bin/apex-scada`, `/usr/bin/apex-scada-admin` | dossier du dépôt |
| IHM web | `…\Apex Supervision\web\` | `/usr/share/apex-supervision/web/` | `./static/` |
| Configuration | `%ProgramData%\Apex Supervision\config\apex-scada.toml` | `/etc/apex-supervision/apex-scada.toml` | `./apex-scada.toml` |
| Sauvegardes de configuration | `…\config\backups\` | `/etc/apex-supervision/backups/` | `./backups/` |
| Données (base, verrou) | `%ProgramData%\Apex Supervision\data\` | `/var/lib/apex-supervision/` | `./data/` |
| Journaux | `%ProgramData%\Apex Supervision\logs\` | `/var/log/apex-supervision/` (et journald) | `./logs/` |
| Jeton de contrôle local | `%ProgramData%\Apex Supervision\run\control.token` | `/run/apex-supervision/control.token` | `./run/` |

Les données sont hors du dossier du programme : une mise à jour ou une désinstallation ne les touche pas.

---

## 5. Configuration (format v1)

| Section | Paramètres | Application |
|---|---|---|
| `config_version` | `1` | Migrations versionnées |
| `[instance]` | `name` | Redémarrage (titre de l'IHM, journaux) |
| `[server]` | `bind` (défaut **`127.0.0.1`**), `port` (défaut `3000`) | Redémarrage |
| `[paths]` | `data_dir`, `log_dir`, `web_dir` (vides = défauts système) | Redémarrage |
| `[database]` | `file` (défaut `apex-scada.db` dans `data_dir`) | Redémarrage |
| `[history]` | `retention_days` (défaut 30, 0 = illimité) | Redémarrage |
| `[security]` | `session_ttl_hours` (défaut 12) | Redémarrage |
| `[logging]` | `level` (`error` à `trace`), `file_enabled`, `retention_files` | Redémarrage |
| `[safety]` | `safe_mode` (défaut `false`) : bloque toute écriture vers les équipements, le planificateur et les sorties de flux | Redémarrage |
| `[simulation]` | `enabled` (défaut `false` en installation, `true` en mode portable), `modbus_slave_port` | Redémarrage |

- Les réglages modifiables à chaud (mise en sommeil des alarmes, etc.) restent en base et se gèrent depuis l'IHM web.
- Les variables `SCADA_*` restent acceptées en **surcharge** (compatibilité), et sont journalisées comme telles.
- **Aucun secret** n'est stocké dans le fichier : les comptes sont en base (Argon2id) et le jeton local est régénéré à chaque démarrage. Si des identifiants de protocole (MQTT, OPC-UA) sont ajoutés, ils iront dans un fichier séparé aux permissions restreintes.
- **Bind par défaut à `127.0.0.1`** : l'exposition réseau est un choix explicite fait dans l'assistant, qui avertit que le serveur parle HTTP non chiffré.

---

## 6. Permissions et sécurité

| Opération | Windows | Linux |
|---|---|---|
| Exécution du moteur | Service sous `LocalService` (droits minimaux), ACL d'écriture sur `data`, `logs` et `run` | Utilisateur `apex-scada` sans shell, durcissement systemd (`ProtectSystem=strict`, `NoNewPrivileges`, `ReadWritePaths`) |
| Lecture de la configuration et des journaux | Utilisateurs | Groupe `apex-scada` |
| Modification de la configuration | Administrateurs (l'utilitaire demande l'élévation UAC) | `root` ou groupe `apex-scada` (fichier `0660`) |
| Démarrer / arrêter le service | Administrateurs (élévation UAC à la demande) | Règle polkit pour le groupe `apex-scada` |
| Jeton de contrôle local | ACL : SYSTEM, Administrateurs, LocalService | `0640 apex-scada:apex-scada` |

L'utilitaire s'exécute **en utilisateur normal**. Seules les actions qui le nécessitent demandent une élévation, au moment où elles sont lancées.

---

## 7. Mise à jour et désinstallation

- **Mise à jour** : l'installateur remplace les binaires et l'IHM. Configuration et données ne sont jamais écrasées. Au démarrage, le moteur applique les migrations de configuration (avec sauvegarde préalable) et de base.
- **Données des versions précédentes** : une commande `apex-scada migrate-legacy --from <ancien dossier>` **copie** l'ancienne base `rust_scada_flow.db`, sans jamais la déplacer ni la supprimer. L'assistant la propose s'il en détecte une.
- **Désinstallation** : supprime les binaires et le service. Elle **conserve** la configuration, les données et les journaux, sauf case explicite « supprimer aussi les données » (Windows) ou `apt purge` (Linux), toujours documentés.
- **Mises à jour automatiques** : aucune en alpha. Les artefacts sont publiés avec leurs empreintes SHA-256. La signature Authenticode (Windows) et la signature des paquets (Linux) sont prévues avant la bêta.

---

## 8. Plan de migration

| Étape | Contenu | Vérifiable sur ce poste |
|---|---|---|
| **3a** Socle | Workspace, `apex-config` (modèle, validation, migrations, écriture atomique, sauvegardes, chemins), découplage d'Axum des modèles | ✅ Compilation et tests Windows |
| **3b** Cycle de vie | CLI, journaux fichiers, instance unique, signaux, mode sûr, option simulation, migration des données existantes | ✅ Exécution Windows en mode manuel |
| **3c** Service Windows | Intégration SCM (`service run/install/uninstall`) | ⚠️ Compilation seulement. L'installation d'un service exige les droits administrateur. |
| **4** Utilitaire desktop | Configuration, état, démarrage/arrêt, journaux, diagnostics, restauration, assistant initial | ✅ Windows (hors actions élevées) |
| **5a** Installateur Windows | Script Inno Setup, raccourcis, service, lancement de l'assistant | ⚠️ Nécessite Inno Setup et un test d'installation administrateur |
| **5b** Paquet Linux | `cargo-deb`, unité systemd, polkit, scripts de maintenance | ⚠️ Nécessite Docker (build + test dans un conteneur Ubuntu) |
| **6** CI et documentation | GitHub Actions (Windows + Ubuntu) : `fmt`, `clippy`, `test`, artefacts ; documentation d'installation et d'administration | ⚠️ Exécutée seulement après un push |

---

## 9. Versionnement

**Constat** : la version d'origine était `0.1.0`. Les itérations précédentes l'ont portée à `1.1.0`, `1.2.0` puis `1.3.0`. Au sens de SemVer, une version 1.x annonce une API et un comportement stables, ce qui contredit le statut alpha.

**Proposition** : `0.4.0-alpha.1` pour cette refonte. La version mineure prend la suite des trois itérations déjà livrées (0.1 → 1.1 → 1.2 → 1.3, soit trois évolutions). Le suffixe `-alpha.N` marque la phase. Ensuite :
- `0.4.0-alpha.N` : itérations de la refonte ;
- `0.5.0-beta.1` : premier gel fonctionnel ;
- `1.0.0` : première version de production.

La version est définie **une seule fois** (`[workspace.package] version`) et propagée au moteur, à l'utilitaire, à l'IHM (via `/api/health`), à l'installateur Windows (`AppVersion`), au paquet DEB, et aux noms des artefacts (`apex-supervision-0.4.0-alpha.1-windows-x64-setup.exe`).

---

## 10. Risques et points bloquants

| Risque | Mesure |
|---|---|
| Régression fonctionnelle lors du découpage en crates | Déplacement de code sans réécriture, 37 tests existants conservés, test d'IHM headless rejoué. |
| Effets physiques pendant les tests | `safe_mode` et simulation désactivée par défaut en installation ; tests uniquement contre le simulateur et l'automate embarqué. |
| Perte de données existantes | Migration par **copie** uniquement ; sauvegarde avant toute migration de configuration. |
| **Pas de droits administrateur sur ce poste** | Le service Windows et l'installateur ne peuvent pas être testés ici. Ces points seront marqués « à valider manuellement ». |
| **Pas d'outillage d'installeur ni de Linux actif** | Il faut installer Inno Setup (winget) et démarrer Docker Desktop. Sinon, les configurations de packaging seront écrites mais **non vérifiées**, et signalées comme telles. |
| Rendu egui sur certains postes (RDP, VM sans GPU) | Rendu wgpu avec repli logiciel ; à valider sur le parc cible. |
| Chaîne `windows-gnu` | Suffisante pour tout le projet ; la CI utilisera MSVC, la cible de référence pour les binaires Windows distribués. |

---

## 11. Critères de passage en bêta

1. Installateurs Windows et Linux **vérifiés** (installation, mise à jour, désinstallation) sur Windows 10, Windows 11, Debian 12, Ubuntu 22.04 et Ubuntu 24.04.
2. Service : redémarrage automatique après crash, arrêt propre en moins de 10 s, aucune perte d'historique à l'arrêt.
3. CI verte sur Windows et Linux (`fmt`, `clippy -D warnings`, `test`), artefacts reproductibles avec empreintes SHA-256.
4. Signature Authenticode des binaires et de l'installateur ; paquets Linux signés.
5. HTTPS natif ou procédure de proxy inverse validée.
6. Protocoles simulés (OPC-UA, BACnet, S7) soit implémentés, soit retirés de l'IHM de production.
7. Tests d'intégration du serveur web automatisés en CI (authentification, RBAC, WebSocket, écriture Modbus contre l'automate embarqué).
8. Documentation d'exploitation relue par un utilisateur cible.

---

## 12. Bilan de la version 0.4.0-alpha.1

### 12.1 Réalisé

| Étape | Contenu livré |
|---|---|
| 3a Socle | Workspace de 4 crates. `apex-config` : modèle v1, validation par champ, migrations, écriture atomique sous verrou, détection de conflit, 20 sauvegardes, couche OS. `apex-core` découplé d'Axum. |
| 3b Cycle de vie | CLI (`run`, `config check/show/init/migrate`, `migrate-legacy`, `service …`, `version`), codes de sortie documentés, journaux fichiers avec rotation, instance unique (PID dans `apex-scada.pid`), réservation du port avant les drivers, arrêt propre unifié (API, Ctrl+C, SIGTERM, SCM) avec suspension des automatismes avant l'arrêt des drivers, mode sûr, option simulation, reprise des bases 1.x par copie. |
| 3c Service Windows | `service install/uninstall/start/stop/restart/status/enable/disable`, compte LocalService, permissions des répertoires, redémarrage après échec, messages SCM explicites. L'installation est refusée **avant** toute création de répertoire si les droits manquent. |
| 4 Utilitaire desktop | `apex-scada-admin` (egui 0.37, rendu wgpu) : état, démarrage, arrêt et redémarrage (service ou manuel), configuration validée, journaux, diagnostics, restauration des sauvegardes, assistant initial (5 étapes avec retour arrière), relance en administrateur, protection contre la fermeture avec des modifications non enregistrées. |
| 5a Installateur Windows | `packaging/windows/apex-supervision.iss` et `build-installer.ps1` : installation, service, assistant, mise à jour (arrêt, `config migrate`, redémarrage), désinstallation conservant les données sauf demande explicite, empreinte SHA-256. |
| 5b Paquet Linux | Métadonnées `cargo-deb` (paquet `apex-supervision`), `postinst` et `postrm`, unité systemd durcie, règle polkit, raccourci desktop. |
| 6 CI et documentation | `.github/workflows/ci.yml` (Windows et Ubuntu 22.04, artefacts sur tag), [INSTALLATION.md](INSTALLATION.md), [ADMINISTRATION.md](ADMINISTRATION.md), [DEVELOPPEMENT.md](DEVELOPPEMENT.md), README. |
| Exigence « rien de simulé présenté comme réel » | L'IHM web regroupe et étiquette OPC-UA, BACnet/IP et S7 comme « SIMULÉ », avec un badge sur les drivers concernés. Le moteur refuse ces drivers si la simulation est désactivée. |

### 12.2 État des vérifications

| Vérification | Résultat |
|---|---|
| `cargo test --workspace` (Windows, windows-gnu) | ✅ 76 tests réussis |
| `cargo clippy --workspace --all-targets` (Windows) | ✅ aucun avertissement |
| `clippy -D warnings` de `apex-admin` et `apex-config` pour `x86_64-unknown-linux-gnu` | ✅ compilation croisée sans avertissement |
| Compilation Linux complète, sous Docker (`ubuntu:22.04`, glibc 2.35, Rust 1.99.1, paquets de `LINUX_GUI_DEPS`) : `cargo build --release --locked -p apex-server -p apex-admin`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, `cargo fmt --all --check` | ✅ exécutée : aucune erreur, aucun avertissement, **76 tests réussis**. Le code `cfg(unix)` / `cfg(not(windows))` n'a demandé aucune correction. Le symbole le plus récent requis est `GLIBC_2.35` : il faut glibc 2.35 ou plus récente. |
| `cargo fmt --check` | ✅ appliqué à tout le workspace (commit dédié) |
| Moteur en mode portable (Windows) : démarrage, `/api/health`, 2ᵉ instance (code 4, PID affiché), port occupé (code 5), configuration invalide (code 3), arrêt propre par jeton, `config migrate` v0 → v1, `migrate-legacy` sur une copie de la base 1.x (source inchangée, empreinte SHA-256 identique) | ✅ exécuté |
| Utilitaire desktop piloté par UI Automation : assistant complet, enregistrement, démarrage du moteur, tableau de bord, configuration, diagnostics, arrêt avec confirmation, erreur de port | ✅ exécuté |
| Utilitaire : onglets Journaux, Sauvegardes, À propos | ⚠️ compilés, non vérifiés visuellement |
| IHM web : nom du produit, version, étiquetage « SIMULÉ » | ✅ vérifié dans un navigateur |
| `service install` sans droits administrateur : refus explicite, aucun répertoire créé | ✅ exécuté |
| Service Windows installé et piloté (SCM) | ⚠️ **à valider manuellement** : il faut les droits administrateur |
| Installateur Windows | ⚠️ **compilé** par Inno Setup (`apex-supervision-0.4.0-alpha.1-windows-x64-setup.exe`), **non exécuté**. Installation, mise à jour et désinstallation restent à valider sur Windows 10 et 11. |
| Paquet DEB : `cargo install cargo-deb --locked` (3.8.0) puis `cargo deb -p apex-server --no-build` | ✅ construit (`apex-supervision_0.4.0~alpha.1-1_amd64.deb`, 10,5 Mo, deux constructions successives identiques octet pour octet). Une seule correction : le résumé du paquet dépassait 79 caractères et cargo-deb le coupait sur deux lignes ; la `description` d'`apex-server` a été raccourcie. |
| Paquet DEB testé sous Docker avec **systemd réel** (PID 1, `--privileged --cgroupns=host`) sur **Ubuntu 22.04** (systemd 249), **Debian 12** (systemd 252) et **Ubuntu 24.04** (systemd 255) | ✅ 35 vérifications réussies sur chaque distribution : `apt install` ; utilisateur et groupe `apex-scada` ; `/etc/apex-supervision/apex-scada.toml` en `0660 root:apex-scada` (dossier `2775`, `backups/` en `2770`) ; écoute par défaut sur `127.0.0.1:3000` ; service actif et activé ; `/api/health` (`"status":"ok"`, `run_mode` `service`) ; IHM web servie depuis `/usr/share/apex-supervision/web` ; `config check` ; `systemctl stop` puis « arrêté proprement » dans `journalctl` (`Result=success`, code 0) ; deux `restart` ; mise à jour par réinstallation (service relancé, nouveau PID, droits rétablis) ; `apt remove` (service arrêté, configuration, données et compte conservés) ; `apt purge` (configuration, données, journaux, compte, groupe et lien d'activation supprimés) ; réinstallation après purge. |
| Mise à jour avec conversion de la configuration (fichier v0 sans `config_version`), Ubuntu 22.04 | ✅ `postinst` convertit le fichier en v1, avec une sauvegarde dans `backups/` ; les valeurs sont conservées et les droits rétablis. Les commentaires ajoutés à la main sont perdus, comme prévu (documenté). |
| Mise à jour d'un service arrêté ou désactivé (Debian 12) | ✅ Un service activé mais arrêté est **redémarré** par la mise à jour (comportement standard de debhelper) ; un service désactivé reste arrêté. Comportement documenté dans INSTALLATION.md §2.4. |
| Durcissement systemd (`ProtectSystem=strict`, `MemoryDenyWriteExecute`, `RestrictAddressFamilies`, `PrivateDevices`, aucune capacité…) | ✅ Aucun refus dans le journal. Base SQLite (WAL), journaux fichiers, PID et verrou fonctionnent dans `StateDirectory`, `LogsDirectory` et `RuntimeDirectory`. Avec simulation et mode sûr, l'automate Modbus embarqué écoute bien sur `127.0.0.1:5502`. `systemd-analyze security` : exposition 3.0 « OK ». |
| Règle polkit (membre du groupe `apex-scada`, sans mot de passe) | ✅ Debian 12 (polkit 122) et Ubuntu 24.04 (polkit 124) : `systemctl restart` accepté ; `disable` refusé (authentification demandée) ; un utilisateur hors du groupe ne peut pas arrêter le service. Ubuntu 22.04 (polkit 0.105) : règle ignorée, authentification demandée, comme documenté. |
| Étape « Test d'installation » de la CI, rejouée à l'identique (Ubuntu 22.04 avec systemd, utilisateur non root avec `sudo`, `bash -e`, journal vidé) | ✅ L'étape d'origine passait. Elle a été complétée, puis rejouée avec succès : droits de la configuration, attente de journald avant la recherche de « arrêté proprement », redémarrage, mise à jour par réinstallation, contrôles après `remove` et `purge`. Le workflow n'a pas encore tourné sur GitHub (aucun push). |
| Utilitaire sous Linux : X11 (Xvfb, rendu logiciel Mesa), Ubuntu 24.04, compte du groupe `apex-scada` | ✅ Démarre et affiche l'état du service (PID, mode service, démarrage automatique) et les derniers avertissements du journal. « Redémarrer… » puis la confirmation redémarrent le service via polkit, sans mot de passe (nouveau PID). |
| Utilitaire sous Wayland, ou avec un GPU réel | ⚠️ non exécuté |
| Paquet sur machines réelles ou VM (hors conteneur), mise à jour depuis une version antérieure du paquet | ⚠️ non exécuté : une seule version du paquet existe |

Plateformes compilées : Windows x86_64 (gnu) et Linux x86_64 (glibc ≥ 2.35, compilé sous Docker).

Anomalies relevées, non corrigées :
- à la purge sur Ubuntu 22.04, dpkg affiche `directory '/usr/lib/systemd/system' not empty so not removed`. L'avertissement est sans conséquence : cargo-deb installe l'unité dans `/usr/lib/systemd/system`, alors que le paquet systemd de cette version déclare `/lib/systemd/system` (fusion de `/usr`) ;
- `crates/apex-config/build.rs` lit le commit avec `git` et surveille `../../.git/HEAD`. Dans un worktree Git (`.git` est alors un fichier), ou sans `git` installé, la version affiche `inconnu` et le script de build est relancé à chaque compilation ;
- le texte de l'utilitaire « Les actions sur le service demandent une confirmation administrateur (UAC sous Windows) » s'affiche aussi sous Linux, même quand la règle polkit dispense de confirmation.

### 12.3 Écarts par rapport au plan

- **Un seul paquet DEB** (`apex-supervision`, moteur et utilitaire) au lieu de paquets séparés. Les bibliothèques graphiques de l'utilitaire sont chargées à l'exécution ; elles sont donc « suggérées », sans s'imposer aux serveurs sans interface.
- **La configuration Linux n'est pas un conffile dpkg** : `postinst` la génère avec `apex-scada config init`, puis la met à jour avec `config migrate`. On évite ainsi les conflits entre dpkg et l'utilitaire, qui la réécrit, ainsi que les droits de lecture inadaptés pour le moteur.
- `config migrate` a été ajoutée. L'ancienne consigne (`config init --force`) aurait remplacé la configuration par les valeurs par défaut.
- Les scripts `Demarrer/Arreter/Redemarrer-SCADA.bat` sont supprimés : ils visaient le binaire `rust-scada-flow.exe`, qui n'existe plus. Ils sont remplacés par l'utilitaire, le service et la CLI.
- Utilitaire : Rust 1.95 minimum (eframe 0.37) ; le moteur reste compatible avec Rust 1.89.

### 12.4 Limites connues et travaux avant la bêta

En plus des critères du §11 :
- valider l'installateur Windows et le service sur Windows 10 et 11 (installation, mise à jour depuis une version antérieure, désinstallation avec et sans suppression des données) ;
- le paquet DEB est validé en conteneur systemd sur Debian 12, Ubuntu 22.04 et 24.04 (§12.2). Restent à valider : des machines réelles, une mise à jour depuis une version antérieure du paquet, l'utilitaire sous Wayland et avec un GPU réel, et un premier passage de la CI sur GitHub. Sur Ubuntu 22.04, polkit 0.105 ignore la règle JavaScript : une authentification est demandée pour chaque action ;
- icône de l'application et ressources de version Windows (`VERSIONINFO`) des binaires ;
- licence du logiciel à définir (paquet marqué `LicenseRef-Proprietary`, aucune licence dans le dépôt) ;
- l'utilitaire lance des actions élevées par PowerShell (`Start-Process -Verb RunAs`) : seul le code de sortie est connu, pas le message détaillé ;
- tests d'intégration automatisés du serveur web (authentification, RBAC, WebSocket) et de l'utilitaire, encore manuels.
