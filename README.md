# Apex Supervision

Plateforme de supervision industrielle **GTC / SCADA / GTB** écrite en Rust : un moteur temps réel avec une IHM web pour les opérateurs, et un utilitaire desktop pour l'installation et l'administration.

> **Statut : version alpha (`0.4.0-alpha.1`).** L'architecture et les fondations (configuration, service, installateurs, journaux, tests) sont en place ; les critères de passage en bêta et les limites connues sont listés dans [docs/REFONTE_ALPHA.md](docs/REFONTE_ALPHA.md).

## Fonctionnalités

- **Temps réel** : base de variables en mémoire, filtrage par bande morte (COV) et diffusion WebSocket.
- **Communications** : Modbus TCP maître (lecture par blocs de registres) et MQTT (TLS), avec des paramètres modifiables à chaud. **OPC-UA, BACnet/IP et Siemens S7 sont simulés** : ils servent à la démonstration et l'IHM les signale comme tels.
- **Simulation** (option) : simulateur physique CVC / chaufferie / TGBT et automate Modbus TCP embarqué, pour tester sans matériel.
- **Synoptiques** : studio glisser-déposer, 28 types de widgets, import de symboles SVG / Draw.io / JSON.
- **Flux logiques** : seuil, hystérésis, PID, portes logiques, temporisation TON, formule, script Rhai.
- **Alarmes ISA-18.2** à 4 états : journal, état conservé au redémarrage, bande morte anti-bagotement, mise en sommeil désactivable.
- **Planificateur horaire hebdomadaire**, historique SQLite avec export CSV, journal d'audit.
- **Contrôle d'accès** par rôles : Administrateur, Superviseur, Visualisation.
- **Exploitation** : service Windows ou unité systemd, installateur Windows, paquet DEB, utilitaire desktop, mode sûr de mise en service, journaux fichiers avec rotation.

## Documentation

| Document | Contenu |
|---|---|
| [docs/INSTALLATION.md](docs/INSTALLATION.md) | Installation, mise à jour et désinstallation (Windows, Linux, mode portable), reprise des données des versions 1.x |
| [docs/ADMINISTRATION.md](docs/ADMINISTRATION.md) | Configuration, utilitaire desktop, ligne de commande, codes de sortie, mode sûr, dépannage |
| [docs/DEVELOPPEMENT.md](docs/DEVELOPPEMENT.md) | Architecture, compilation, tests, intégration continue, publication |
| [docs/REFONTE_ALPHA.md](docs/REFONTE_ALPHA.md) | Audit, architecture cible, plan de migration, bilan de l'alpha et critères de passage en bêta |
| [docs/AUDIT_ET_PLAN.md](docs/AUDIT_ET_PLAN.md) | Historique des versions 1.1 à 1.3 |

## Démarrage rapide (développement, mode portable)

Prérequis : [Rust](https://rustup.rs) 1.95 ou plus récent.

```bash
cargo build
target/debug/apex-scada config init --portable --simulation   # crée ./apex-scada.toml
target/debug/apex-scada run                                    # Ctrl+C pour arrêter proprement
target/debug/apex-scada-admin --config apex-scada.toml         # utilitaire desktop
```

L'IHM web est disponible sur <http://localhost:3000>. Pour une installation en service, voir [docs/INSTALLATION.md](docs/INSTALLATION.md).

## Comptes

Au premier démarrage, trois comptes d'usine sont créés :

| Identifiant | Mot de passe initial | Rôle |
|---|---|---|
| `admin` | `admin123` | Administrateur : tous les droits, dont système et utilisateurs |
| `superviseur` | `super123` | Superviseur : tout sauf système et utilisateurs |
| `visu` | `visu123` | Visualisation : synoptiques en lecture seule |

> ⚠️ **Changez ces mots de passe dès la première connexion.** Pour cela, cliquez sur le badge utilisateur en haut à droite, puis sur « Changer mon mot de passe ». Tant qu'un compte garde son mot de passe d'usine, le moteur l'indique dans ses journaux à chaque démarrage.

## Sécurité

- Toute l'API et le WebSocket exigent une session. Les jetons sont aléatoires (256 bits) et expirent après inactivité. Seul `GET /api/health` est public. Il ne publie les détails d'exécution qu'aux connexions locales.
- Les mots de passe sont hachés en **Argon2id**. Un identifiant est verrouillé 60 s après 5 échecs.
- Les SVG importés sont assainis, côté serveur comme côté navigateur, et toutes les données affichées sont échappées.
- Le serveur écoute sur `127.0.0.1` par défaut. **Il parle HTTP non chiffré** : pour un accès réseau, placez-le derrière un proxy inverse HTTPS (Caddy, nginx, IIS).
- Le service tourne avec des droits minimaux (LocalService sous Windows, utilisateur `apex-scada` et unité durcie sous Linux).

## Comportement des commandes

Une consigne envoyée à un équipement est convertie en valeur brute (inverse de la mise à l'échelle du tag), puis transmise au driver. La valeur affichée provient toujours de la **relecture de l'équipement**. La commande est refusée si le driver est arrêté ou déconnecté, ou si le **mode sûr** est actif. Les commandes en attente pendant une coupure Modbus ne sont pas rejouées à la reconnexion.

Un tag dont le « driver » ne correspond à aucun driver déclaré est une **variable interne** : la valeur y est appliquée directement.

> Sans mode sûr, le planificateur et les flux écrivent sur les équipements **dès le démarrage** du moteur.

## Scripts Rhai (nœud « Script Rhai »)

| Élément | Description |
|---|---|
| `val` | Valeur d'entrée du nœud |
| `tag("ID")` | Valeur numérique d'un tag |
| `tag_bool("ID")` | Valeur booléenne d'un tag |
| `tag_text("ID")` | Valeur texte d'un tag |
| `clamp(x, min, max)` | Bornage |
| `consigne` | Conservé pour compatibilité : consigne de `GTB.CTA01.Consigne_Temp`. Préférez `tag("ID")`. |

Les scripts sont limités en nombre d'opérations, en taille de chaînes et de tableaux, et en profondeur d'appel.
