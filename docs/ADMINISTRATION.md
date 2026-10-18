# Apex Supervision — Guide d'administration

Ce guide s'adresse à la personne chargée d'installer et d'exploiter le moteur. L'installation est décrite dans [INSTALLATION.md](INSTALLATION.md). Les fonctions de supervision (synoptiques, alarmes, flux, comptes) se gèrent dans l'IHM web.

---

## 1. Où se règle quoi ?

| Réglage | Outil | Application |
|---|---|---|
| Réseau, port, emplacements, journalisation, rétention, sessions, mode sûr, simulation | Fichier `apex-scada.toml`, via l'**utilitaire desktop** ou la CLI | Au **redémarrage** du moteur |
| Démarrage automatique du service | Utilitaire (onglet « État du moteur ») ou gestionnaire de services | Immédiate |
| Comptes, mots de passe, rôles | IHM web (Administrateur) | Immédiate |
| Mise en sommeil des alarmes (activation, durée maximale) | IHM web, onglet « Système » | Immédiate |
| Drivers, variables, synoptiques, flux, plannings | IHM web | Immédiate |

Les états animés des symboles de synoptique se règlent dans le Studio avec la variable liée, la variable de défaut facultative et, pour un niveau, les bornes minimum et maximum (voir [`packs/README.md`](../packs/README.md)).

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
| `simulation.enabled` | `false` (installation), `true` (portable) | — | Automate Modbus embarqué, simulateur GTB et données de démonstration (voir §6). |
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

L'utilitaire ne dépend pas du moteur. Il reste utilisable quand le moteur est arrêté, quand son port est incorrect ou quand la configuration est invalide. Il interroge le moteur sur son adresse d'écoute (`/api/health`), en boucle locale quand l'écoute est `127.0.0.1` ou `0.0.0.0`. Après une modification de l'adresse ou du port, il continue de suivre le moteur en marche sur son ancienne adresse jusqu'à son redémarrage.

> Avec une **adresse d'écoute précise** (ex. `192.168.1.10`), le moteur n'est pas joignable en boucle locale : l'utilitaire voit son état, mais pas ses détails d'exécution. Il ne peut alors ni détecter qu'un redémarrage est nécessaire, ni arrêter un moteur lancé manuellement (le jeton local n'est accepté qu'en boucle locale). Le service, lui, reste pilotable.

**Droits** : l'utilitaire s'exécute en utilisateur standard, et la configuration est alors en lecture seule sur une installation système.
- Sous Windows, le bouton « Relancer en administrateur » donne les droits nécessaires. Les actions sur le service demandent une confirmation UAC.
- Sous Linux, l'appartenance au groupe `apex-scada` suffit pour modifier la configuration et pour démarrer, arrêter ou redémarrer le service (règle polkit, ignorée par Ubuntu 22.04). Activer ou désactiver le démarrage automatique demande le mot de passe administrateur.

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
| `apex-scada migrate-legacy --from <dossier\|fichier.db> [--force]` | Copie la base d'une version 1.x (journal WAL compris). La source n'est jamais modifiée. Une base déjà en place n'est remplacée qu'avec `--force` : elle est d'abord sauvegardée en entier sous `<base>.bak-<date>`, et la commande refuse si cette sauvegarde est impossible. En mode portable, ajoutez `--config <fichier>`. |
| `apex-scada service install\|uninstall\|start\|stop\|restart\|status\|enable\|disable\|permissions` | Service Windows (administrateur). `permissions` réapplique le propriétaire et les droits de `C:\ProgramData\Apex Supervision`. Sous Linux, utilisez `systemctl`. |
| `apex-scada version` | Version, commit et cible de compilation. |

### Codes de sortie

| Code | Signification | Action |
|---|---|---|
| 0 | Succès / arrêt normal | — |
| 1 | Erreur | Consulter les journaux. Pour `migrate-legacy` : droits insuffisants sur le répertoire des données. |
| 2 | Ligne de commande invalide | `apex-scada help`. |
| 3 | Configuration invalide, ou logiciel non installé (emplacements système absents) | `apex-scada config check`, ou onglet Configuration ; sinon installer le logiciel ou utiliser `--config`. |
| 4 | Une autre instance utilise déjà le répertoire de données | Arrêter l'autre instance (le PID est indiqué). |
| 5 | Adresse ou port d'écoute indisponible | Changer le port, ou arrêter le programme qui l'occupe. |
| 6 | Base de données inaccessible | Vérifier les droits du répertoire des données et l'espace disque. |

Sous Windows, le code de sortie du service est visible dans `apex-scada service status` et dans l'utilitaire (« Dernier arrêt »). L'unité systemd ne redémarre pas en boucle sur les codes 2 à 6.

---

## 5. Mode sûr (mise en service)

Avec `safety.safe_mode = true`, **aucune commande n'est envoyée aux équipements réels** :
- les écritures depuis l'IHM sont refusées, avec un message explicite (HTTP 403) ;
- le planificateur et les sorties des flux logiques sont bloqués ;
- l'acquisition, les alarmes et l'historique fonctionnent normalement.

Le mode sûr bloque toute écriture vers l'ensemble des **cinq protocoles industriels réels** (Modbus TCP, MQTT, Siemens S7, OPC-UA et BACnet/IP). Seul le simulateur GTB (driver simulé) reste pilotable.

L'IHM signale l'effet du mode sûr : un planning actif dont la variable cible appartient à un équipement réel est affiché **SUSPENDU (mode sûr)** en orange, et un flux dont une sortie vise un équipement réel est marqué 🟠 suspendu, avec la liste des sorties bloquées.

Utilisez ce mode pour vérifier une installation, une configuration de drivers ou une mise à jour sans risque physique, puis désactivez-le et redémarrez le moteur.

Sans mode sûr, le planificateur et les flux **écrivent sur les équipements dès le démarrage du moteur**. Tenez-en compte avant tout redémarrage en exploitation.

---

## 6. Simulation et protocoles

Apex Supervision intègre cinq protocoles industriels réels dialoguant avec de vrais équipements matériels ou logiciels, ainsi qu'un environnement de simulation pour les tests et la formation.

### Règles communes aux protocoles réels

- **Détection des coupures et reconnexion** : si un équipement ne répond plus, la liaison passe à l'état déconnecté, la qualité de ses variables est dégradée en `BadDisconnect` et le moteur tente automatiquement la reconnexion à chaque cycle.
- **Isolation des erreurs d'adressage** : une adresse invalide ou refusée par l'équipement ne dégrade que sa variable propre ; la liaison du driver et les autres variables restent actives.
- **Contrôle avant émission** : toute consigne hors de la plage admissible de son type (dépassement de capacité, type incompatible) est rejetée immédiatement (HTTP 400) avant tout envoi sur le réseau.
- **Bornes de consigne** : les champs facultatifs « Consigne minimale » et « Consigne maximale » du catalogue sont vérifiés par le serveur pour toutes les commandes, y compris les flux, plannings et impulsions. Une borne doit être finie et le minimum ne peut pas dépasser le maximum.
- **Qualité et commandes** : une variable d'équipement en `BadDisconnect` refuse les nouvelles commandes. La dérogation « Écriture sans retour » est réservée aux équipements dont la commande reste nécessaire sans lecture fiable. `Good` et `Uncertain` autorisent l'envoi si le driver est connecté.
- **Retour des commandes** : la réponse immédiate confirme la transmission au driver, pas l'application par l'équipement. Un refus ultérieur est journalisé et signalé aux IHM connectées.
- **Fraîcheur des valeurs** : le champ facultatif « Fraîcheur maximale (secondes) » dégrade une valeur `Good` en `Uncertain` si aucune mise à jour valide n'arrive dans le délai. La prochaine publication valide restaure `Good`. Cette règle vaut pour tous les protocoles, en particulier MQTT. Sans délai configuré, le comportement historique est conservé.
- **Mode sûr** : bloque toute écriture vers ces cinq protocoles.

---

### 6.1 Modbus TCP

Communication maître Modbus TCP standard, avec lecture optimisée par blocs de registres contigus.

- **Endpoint** : `IP:port` (port par défaut : 502, ex. `192.168.1.10:502` ou `192.168.1.10`).
- **Adresses supportées** :
  - Bobines (Coils) : `COIL:n` (inscriptibles).
  - Entrées discrètes : `DI:n` (lecture seule).
  - Registres d'entrée : `IR:n` ou `IR:n:FLOAT32` (lecture seule).
  - Registres de maintien : `HR:n` ou `HR:n:INT16`, `HR:n:UINT16`, `HR:n:INT32`, `HR:n:FLOAT32`, `HR:n:FLOAT32_SWAP` (inscriptibles).
- **Fonctionnement et limites** :
  - Délai maximal de réponse de 3 secondes par requête.
  - Liaison déclarée établie dès la première réponse reçue.
  - Pas d'auto-découverte.

---

### 6.2 MQTT

Souscription et publication MQTT pour les équipements IoT et les passerelles de télémétrie.

- **Endpoint** : `mqtt://hôte:port` (port par défaut : 1883, ex. `mqtt://192.168.1.50:1883`).
- **Adresses (sujets)** :
  - Sujet complet : `sujet` (ex. `atelier/temperature`).
  - Clé JSON : `sujet#clé` (ex. `telemetrie/groupe1#puissance`).
  - En lecture, les jokers `+` (un niveau) et `#` (multi-niveaux) sont supportés.
- **Fonctionnement et limites** :
  - Publications de consigne émises avec une qualité de service QoS 1.
  - Identifiant client MQTT de la forme `apex-scada-<id>`.
  - Pas encore d'authentification (utilisateur/mot de passe) ni de TLS.
  - Pas d'auto-découverte.

---

### 6.3 Siemens S7 (S7-300, S7-400, S7-1200, S7-1500)

Communication native ISO-on-TCP avec les automates programmables Siemens.

- **Endpoint** : `IP[:port][?rack=R&slot=S]` (port 102 par défaut, rack 0, slot 1 pour S7-1200/1500 ; pour S7-300, spécifier `slot=2`, ex. `192.168.1.20:102?rack=0&slot=2`).
- **Adresses supportées** :
  - Blocs de données (DB) : `DB10.DBD0:REAL`, `DB10.DBX4.0`, `DB10.DBW2:INT`.
  - Mémentos : `MW10`, `MD20`, `M0.1`.
  - Entrées physiques : `I0.0` ou notation Siemens `E0.0`, `IW64` (lecture seule).
  - Sorties physiques : `Q0.0` ou notation Siemens `A0.0` (inscriptibles).
  - Types de données : `BOOL`, `BYTE`, `INT`, `WORD`, `DINT`, `DWORD`, `REAL`.
- **Fonctionnement et limites** :
  - S7-1200/1500 : autoriser l'accès PUT/GET dans les propriétés de la CPU sous TIA Portal, et configurer les blocs de données (DB) en accès standard (non optimisé).
  - Les entrées physiques ne sont pas inscriptibles.
  - Pas d'auto-découverte.

---

### 6.4 OPC-UA

Client OPC-UA binaire pour serveurs d'automatismes et passerelles SCADA.

- **Endpoint** : `opc.tcp://hôte:4840[/chemin]` (ex. `opc.tcp://192.168.1.30:4840`).
- **Adresses (NodeId)** :
  - Identifiant chaîne : `ns=2;s=Nom`.
  - Identifiant numérique : `ns=3;i=1002`.
- **Fonctionnement et limites** :
  - Connexion sans chiffrement et session anonyme uniquement pour l'instant.
  - L'écriture d'une consigne est automatiquement convertie dans le type du nœud (appris lors de la lecture).
  - Certificats de la bibliothèque rangés dans `<données>/opcua-pki`.
  - Pas d'auto-découverte.

---

### 6.5 BACnet/IP

Protocole normalisé pour la gestion technique du bâtiment (GTB).

- **Endpoint** : `IP[:port]` (port standard UDP : 47808, ex. `192.168.1.40:47808` ou `192.168.1.40`).
- **Adresses supportées** :
  - Format : `BACNET:type:instance[:propriété][:priorité]` (ex. `BACNET:AV:201` ou `BACNET:AV:201:Present_Value:8`).
  - Types d'objets : `AI`, `AO`, `AV`, `BI`, `BO`, `BV`, `MSI`, `MSO`, `MSV`.
  - Propriété par défaut : `Present_Value`.
  - Priorité d'écriture configurable de 1 à 16 (8 par défaut).
- **Auto-Découverte réelle** :
  - BACnet/IP bénéficie d'une **auto-découverte réelle** : le moteur interroge l'automate BACnet pour dresser la liste des objets présents, leurs noms et leurs unités, et en permettre l'ajout direct.
- **Fonctionnement et limites** :
  - Équipement joint directement par son IP (pas de routage MS/TP).
  - Les entrées physiques (`AI`, `BI`, `MSI`) sont non inscriptibles.

---

### 6.6 Simulation GTB et automate embarqué

Le réglage `simulation.enabled = true` active l'environnement de simulation :
- un automate Modbus TCP embarqué (groupe froid), sur `127.0.0.1:<modbus_slave_port>` ;
- le simulateur physique GTB (`Simulator` : CTA, chaufferie, TGBT) ;
- les données de démonstration et les trois comptes d'usine (`admin`, `superviseur`, `visu`), au **premier** démarrage sur une base vide. Quand la simulation est désactivée (production), seul le compte `admin` est créé.

Le seul driver de démonstration restant est le **simulateur GTB** (`Simulator`, `ProtocolType::is_simulated()`). Quand la simulation est désactivée, seul le simulateur GTB est refusé à l'ajout et au démarrage (erreur HTTP 400). À l'import d'un projet, il est écarté avec ses variables (`ignored_drivers`, `ignored_tags`). Un simulateur déjà enregistré reste modifiable mais ne démarre pas.

Les cinq protocoles industriels réels (Modbus TCP, MQTT, Siemens S7, OPC-UA, BACnet/IP) fonctionnent de manière indépendante et restent disponibles que la simulation soit activée ou non.

---

## 6 bis. Drivers et automatismes : règles de contrôle

- **Commandes depuis les synoptiques** : elles se font en deux temps. Un clic sur une commande (marche/arrêt, ouverture, curseur, consigne) la **sélectionne** : l'équipement s'encadre en orange et un bandeau indique la variable et la valeur, avec « Exécuter » et « Annuler ». Rien n'est envoyé tant que l'opérateur ne clique pas sur « Exécuter », dans les 10 secondes. Passé ce délai, sur « Annuler », sur Échap ou en changeant de synoptique ou d'onglet, la sélection est abandonnée. Un bouton impulsionnel s'arme au premier appui, puis envoie une seule impulsion (1 puis 0) tant qu'il est maintenu. L'écriture directe depuis l'écran Équipements et les boutons « Forcer » du planning restent immédiats.
- **Profil Visualisation** : il consulte la liste et le journal des alarmes et peut **acquitter une alarme à la fois**, après une fenêtre de confirmation. L'acquittement est tracé à son nom. L'acquittement global, la mise en sommeil et l'effacement des alarmes résolues restent réservés aux profils Superviseur et Administrateur. Il lit aussi les derniers points d'historique des variables affichées par une courbe de tendance, pour la pré-remplir.
- **Courbes de tendance** : au chargement de la page, elles sont pré-remplies avec les 55 derniers points de l'historique. Une variable non historisée démarre vide, avec la mention « non historisée ».

- **Modbus TCP** : une variable inscriptible doit viser une bobine (`CO`) ou un registre de maintien (`HR`). `DI`, `IR` et les types texte sont refusés à l'enregistrement. `HR:x:INT32` est inscriptible. Une valeur hors de la plage du type (INT16, UINT16, INT32, FLOAT32) est refusée (400) au lieu d'être saturée.
- **Écriture refusée par l'équipement** (exception Modbus) : la liaison reste saine et aucune alarme de communication n'est levée. Le refus est tracé dans le journal d'audit (action `WRITE_REJECTED`, acteur = identifiant du driver). Il reste affiché en orange sur la carte du driver jusqu'à la prochaine écriture acceptée.
- **Alarmes** : la bande morte d'une alarme retarde seulement la disparition du niveau actif. Elle n'empêche jamais l'escalade vers HH ou LL. Chaque alarme porte son niveau (`level` : `Tor`, `HighHigh`, `High`, `LowLow` ou `Low`) dans `/api/alarms` et dans le WebSocket.
- **Flux** : un script Rhai vide ou invalide est refusé à l'enregistrement (400). Une erreur d'exécution n'est journalisée qu'une fois par nœud, jusqu'au rétablissement.
- **Nœud « Écrire variable »** : il ne réécrit sa sortie que si elle varie d'au moins max(bande morte du tag, 0,08) depuis sa dernière écriture. Une valeur modifiée par un opérateur n'est donc pas écrasée à chaque cycle. La sortie est réécrite dans trois cas :
  - un redéploiement du flux ;
  - un refus de l'équipement ;
  - un changement d'état de la liaison du driver (commande perdue pendant une coupure).

---

## 7. Sécurité

- **Comptes au premier démarrage** : en production (simulation désactivée), seul le compte administrateur `admin` est créé avec son mot de passe initial `admin123` ; les comptes `superviseur` et `visu` ne sont pas créés. En démonstration (simulation active), les trois comptes d'usine (`admin`, `superviseur`, `visu`) sont initialisés. Une base déjà initialisée n'est jamais modifiée au redémarrage.
- **Obligation de changer le mot de passe administrateur** : changez impérativement le mot de passe d'`admin` dès la première connexion (et ceux des autres comptes d'usine s'ils ont été créés). Le moteur émet un avertissement de sécurité dans ses journaux à chaque démarrage tant qu'un mot de passe d'usine reste en place.
- **Connexion** : après 5 tentatives sans succès, l'identifiant est verrouillé 60 secondes, y compris face à des tentatives simultanées. Un identifiant inconnu est traité comme un mot de passe faux, avec le même message et le même temps de réponse. Le moteur fait au plus 4 vérifications de mot de passe à la fois. Une requête qui attend plus de 5 secondes une place reçoit 503 (« serveur occupé ») : la tentative n'est pas comptée. Le changement de son propre mot de passe est soumis au même verrouillage, avec un compteur distinct.
- **Sessions conservées** : les sessions ouvertes survivent à un redémarrage du moteur, y compris après un arrêt brutal. Elles expirent après `security.session_ttl_hours` heures d'inactivité. La base (table `user_sessions`) ne contient que l'empreinte SHA-256 de chaque jeton, jamais le jeton lui-même : une copie de la base ne permet pas de reprendre une session. Déconnexions et fermetures de sessions sont écrites en base avant la réponse, et les dates d'activité au plus une minute après.
- **Sessions** : changer le rôle d'un compte, le désactiver ou lui donner un nouveau mot de passe depuis l'écran Utilisateurs ferme toutes ses sessions ouvertes. Un administrateur qui change son propre mot de passe garde la session d'où il agit. Une connexion en cours pendant la modification ne peut ni restaurer l'ancien mot de passe ni conserver une session.
- Le serveur parle **HTTP non chiffré**. Gardez `server.bind = 127.0.0.1` ou limitez l'accès par pare-feu. Pour un accès réseau, utilisez un proxy inverse HTTPS (nginx, Caddy, IIS).
- **Jeton de contrôle local** (`run/control.token`, régénéré à chaque démarrage et protégé par les permissions du système). Il sert uniquement à l'utilitaire desktop et n'ouvre que deux routes : `POST /api/system/shutdown` (arrêt propre) et `GET /api/system/status`. Toute autre route répond 403 avec ce jeton : pas d'administration, pas de consigne, pas de WebSocket. Les actions faites avec ce jeton sont journalisées sous l'acteur interne **« Utilitaire local »**. Cet acteur ne correspond à aucun compte : l'arrêt propre fonctionne même si le compte `admin` est désactivé.
- **Détails d'exécution de `/api/health`** (chemins de configuration, de données et de journaux, PID, mode, empreinte de la configuration) : publiés seulement à une requête qui présente le jeton de contrôle local, en plus des deux conditions ci-dessous. Une page web ou un proxy local mal configuré ne peuvent donc pas les lire. L'utilitaire lit ce jeton : sous Windows, lancé sans élévation, il n'affiche pas ces détails et le signale.
- **Ce qui est réservé au poste local.** Le jeton de contrôle local n'est accepté, et les détails d'exécution ne sont publiés, qu'à une requête qui remplit deux conditions :
  - elle vient de la boucle locale (`127.0.0.1` ou `::1`) ;
  - elle ne porte **aucun** en-tête de proxy inverse : `Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto` ou `X-Real-IP`.

  Derrière un proxy inverse installé **sur le même poste**, toutes les requêtes arrivent de `127.0.0.1` : l'adresse seule ne permet pas de savoir qu'elles viennent du réseau. **Configurez donc le proxy pour qu'il ajoute `X-Forwarded-For`** (ou `Forwarded`) à chaque requête. Caddy le fait par défaut. Avec nginx, ajoutez `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`. Avec IIS/ARR, activez l'en-tête `X-Forwarded-For`. Un proxy qui n'ajoute aucun de ces en-têtes rend ce contrôle inopérant.
- Aucune interface d'administration n'est exposée sur le réseau en dehors de l'IHM web authentifiée.
- **Modules complémentaires de symboles** : téléchargez l'archive `apex-symboles-<version>.zip` et son empreinte `.sha256` depuis les Releases, puis installez les fichiers JSON dans la fenêtre Bibliothèque. L'installation et la désinstallation d'un module, y compris les trois packs historiques, sont réservées à l'Administrateur. L'import libre de symboles reste accessible au Superviseur ; les imports JSON sont regroupés dans un module `import-<slug>`. La liste indique la version et le nombre de symboles installés. Si un synoptique utilise un symbole, la désinstallation répond 409 et demande une action forcée explicite. La réinstallation retire les symboles absents du nouveau fichier, sauf ceux utilisés par un synoptique, signalés dans `conserves`. Les modules ne font partie ni du binaire ni des installateurs. Voir [le format des modules](../packs/README.md).
- **Symboles SVG** (import de bibliothèque, symboles des synoptiques) : le serveur analyse le SVG comme du XML, puis le reconstruit à partir d'une liste blanche d'éléments et d'attributs de dessin. Il retire les scripts, les styles globaux, `foreignObject`, les gestionnaires `on*`, les liens externes (seules les références internes `#id` et les images PNG, JPEG, GIF ou WebP embarquées sont gardées), les commentaires, les instructions de traitement, les sections CDATA et le DOCTYPE. Un fichier qui n'est pas du XML bien formé est refusé avec un message explicite. Un symbole déjà enregistré qui ne passe plus ce contrôle est ignoré au chargement, avec un avertissement dans le journal. Les règles de classe simples d'une feuille `<style>` (`.st0 { fill: … }`, exports Illustrator) sont reportées dans l'attribut `style` des éléments concernés. Les autres sélecteurs sont ignorés. L'IHM refait le même filtrage et insère le dessin par le DOM, jamais sous forme de HTML.

---

## 8. Sauvegardes

| Quoi | Comment |
|---|---|
| Configuration (`apex-scada.toml`) | Automatique à chaque modification (20 versions dans `…/config/backups/`). Restauration depuis l'onglet « Sauvegardes ». |
| Projet (drivers, variables, synoptiques, flux, plannings) | IHM web, onglet « Système » : export et import JSON (fichier `apex_supervision_projet_<date>.json`, 64 Mo au plus à l'import). |
| Base complète (historique, comptes, audit) | Moteur arrêté : copie du fichier `apex-scada.db`, ainsi que des fichiers `-wal` et `-shm` s'ils existent (après un arrêt propre, le journal est reporté dans `apex-scada.db`, qui se suffit alors à lui-même). |

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
