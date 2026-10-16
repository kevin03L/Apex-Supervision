# Apex Supervision — Guide d'administration

Ce guide s'adresse à la personne chargée d'installer et d'exploiter le moteur. L'installation est décrite dans [INSTALLATION.md](INSTALLATION.md). Les fonctions de supervision (synoptiques, alarmes, flux, comptes) se gèrent dans l'IHM web.

---

## 1. Où se règle quoi ?

| Réglage | Outil | Application |
|---|---|---|
| Réseau, port, emplacements, journalisation, rétention, sessions, mode sûr, simulation | Fichier `apex-scada.toml`, via l'**utilitaire desktop** ou la CLI | Au **redémarrage** du moteur |
| Démarrage automatique du service | Utilitaire (onglet « État du moteur ») ou gestionnaire de services | Immédiate |
| Comptes, mots de passe, rôles | IHM web (Administrateur) | Immédiate |
| Mise en sommeil des alarmes (activation, durée maximale) | IHM web, onglet « Historian & Système » | Immédiate |
| Drivers, variables, synoptiques, flux, plannings | IHM web | Immédiate |

Le fichier de configuration a **un seul rédacteur à la fois**. Le moteur le lit sans jamais le réécrire. L'utilitaire et la CLI l'écrivent sous verrou, en écriture atomique (fichier temporaire, puis renommage), après une sauvegarde horodatée.

---

## 2. Référence de la configuration (format v1)

Emplacement : `C:\ProgramData\Apex Supervision\config\apex-scada.toml` sous Windows, `/etc/apex-supervision/apex-scada.toml` sous Linux, `./apex-scada.toml` en mode portable.

| Paramètre | Défaut | Validation | Rôle |
|---|---|---|---|
| `config_version` | `1` | géré par les migrations | Version du format. Un fichier sans version est lu comme v0 et migré. |
| `instance.name` | `Apex Supervision` | 1 à 64 caractères | Nom affiché dans l'IHM, les journaux et l'utilitaire. |
| `server.bind` | `127.0.0.1` | adresse IP | `127.0.0.1` : ce poste uniquement. `0.0.0.0` : tout le réseau. Avertissement : le serveur parle **HTTP non chiffré**. |
| `server.port` | `3000` | 1 à 65535 | Port de l'IHM web et de l'API. |
| `paths.data_dir` | vide | chemin absolu ou vide | Base SQLite et verrou d'instance. Vide : emplacement standard. |
| `paths.log_dir` | vide | chemin absolu ou vide | Journaux. |
| `paths.web_dir` | vide | chemin absolu ou vide | IHM web. Vide : fichiers installés avec le programme. |
| `database.file` | `apex-scada.db` | sans `..` | Relatif à `data_dir`, ou absolu. |
| `history.retention_days` | `30` | 0 à 3650 | Rétention de l'historique ; `0` = illimitée. |
| `security.session_ttl_hours` | `12` | 1 à 720 | Expiration d'une session inactive. |
| `logging.level` | `info` | `error`, `warn`, `info`, `debug`, `trace` | Niveau des journaux. La variable `RUST_LOG` le remplace si elle est définie. |
| `logging.file_enabled` | `true` | — | Fichiers quotidiens `apex-scada.AAAA-MM-JJ.log`, en plus de la sortie standard. |
| `logging.retention_files` | `14` | 1 à 365 | Nombre de fichiers journaliers conservés. |
| `safety.safe_mode` | `false` | — | **Mode sûr** : aucune commande vers les équipements réels (voir §5). |
| `simulation.enabled` | `false` (installation), `true` (portable) | — | Automate Modbus embarqué, simulateur GTB et protocoles simulés (voir §6). |
| `simulation.modbus_slave_port` | `5502` | ≠ port web | Port de l'automate embarqué, en écoute sur `127.0.0.1` uniquement. |

Le fichier ne contient **aucun secret**. Les comptes sont en base, avec des mots de passe hachés en Argon2id, et le jeton de contrôle local est régénéré à chaque démarrage.

**Compatibilité** : les variables `SCADA_BIND`, `SCADA_PORT`, `SCADA_DB`, `SCADA_MODBUS_SLAVE_PORT`, `SCADA_HISTORY_RETENTION_DAYS`, `SCADA_SESSION_TTL_HOURS` et `SCADA_STATIC_DIR` des versions 1.x sont encore acceptées. Elles **remplacent** les valeurs du fichier, et le moteur le signale dans ses journaux. Préférez le fichier.

---

## 3. Utilitaire desktop (`apex-scada-admin`)

| Onglet | Fonctions |
|---|---|
| **État du moteur** | Affiche l'état réel : en ligne, service démarré mais web injoignable, arrêté. Indique aussi la version, la durée de fonctionnement, le PID, les drivers et les alarmes actives, le mode sûr et la simulation. Boutons Démarrer, Arrêter et Redémarrer (avec confirmation), ouverture de l'IHM web, démarrage automatique. Liste les dernières erreurs et les derniers avertissements. Signale quand **la configuration a changé depuis le démarrage** et qu'un redémarrage est nécessaire. |
| **Configuration** | Édition validée champ par champ : les erreurs bloquent l'enregistrement, les avertissements sont affichés. Vérification de disponibilité du port, liste des paramètres modifiés, annulation, rechargement. Un conflit est détecté si un autre programme a modifié le fichier entre-temps. |
| **Journaux** | Fichiers journaliers et fichier des erreurs de démarrage, avec filtre « erreurs et avertissements », suivi en continu, copie et ouverture du dossier. |
| **Diagnostics** | Vérifie la configuration, le port, le serveur web, la configuration appliquée, l'IHM, la base, les journaux, le service et la présence d'une ancienne version. Le rapport est copiable. |
| **Sauvegardes** | Liste les 20 dernières versions de la configuration, compare chacune à la version actuelle et permet de la restaurer (la version remplacée est elle-même sauvegardée). |
| **À propos** | Versions de l'utilitaire et du moteur, emplacements, historique des messages. |

L'utilitaire ne dépend pas du moteur. Il reste utilisable quand le moteur est arrêté, quand son port est incorrect ou quand la configuration est invalide. Il interroge le moteur uniquement en boucle locale (`/api/health`).

**Droits** : l'utilitaire s'exécute en utilisateur standard, et la configuration est alors en lecture seule sur une installation système.
- Sous Windows, le bouton « Relancer en administrateur » donne les droits nécessaires. Les actions sur le service demandent une confirmation UAC.
- Sous Linux, l'appartenance au groupe `apex-scada` suffit.

Options : `--config <fichier>` pour administrer une instance précise, `--wizard` pour ouvrir l'assistant.

---

## 4. Ligne de commande (`apex-scada`)

| Commande | Rôle |
|---|---|
| `apex-scada [run] [--config <fichier>]` | Moteur au premier plan. Ctrl+C ou SIGTERM déclenchent un arrêt propre. |
| `apex-scada config check` | Valide la configuration et vérifie que le port est libre. |
| `apex-scada config show` | Affiche les emplacements, l'empreinte et la configuration effective. |
| `apex-scada config init [--portable] [--force] [--name …] [--bind …] [--port …] [--simulation] [--safe-mode]` | Crée un fichier. Un fichier existant n'est remplacé qu'avec `--force`, et il est alors sauvegardé. |
| `apex-scada config migrate` | Réécrit le fichier au format courant : valeurs conservées, sauvegarde préalable. |
| `apex-scada migrate-legacy --from <dossier\|fichier.db> [--force]` | Copie la base d'une version 1.x. La source n'est jamais modifiée. |
| `apex-scada service install\|uninstall\|start\|stop\|restart\|status\|enable\|disable` | Service Windows (administrateur). Sous Linux, utilisez `systemctl`. |
| `apex-scada version` | Version, commit et cible de compilation. |

### Codes de sortie

| Code | Signification | Action |
|---|---|---|
| 0 | Succès / arrêt normal | — |
| 1 | Erreur | Consulter les journaux. |
| 2 | Ligne de commande invalide | `apex-scada help`. |
| 3 | Configuration invalide | `apex-scada config check`, ou onglet Configuration. |
| 4 | Une autre instance utilise déjà le répertoire de données | Arrêter l'autre instance (le PID est indiqué). |
| 5 | Adresse ou port d'écoute indisponible | Changer le port, ou arrêter le programme qui l'occupe. |
| 6 | Base de données inaccessible | Vérifier les droits du répertoire des données et l'espace disque. |

Sous Windows, le code de sortie du service est visible dans `apex-scada service status` et dans l'utilitaire (« Dernier arrêt »). L'unité systemd ne redémarre pas en boucle sur les codes 2 à 6.

---

## 5. Mode sûr (mise en service)

Avec `safety.safe_mode = true`, **aucune commande n'est envoyée aux équipements réels** :
- les écritures depuis l'IHM sont refusées, avec un message explicite ;
- le planificateur et les sorties des flux logiques sont bloqués ;
- l'acquisition, les alarmes et l'historique fonctionnent normalement.

Les drivers simulés restent pilotables. Utilisez ce mode pour vérifier une installation, une configuration de drivers ou une mise à jour sans risque physique, puis désactivez-le et redémarrez le moteur.

Sans mode sûr, le planificateur et les flux **écrivent sur les équipements dès le démarrage du moteur**. Tenez-en compte avant tout redémarrage en exploitation.

---

## 6. Simulation et protocoles simulés

`simulation.enabled = true` active :
- un automate Modbus TCP embarqué (groupe froid), sur `127.0.0.1:<modbus_slave_port>` ;
- le simulateur physique GTB (CTA, chaufferie, TGBT) ;
- les données de démonstration, au **premier** démarrage sur une base vide.

Les drivers **OPC-UA, BACnet/IP et Siemens S7 sont simulés** à ce stade : ils produisent des valeurs fictives. Ils ne sont **pas** opérationnels sur de vrais équipements. Seuls **Modbus TCP** et **MQTT** communiquent réellement.

---

## 7. Sécurité

- Changez les mots de passe des comptes d'usine (`admin`, `superviseur`, `visu`) dès la première connexion. Le moteur le rappelle dans ses journaux tant que ce n'est pas fait.
- Le serveur parle **HTTP non chiffré**. Gardez `server.bind = 127.0.0.1` ou limitez l'accès par pare-feu. Pour un accès réseau, utilisez un proxy inverse HTTPS (nginx, Caddy, IIS).
- Le jeton de contrôle local (arrêt propre par l'utilitaire) n'est accepté que depuis `127.0.0.1` et `::1`. Il est protégé par les permissions du système et régénéré à chaque démarrage.
- Aucune interface d'administration n'est exposée sur le réseau en dehors de l'IHM web authentifiée.

---

## 8. Sauvegardes

| Quoi | Comment |
|---|---|
| Configuration (`apex-scada.toml`) | Automatique à chaque modification (20 versions dans `…/config/backups/`). Restauration depuis l'onglet « Sauvegardes ». |
| Projet (drivers, variables, synoptiques, flux, plannings) | IHM web, onglet « Historian & Système » : export et import JSON. |
| Base complète (historique, comptes, audit) | Moteur arrêté : copie du fichier `apex-scada.db`, ainsi que des fichiers `-wal` et `-shm` s'ils existent. |

---

## 9. Dépannage

| Symptôme | Cause probable | Solution |
|---|---|---|
| L'utilitaire indique « Service démarré, mais le serveur web ne répond pas » | Démarrage en cours, erreur de démarrage ou port filtré | Consulter l'onglet Journaux, puis lancer les Diagnostics. |
| Code 5 au démarrage | Port déjà utilisé | Choisir un autre port dans Configuration, avec « Vérifier la disponibilité ». |
| Code 4 au démarrage | Une autre instance tourne sur les mêmes données | Arrêter l'autre instance (son PID figure dans le message). |
| « Lecture seule » dans l'utilitaire | Compte sans droit d'écriture sur la configuration | Windows : « Relancer en administrateur ». Linux : rejoindre le groupe `apex-scada`. |
| L'IHM web affiche une erreur 404 | IHM introuvable | Diagnostics, ligne « IHM web ». Réinstaller, ou renseigner `paths.web_dir`. |
| Les commandes sont refusées (« Mode sûr actif ») | Mode sûr activé | Le désactiver dans Configuration, puis redémarrer le moteur. |
| La fenêtre de l'utilitaire ne s'ouvre pas | Aucun rendu graphique disponible | Lire le fichier `apex-scada-admin-erreur.txt` dans le dossier temporaire. Administrer avec la CLI (`config show`, `config check`, `service status`). |
