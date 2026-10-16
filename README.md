# RustScadaFlow

Plateforme de supervision industrielle **SCADA / GTB** (dans l'esprit de PcVue) avec un moteur de logique par flux (dans l'esprit de Node-RED). Elle tient en un seul exécutable Rust et s'utilise depuis un navigateur.

- **Temps réel** : base de variables en mémoire, filtrage par bande morte (COV) et diffusion WebSocket.
- **Drivers** : simulateur physique CVC / chaufferie / TGBT, Modbus TCP (maître, lecture par blocs de registres), MQTT. Leurs paramètres de communication sont modifiables à chaud. OPC-UA, BACnet/IP et Siemens S7 sont **simulés** à ce stade.
- **Automate Modbus TCP embarqué** (groupe froid simulé) sur `127.0.0.1:5502`, pour tester sans matériel.
- **Synoptiques** : studio glisser-déposer, 28 types de widgets, import de symboles SVG / Draw.io / JSON.
- **Flux logiques** : seuil, hystérésis, PID, portes logiques, temporisation TON, formule, script Rhai.
- **Alarmes ISA-18.2** à 4 états, avec journal, état conservé au redémarrage, bande morte anti-bagotement, mise en sommeil temporisée (désactivable globalement ou par variable) et une seule alarme de communication par équipement.
- **Planificateur horaire hebdomadaire**, historique SQLite avec export CSV, journal d'audit.
- **Contrôle d'accès** par rôles : Administrateur, Superviseur, Visualisation.

---

## Démarrage rapide (Windows)

Prérequis : [Rust](https://rustup.rs) (édition 2021).

```bat
cargo build --release
Demarrer-SCADA.bat
```

L'IHM s'ouvre sur <http://localhost:3000>.

| Script | Rôle |
|---|---|
| `Demarrer-SCADA.bat` | Lance le serveur (binaire `release` en priorité, sinon `debug`, sinon compilation) et ouvre le navigateur. |
| `Arreter-SCADA.bat` | Demande un arrêt propre, attend jusqu'à 10 s, puis force l'arrêt si nécessaire. |
| `Redemarrer-SCADA.bat` | Redémarre le moteur SCADA et les drivers actifs ; relance le serveur s'il est arrêté. |

Les scripts s'authentifient avec un **jeton de contrôle local** aléatoire. Le serveur l'écrit au démarrage dans `.scada_local_token` et le supprime à l'arrêt. Ce jeton n'est accepté que depuis la machine elle-même (`127.0.0.1`).

## Comptes

Au premier lancement, trois comptes d'usine sont créés :

| Identifiant | Mot de passe initial | Rôle |
|---|---|---|
| `admin` | `admin123` | Administrateur : tous les droits, dont système et utilisateurs |
| `superviseur` | `super123` | Superviseur : tout sauf système et utilisateurs |
| `visu` | `visu123` | Visualisation : synoptiques en lecture seule |

> ⚠️ **Changez ces mots de passe dès la première connexion** : badge utilisateur en haut à droite, puis « Changer mon mot de passe ». Tant qu'un compte garde son mot de passe d'usine, un avertissement apparaît dans les journaux du serveur à chaque démarrage.

## Configuration

Tous les paramètres sont lus dans des variables d'environnement :

| Variable | Défaut | Rôle |
|---|---|---|
| `SCADA_BIND` | `0.0.0.0` | Adresse d'écoute. `127.0.0.1` limite l'accès au poste local. |
| `SCADA_PORT` | `3000` | Port HTTP de l'IHM et de l'API (également lu par les scripts `.bat`). |
| `SCADA_DB` | `rust_scada_flow.db` | Chemin de la base SQLite. |
| `SCADA_MODBUS_SLAVE_PORT` | `5502` | Port de l'automate Modbus embarqué. |
| `SCADA_HISTORY_RETENTION_DAYS` | `30` | Rétention de l'historique ; `0` désactive la purge automatique. |
| `SCADA_SESSION_TTL_HOURS` | `12` | Expiration d'une session inactive. |
| `SCADA_STATIC_DIR` | auto | Dossier de l'IHM (par défaut `static/` du dossier courant ou situé au-dessus de l'exécutable). |
| `RUST_LOG` | `rust_scada_flow=info` | Niveau de journalisation. |

Exemple (PowerShell) : `$env:SCADA_BIND="127.0.0.1"; $env:SCADA_PORT="8080"; .\target\release\rust-scada-flow.exe`

## Sécurité

- Toute l'API et le WebSocket exigent une session. Les jetons sont aléatoires (256 bits) et expirent après inactivité. Le seul point d'accès sans authentification est `GET /api/health`.
- Les mots de passe sont hachés en **Argon2id**. Les anciens mots de passe en clair (versions ≤ 1.0) sont migrés automatiquement au démarrage.
- Un identifiant est verrouillé 60 s après 5 échecs de connexion.
- Les SVG importés sont assainis côté serveur et côté navigateur, et toutes les données affichées sont échappées.
- Chaque action est tracée dans le journal d'audit sous le nom de l'utilisateur authentifié.
- **Le serveur parle HTTP non chiffré.** Pour un accès réseau, placez-le derrière un proxy inverse HTTPS (Caddy, nginx, IIS) ou restreignez-le avec `SCADA_BIND=127.0.0.1`.

## Persistance

L'état des alarmes (acquittements, commentaires, mises en sommeil) est enregistré et restauré au redémarrage.

La base SQLite (mode WAL) est la source de vérité. La démonstration (synoptiques, flux, plannings, drivers par défaut) n'y est écrite **qu'au premier lancement** ; un élément supprimé ne réapparaît donc pas. Les sessions sont gardées en mémoire : il faut se reconnecter après un redémarrage du processus.

Sauvegarde et restauration du projet (drivers, tags, synoptiques, flux, plannings) : onglet **Historian & Système**, boutons Exporter et Importer (`.json`).

## Comportement des commandes

Une consigne envoyée vers un équipement est convertie en valeur brute (inverse de la mise à l'échelle du tag) puis transmise au driver. La valeur affichée provient toujours de la **relecture de l'équipement**. La commande est refusée si le driver est arrêté ou déconnecté, et les commandes en attente pendant une coupure Modbus ne sont pas rejouées à la reconnexion.

Un tag dont le « driver » ne correspond à aucun driver déclaré est une **variable interne** : la valeur y est appliquée directement.

## Scripts Rhai (nœud « Script Rhai »)

| Élément | Description |
|---|---|
| `val` | Valeur d'entrée du nœud |
| `tag("ID")` | Valeur numérique d'un tag |
| `tag_bool("ID")` | Valeur booléenne d'un tag |
| `tag_text("ID")` | Valeur texte d'un tag |
| `clamp(x, min, max)` | Bornage |
| `consigne` | Conservé pour compatibilité : consigne de `GTB.CTA01.Consigne_Temp`. Préférez `tag("ID")`. |

Les scripts sont limités en nombre d'opérations, en taille de chaînes et de tableaux et en profondeur d'appel.

## Développement

```bash
cargo test            # tests unitaires
cargo clippy          # analyse statique
cargo run             # serveur en mode debug
```

| Fichier | Rôle |
|---|---|
| `src/main.rs` | Serveur Axum, routes, middleware d'authentification, WebSocket |
| `src/core.rs` | Noyau temps réel `ScadaCore` : tags, drivers, alarmes, planning, historique, audit, utilisateurs |
| `src/security.rs` | Hachage des mots de passe, jetons, validation des identifiants, assainissement SVG |
| `src/config.rs` | Variables d'environnement |
| `src/models.rs` | Types de données partagés |
| `src/flow_engine.rs` | Moteur de flux et Rhai |
| `src/drivers/` | Drivers industriels et automate Modbus embarqué |
| `static/` | IHM (HTML / CSS / JavaScript sans framework) |

L'audit et le plan de correction (1.1), la revue de contrôle (1.2) et les évolutions de la 1.3 sont dans [docs/AUDIT_ET_PLAN.md](docs/AUDIT_ET_PLAN.md).
