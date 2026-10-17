# RustScadaFlow — Audit complet et plan de correction

- **Date de l'audit** : 16/10/2026
- **Version auditée** : `rust-scada-flow 0.1.0` (affichée « 1.0.0-PROD » dans l'interface)
- **Périmètre** : `src/` (Rust, ~5 700 lignes), `static/` (HTML/JS/CSS, ~3 900 lignes), scripts `.bat`, base `rust_scada_flow.db`
- **Méthode** : lecture intégrale du code, vérification de la compilation (`cargo check`), analyse des flux de données, des droits d'accès, de la persistance et des chemins critiques en performance.
- **Sauvegarde avant modification** : `D:\Vs-code\Apex-Supervision_backup_2026-10-16`

---

## 1. Synthèse

L'application est fonctionnelle et bien structurée : noyau temps réel (`ScadaCore`) sur `DashMap` + canaux Tokio, drivers isolés dans des tâches, moteur de flux, alarmes ISA-18.2, historique SQLite et IHM complète. La base est saine pour évoluer.

En revanche, trois familles de défauts empêchent un usage en production :

1. **Sécurité** : le contrôle d'accès (RBAC) est contournable par n'importe quelle requête sans jeton (le serveur retombe sur le compte `admin`). Les mots de passe sont en clair et l'IHM est vulnérable au XSS stocké, notamment via les SVG importés.
2. **Persistance** : les drivers ajoutés depuis l'IHM sont perdus au redémarrage, et les données de démonstration réapparaissent après avoir été supprimées.
3. **Performance** : l'historique écrit une ligne par changement de valeur sans regroupement ni rétention, et l'IHM redessine des tableaux entiers à chaque changement de valeur.

| Sévérité | Sécurité | Stabilité | Performance | Fonctionnel | Total |
|---|---|---|---|---|---|
| Critique | 2 | – | – | – | **2** |
| Élevée | 4 | 4 | 2 | – | **10** |
| Moyenne | 5 | 9 | 4 | – | **18** |
| Faible | 3 | 9 | 2 | 9 | **23** |

*Les 6 constats marqués « (découvert en cours de réalisation) » ont été trouvés pendant les corrections et ajoutés ensuite : SEC-14 et STB-18 à STB-22.*

---

## 2. Constats détaillés

Les numéros de ligne renvoient à la version auditée (sauvegarde).

### 2.1 Sécurité (SEC)

| ID | Sévérité | Emplacement | Constat | Impact |
|---|---|---|---|---|
| SEC-01 | **Critique** | `core.rs:2018-2049` | `resolve_user` renvoie le compte **admin** si le jeton est absent ou inconnu. Le nom d'utilisateur brut est aussi accepté comme jeton (`X-Scada-Token: admin`). | N'importe quel poste du réseau peut éteindre le serveur, modifier les consignes ou créer des comptes. Le RBAC est purement décoratif. |
| SEC-02 | **Critique** | `core.rs:1940`, `app.js:8,34` | Session admin permanente codée en dur : `scada-default-admin-session`. | Accès admin connu publiquement, même si SEC-01 est corrigé. |
| SEC-03 | Élevée | `core.rs:1908,1970,2102` | Le champ `password_hash` contient le mot de passe **en clair**, comparé directement. Mot de passe par défaut `scada123` pour les nouveaux comptes. | La lecture de la base (fichier ou sauvegarde) révèle tous les mots de passe. |
| SEC-04 | Élevée | `core.rs:1980` | Jetons prévisibles (`scada-tok-<user>-<millisecondes>`) sans expiration. | Usurpation de session par devinette. Les sessions ne se ferment jamais. |
| SEC-05 | Élevée | `main.rs` (routes GET) | Aucune authentification sur les lectures : `/api/system/export` (projet complet), `/api/audit`, `/api/history`, `/api/tags`, `/ws/realtime`… | Fuite complète de la configuration et des données process. |
| SEC-06 | Élevée | `app.js` (≈80 `innerHTML`), `app.js:984-990`, `core.rs:2257` | Les données (libellés, messages, noms, erreurs drivers, valeurs texte MQTT) sont injectées sans échappement. Les SVG importés sont stockés et rendus tels quels (`<script>`, `onload=`…). | XSS stocké : un symbole piégé exécute du code dans le navigateur de l'admin, avec son jeton. |
| SEC-07 | Moyenne | `app.js` (`onclick="f('${id}')"`), `core.rs` | Les identifiants (tags, drivers, flux…) ne sont pas validés côté serveur et sont injectés dans des attributs `onclick`. | Injection JavaScript via un identifiant contenant `'`. |
| SEC-08 | Moyenne | `main.rs:120-123,193` | Écoute sur `0.0.0.0` avec un CORS totalement ouvert (`Any`), sans configuration possible. | Toute page web visitée peut appeler l'API depuis le navigateur d'un opérateur. |
| SEC-09 | Moyenne | `index.html:32-55` | Les mots de passe par défaut sont affichés et pré-remplis dans la fenêtre de connexion. | Divulgation des identifiants à tout visiteur. |
| SEC-10 | Moyenne | `main.rs:747`, `core.rs` (≈20 appels `record_audit("Opérateur", …)`) | Le nom d'opérateur d'un acquittement est fourni librement par le client. L'acteur des actions métier est codé en dur. | La traçabilité réglementaire est falsifiable ou inexploitable. |
| SEC-11 | Moyenne | `core.rs:1955` | Aucune limite de tentatives de connexion. | Attaque par force brute sur les mots de passe. |
| SEC-12 | Faible | `Arreter-SCADA.bat`, `Redemarrer-SCADA.bat` | Les scripts appellent l'API sans jeton : ils ne fonctionnent qu'à cause de SEC-01. | Ils cesseront de fonctionner une fois SEC-01 corrigé, sauf adaptation. |
| SEC-13 | Faible | `flow_engine.rs:93` | Le moteur Rhai n'est limité qu'en nombre d'opérations (pas de limite de taille de chaînes, de tableaux, de profondeur). | Un script mal écrit peut saturer la mémoire. |
| SEC-14 | Faible | import SVG | *(découvert en cours de réalisation)* Un élément `<style>` d'un SVG inséré dans la page s'applique à **toute** l'IHM. | Défiguration ou masquage de l'interface par un symbole importé. |

### 2.2 Persistance et stabilité (STB)

| ID | Sévérité | Emplacement | Constat | Impact |
|---|---|---|---|---|
| STB-01 | Élevée | `core.rs:1370-1454`, `main.rs:99-114` | `drivers_config` est écrit mais **jamais relu** au démarrage. Seuls les deux drivers codés en dur sont relancés. | Les drivers ajoutés via l'IHM disparaissent au redémarrage ; leurs tags deviennent orphelins. |
| STB-02 | Élevée | `core.rs:1464-1949` | Les données de démo (synoptiques, flux, plannings, comptes, symboles) sont recréées en mémoire à chaque démarrage. | Une suppression ne tient pas : les éléments supprimés réapparaissent. |
| STB-03 | Moyenne | `core.rs:539`, `core.rs:505` | La suppression d'un tag ne le retire pas de la configuration du driver. La modification d'un tag n'est pas répercutée dans `drivers_config`. | Une fois STB-01 corrigé, les tags supprimés reviendraient. |
| STB-04 | Moyenne | `core.rs:1344` | `import_project_bundle` ignore la liste des drivers et les erreurs d'import. | La restauration d'une sauvegarde est incomplète. |
| STB-05 | Moyenne | `main.rs:82,224` | Base de secours `sqlite::memory:` avec un pool de 5 connexions : chaque connexion ouvre une base vide distincte. | En mode dégradé, les tables n'existent que sur une connexion : erreurs aléatoires. |
| STB-06 | Moyenne | `core.rs:1033-1036` | Le planificateur marque une règle comme appliquée même si l'écriture a échoué. | Une consigne horaire peut ne jamais être appliquée (driver arrêté au mauvais moment). |
| STB-07 | Moyenne | `core.rs:388-406` | `restart_all_systems` redémarre **tous** les drivers, y compris ceux arrêtés volontairement. | Remise en service non souhaitée d'un équipement consigné. |
| STB-08 | Moyenne | `drivers/mod.rs:520-551` | Le simulateur réinitialise ses valeurs internes (consignes, états pompes) à chaque redémarrage. | Les consignes opérateur sont perdues après une redémarrage du driver. |
| STB-09 | Moyenne | `main.rs:910` | Si un client WebSocket prend du retard (`Lagged`), les mises à jour perdues sont ignorées en silence. | L'IHM affiche des valeurs fausses jusqu'au rechargement de la page. |
| STB-10 | Faible | `modbus_slave.rs:171-214` | Esclave Modbus : la quantité demandée n'est pas bornée (débordement du `u8` au-delà de 127 registres) et les exceptions 0x02/0x03 ne sont pas renvoyées. | Trames invalides pour un maître tiers. |
| STB-11 | Moyenne | `drivers/mod.rs:878` | Les commandes d'écriture mises en file pendant une coupure Modbus sont rejouées à la reconnexion. | Une commande obsolète peut être appliquée plusieurs minutes plus tard (risque process). |
| STB-12 | Faible | `core.rs` (≈25 `let _ = sqlx::query…`) | Les erreurs SQLite sont ignorées sans journalisation. | Pertes de données invisibles. |
| STB-13 | Faible | `core.rs:408` | Les statistiques des drivers (trames, latence, erreurs) ne sont jamais diffusées en temps réel. | L'écran « Drivers » reste figé tant qu'on ne clique pas sur Actualiser. |
| STB-14 | Faible | `app.js:970` | Bouton impulsion : si la souris est relâchée hors du bouton, la commande reste à 1. | Commande maintenue involontairement. |
| STB-15 | Faible | `app.js` (widgets) | Le profil Visualisation voit les boutons de commande et reçoit une alerte 403 au clic. La classe CSS `btn-success` est utilisée mais pas définie. | Ergonomie. |
| STB-16 | Faible | `models.rs`, `core.rs:182,1331` | Version codée en dur (« 1.0.0-PROD ») et différente de `Cargo.toml` (0.1.0). | Confusion lors du support. |
| STB-17 | Faible | `drivers/mod.rs:1167`, `flow_engine.rs:66` | Le WebSocket ne gère pas `Closed` sur les canaux alarme/driver/trace, et le moteur de flux s'arrête définitivement si son canal se ferme. | Robustesse. |
| STB-18 | Élevée | `core.rs:920-960` | *(découvert en cours de réalisation)* Une consigne écrite sur un tag mis à l'échelle est **remise à l'échelle une seconde fois** (21,5 devient 2,15 avec un facteur 0,1), et la valeur envoyée à l'équipement n'est pas convertie en valeur brute. | Une mauvaise consigne est appliquée au process dès qu'un facteur d'échelle est utilisé. |
| STB-19 | Élevée | `core.rs:937-949` | *(découvert en cours de réalisation)* `write_tag` garde un verrou `DashMap` pendant un `send().await` sur un canal borné. | Driver déconnecté et file pleine : blocage de l'API, du planificateur et du moteur de flux. |
| STB-20 | Faible | `styles.css:592` | *(découvert en cours de réalisation)* Les symboles importés « à rotation » utilisent l'animation `spin`, qui n'existe pas (seule `spinAnim` est définie). | La rotation des agitateurs et turbines importés n'a jamais fonctionné. |
| STB-21 | Faible | `core.rs:106-110` | *(découvert en cours de réalisation)* Le planificateur et le moteur de flux démarrent avant les drivers, et un échec d'écriture répété est journalisé toutes les 1,2 s. | Échecs parasites au démarrage et journaux saturés pendant une coupure. |
| STB-22 | Moyenne | `core.rs:951-959` | *(découvert en cours de réalisation)* Écriture « optimiste » : la valeur affichée passe en qualité `Good` même si l'équipement n'a rien reçu (driver arrêté ou déconnecté). | L'opérateur croit une consigne appliquée alors qu'elle ne l'est pas. |

### 2.3 Performance (PRF)

| ID | Sévérité | Emplacement | Constat | Impact |
|---|---|---|---|---|
| PRF-01 | Élevée | `core.rs:596-615` | Historique : une tâche Tokio et un `INSERT` auto-validé **par changement de valeur**, sans transaction. | ~30 écritures disque par seconde avec la démo. Ce volume grandit linéairement avec le nombre de tags et limite la tenue en charge. |
| PRF-02 | Élevée | `tag_history` | Aucune rétention : croissance illimitée (17 Mo déjà). | Saturation du disque et requêtes de plus en plus lentes. |
| PRF-03 | Moyenne | `main.rs:223` | SQLite sans mode WAL, `synchronous=NORMAL` ni `busy_timeout`. | Verrous `database is locked` sous charge et écritures lentes. |
| PRF-04 | Moyenne | `flow_engine.rs:519`, `flow_engine.rs:130` | Les scripts Rhai sont recompilés à chaque exécution (≈ toutes les 1,2 s et à chaque COV). « Exécuter maintenant » reconstruit un moteur complet. | Consommation CPU inutile. |
| PRF-05 | Moyenne | `app.js:268-285` | Chaque `TagUpdate` déclenche un rendu DOM immédiat. Sur l'onglet Tags, **tout le tableau** est reconstruit à chaque message. | IHM saccadée et CPU navigateur élevé. |
| PRF-06 | Moyenne | `app.js:1505-1511` | Le déplacement d'un nœud de flux redessine tout l'éditeur à chaque mouvement de souris. | Déplacement saccadé avec beaucoup de nœuds. |
| PRF-07 | Faible | `drivers/mod.rs:901` | Modbus : une requête par tag, sans regroupement des registres contigus. | Latence de scrutation proportionnelle au nombre de tags. |
| PRF-08 | Faible | `tag_history` | Pas d'index temporel : le filtrage par date et la purge par âge sont impossibles à faire efficacement. | Bloque la rétention (PRF-02) et le filtrage par période (FCT-03). |

### 2.4 Fonctionnel (FCT)

| ID | Constat |
|---|---|
| FCT-01 | Planificateur « hebdomadaire » sans gestion des jours de la semaine. |
| FCT-02 | Pas de déconnexion explicite ni de changement de mot de passe par l'utilisateur. |
| FCT-03 | Historique consultable uniquement par « N derniers points », sans filtre par période. |
| FCT-04 | L'inspecteur de widget ne propose que 14 des 28 types disponibles. |
| FCT-05 | Paramètres codés en dur : port, adresse d'écoute, chemin de la base, port Modbus. |
| FCT-06 | Pas d'endpoint de santé pour la supervision du service. |
| FCT-07 | Aucun test automatisé. |
| FCT-08 | Aucune documentation d'installation ou d'exploitation. |
| FCT-09 | Rhai : la variable `consigne` est liée en dur à `GTB.CTA01.Consigne_Temp`. |

---

## 3. Plan de correction

Ordre de priorité : sécurité, puis persistance et stabilité, puis performance, puis fonctionnalités. Chaque phase doit compiler, passer les tests et être vérifiée en exécution avant de passer à la suivante.

### Phase 0 — Préparation
- [x] Sauvegarde complète du projet et de la base.
- [x] Ajout des dépendances `argon2` (hachage des mots de passe) et `regex` (assainissement des SVG).

### Phase 1 — Sécurité
| Tâche | Corrige | Détail |
|---|---|---|
| [x] 1.1 Middleware d'authentification | SEC-01, SEC-05 | Couche Axum qui exige un jeton valide sur toutes les routes `/api/*` et `/ws/*`, sauf `/api/auth/login` et `/api/health`. Suppression du repli sur admin et du jeton égal au nom d'utilisateur. WebSocket authentifié par `?token=`. |
| [x] 1.2 Sessions sécurisées | SEC-02, SEC-04 | Jetons aléatoires de 256 bits, expiration après inactivité (12 h par défaut, configurable). Suppression de la session codée en dur. |
| [x] 1.3 Hachage Argon2id | SEC-03 | Hachage des mots de passe au format PHC. **Migration automatique** des mots de passe en clair existants au démarrage. Plus de mot de passe par défaut implicite pour les nouveaux comptes (mot de passe obligatoire, 6 caractères minimum). |
| [x] 1.4 Anti force brute | SEC-11 | Verrouillage de 60 s après 5 échecs consécutifs sur un même identifiant. |
| [x] 1.5 Validation des identifiants | SEC-07 | Les ID de tag, driver, synoptique, flux, planning et symbole sont limités à `[A-Za-z0-9_.:-]`, 1 à 128 caractères. |
| [x] 1.6 Assainissement SVG | SEC-06 | Côté serveur, à l'import, à l'enregistrement d'un synoptique et au chargement : suppression de `<script>`, `<foreignObject>`, `<iframe>`, `<object>`, `<embed>`, attributs `on*` et URL `javascript:`. Côté client, second filtrage avant rendu. |
| [x] 1.7 Échappement systématique IHM | SEC-06, SEC-07 | Fonctions `esc()` (HTML) et `jsArg()` (argument JS dans un attribut) appliquées à toutes les données dynamiques. |
| [x] 1.8 Traçabilité fiable | SEC-10 | L'acteur de l'audit est toujours l'utilisateur authentifié, transmis à toutes les opérations du noyau. L'opérateur d'un acquittement est l'utilisateur connecté (le commentaire reste libre). |
| [x] 1.9 Exposition réseau | SEC-08 | Suppression du CORS ouvert : l'IHM est servie par la même origine. Adresse et port configurables (voir 4.5). |
| [x] 1.10 Écran de connexion | SEC-09 | Fenêtre de connexion obligatoire et non fermable tant qu'on n'est pas authentifié. Plus d'identifiants pré-remplis. Retour automatique à la connexion sur une réponse 401. Avertissement dans les journaux tant qu'un compte utilise encore son mot de passe d'usine. |
| [x] 1.11 Scripts `.bat` | SEC-12 | Le serveur écrit au démarrage un jeton de contrôle local aléatoire dans `.scada_local_token`. Les scripts d'arrêt et de redémarrage l'utilisent. |
| [x] 1.12 Limites Rhai | SEC-13 | Limites de taille de chaînes et tableaux, de profondeur d'appel et d'expression. |

### Phase 2 — Persistance et stabilité
| Tâche | Corrige | Détail |
|---|---|---|
| [x] 2.1 Démarrage piloté par la base | STB-01, STB-02 | Table `meta` avec un indicateur d'initialisation. Au premier lancement, la démo est écrite en base (`INSERT OR IGNORE`, donc sans écraser l'existant). Ensuite, **seule la base fait foi**. Les drivers sont rechargés et démarrés depuis `drivers_config`. |
| [x] 2.2 Cohérence tags ↔ drivers | STB-03 | La configuration d'un driver est persistée avec ses tags courants à chaque création, modification ou suppression de tag. |
| [x] 2.3 Import complet | STB-04 | L'import d'un projet restaure aussi les drivers et renvoie un rapport des erreurs. |
| [x] 2.4 Base de secours en mémoire | STB-05 | Pool à une seule connexion pour `:memory:`. |
| [x] 2.5 Planificateur fiable | STB-06 | La règle n'est marquée appliquée que si l'écriture a réussi. Sinon, nouvelle tentative au cycle suivant. |
| [x] 2.6 Redémarrage respectueux | STB-07 | « Redémarrer » ne relance que les drivers dont l'état voulu est « actif ». |
| [x] 2.7 Simulateur | STB-08 | Les valeurs internes sont initialisées depuis la dernière valeur connue des tags. |
| [x] 2.8 Resynchronisation WebSocket | STB-09, STB-17 | En cas de retard (`Lagged`), un instantané complet est renvoyé au client. Gestion propre de `Closed` sur tous les canaux. |
| [x] 2.9 Esclave Modbus conforme | STB-10 | Bornes de quantité (2000 bits, 125 registres) et exceptions 0x02 (adresse) et 0x03 (valeur). |
| [x] 2.10 Écritures Modbus obsolètes | STB-11 | À la reconnexion, les commandes en attente sont purgées et journalisées au lieu d'être rejouées. |
| [x] 2.11 Journalisation des erreurs SQL | STB-12 | Toutes les erreurs de persistance sont tracées en `warn`. |
| [x] 2.12 Télémétrie drivers en direct | STB-13 | Diffusion de l'état des drivers toutes les 2 s, uniquement si au moins un client est connecté. |
| [x] 2.13 Corrections IHM | STB-14, STB-15 | Bouton impulsion relâché aussi sur `mouseleave`. Commandes masquées pour le profil Visualisation. Ajout de `.btn-success`. |
| [x] 2.14 Version unique | STB-16 | Version issue de `Cargo.toml` (`CARGO_PKG_VERSION`), passée en 1.1.0. |

### Phase 3 — Performance
| Tâche | Corrige | Détail |
|---|---|---|
| [x] 3.1 Écrivain d'historique groupé | PRF-01 | Une seule tâche dédiée reçoit les points par un canal borné et les écrit par lots (500 points ou 1 s) dans une transaction. Si la file est pleine, les points excédentaires sont comptés et non bloquants. |
| [x] 3.2 Rétention | PRF-02, PRF-08 | Index `recorded_at` et purge automatique quotidienne des points plus anciens que N jours (30 par défaut, configurable). |
| [x] 3.3 Réglages SQLite | PRF-03 | WAL, `synchronous=NORMAL`, `busy_timeout=5s`. |
| [x] 3.4 Cache Rhai | PRF-04 | Compilation unique des scripts (cache d'AST indexé par le texte du script). Moteur partagé, aussi pour « Exécuter maintenant ». |
| [x] 3.5 Rendu IHM groupé | PRF-05 | Mises à jour COV regroupées par `requestAnimationFrame` : au plus un rendu par image. Lignes du tableau des tags mises à jour sur place. |
| [x] 3.6 Éditeur de flux | PRF-06 | Pendant un déplacement, seuls le nœud et les câbles sont mis à jour. |

### Phase 4 — Fonctionnalités
| Tâche | Corrige | Détail |
|---|---|---|
| [x] 4.1 Planning hebdomadaire | FCT-01 | Jours actifs par règle (lundi à dimanche), gestion des plages qui passent minuit. Cases à cocher dans l'IHM. Les règles existantes deviennent « tous les jours ». |
| [x] 4.2 Compte utilisateur | FCT-02 | Déconnexion et changement de son propre mot de passe (`POST /api/auth/password`). |
| [x] 4.3 Historique par période | FCT-03 | Paramètres `from` et `to` (ISO 8601) sur l'API et sélecteurs de dates dans l'IHM. |
| [x] 4.4 Inspecteur complet | FCT-04 | Les 28 types de widgets sont sélectionnables, ainsi que le pas de consigne. |
| [x] 4.5 Configuration | FCT-05 | Variables d'environnement : `SCADA_BIND`, `SCADA_PORT`, `SCADA_DB`, `SCADA_MODBUS_SLAVE_PORT`, `SCADA_HISTORY_RETENTION_DAYS`, `SCADA_SESSION_TTL_HOURS`. |
| [x] 4.6 Santé | FCT-06 | `GET /api/health` (sans authentification) : statut, version, uptime. |
| [x] 4.7 Rhai | FCT-09 | `consigne` est conservée pour compatibilité ; ajout de `tag_text()` et documentation de `tag("ID")`. |

### Phase 5 — Qualité et documentation
| Tâche | Corrige | Détail |
|---|---|---|
| [x] 5.1 Tests unitaires | FCT-07 | Conversions `TagValue`, deadband, seuils et hystérésis, encodage Modbus, analyse des adresses, fenêtre horaire du planning, assainissement SVG, validation des ID, hachage des mots de passe. |
| [x] 5.2 Tests d'intégration manuels | – | Démarrage, connexion des 3 profils, refus 401/403, écriture de consigne Modbus, alarme, persistance après redémarrage. |
| [x] 5.3 README | FCT-08 | Installation, configuration, comptes, sécurité, exploitation. |

### Hors périmètre (recommandations futures)
- **HTTPS** : placer le serveur derrière un proxy inverse TLS (Caddy, nginx, IIS) pour un usage réseau.
- [x] **Vrais drivers OPC-UA, BACnet/IP et Siemens S7** : désormais implémentés et validés sur serveurs réels (Modbus 19/19, MQTT 13/13, S7 26/26, OPC-UA 23/23, BACnet/IP 20/20). Seul le simulateur GTB reste réservé à la simulation active.
- **Regroupement des requêtes Modbus** (PRF-07) : lecture des registres contigus en une seule trame.
- **CSP stricte** : elle demande de retirer les `onclick` en ligne de l'IHM (refonte importante).
- **Sessions persistantes** : aujourd'hui en mémoire, donc perdues à l'arrêt du processus.

---

## 4. Impacts pour l'exploitation après mise à jour

- **Connexion obligatoire** au chargement de l'IHM. Les comptes d'usine restent `admin` / `superviseur` / `visu`, avec leurs mots de passe actuels ; **les changer dès la première connexion**.
- Les mots de passe existants sont **migrés automatiquement** vers Argon2id au premier démarrage, sans action requise.
- Les sessions sont perdues à l'arrêt du processus : il faut se reconnecter après un redémarrage complet du `.exe`.
- La base existante est conservée. Les éléments de démo déjà présents restent, et ceux supprimés ensuite ne reviennent plus.
- Les scripts `.bat` continuent de fonctionner grâce au jeton local.

---

## 5. Journal de réalisation

**16/10/2026 — version 1.1.0 : les 42 tâches du plan sont réalisées.** Les 6 constats découverts en cours de route (SEC-14, STB-18 à STB-22) sont également corrigés.

### 5.1 Modifications par fichier

| Fichier | Changements principaux |
|---|---|
| `src/security.rs` *(nouveau)* | Argon2id, jetons de 256 bits, `validate_id`, `slugify_id`, assainissement SVG (y compris `<style>`), 10 tests. |
| `src/config.rs` *(nouveau)* | 6 variables d'environnement `SCADA_*`. |
| `src/main.rs` | Réécrit : middleware d'authentification (aucun repli admin), en-têtes de sécurité, suppression du CORS ouvert, SQLite en WAL, `/api/health`, `/api/auth/password`. WebSocket authentifié, resynchronisé en cas de retard et filtré selon le rôle. Historique par période, CSV protégé contre l'injection de formules, jeton local `.scada_local_token`. |
| `src/core.rs` | Sessions avec expiration et anti force brute. Acteur réel dans tout l'audit. La base fait foi (table `meta`, initialisation unique). Drivers rechargés depuis la base. Écritures non bloquantes, converties en valeur brute et relues depuis l'équipement. Planning hebdomadaire avec nouvel essai en cas d'échec. Écrivain d'historique groupé, rétention automatique, import et export complets, 4 tests. |
| `src/flow_engine.rs` | Moteur Rhai partagé, cache de compilation, bac à sable renforcé, `tag_text()`, avertissement unique par sortie en échec, 2 tests. |
| `src/drivers/mod.rs` | Restauration des consignes du simulateur, purge des écritures Modbus obsolètes, 2 tests. |
| `src/drivers/modbus_slave.rs` | Fonction `process_pdu` testable, bornes et exceptions Modbus conformes, 4 tests. |
| `src/models.rs` | Jours de la semaine dans `ScheduleRule`, `is_active_at` (plages passant minuit), `ChangePasswordRequest`, 6 tests. |
| `static/app.js` | Réécrit : `esc()` / `jsArg()` partout, `sanitizeSvg()`, gestion de session (401 → écran de connexion), rendu groupé par `requestAnimationFrame`, catalogue mis à jour sur place, déplacement fluide des nœuds de flux, 28 widgets dans l'inspecteur, planning hebdomadaire, historique par période, téléchargements authentifiés. |
| `static/index.html`, `static/styles.css` | Écran de connexion verrouillé, section « Mon compte », pastilles de jours, `.btn-success`, correction de l'animation de rotation, commandes masquées en Visualisation. |
| `*.bat` | Jeton local ; `Arreter-SCADA.bat` attend la fin de l'arrêt propre avant de forcer. |
| `README.md`, `.gitignore` *(nouveaux)* | Documentation d'exploitation ; exclusion de `target/`, des bases et du jeton. |

### 5.2 Vérifications effectuées

- **Compilation** : aucune erreur ni avertissement. `cargo clippy` : seuls restent 5 avertissements de style antérieurs (accesseurs `name()` des drivers, `impl Default` dérivable).
- **Tests unitaires** : 28 tests, tous réussis (`cargo test`).
- **API**, testée sur une copie de la base de production puis sur une base vierge :
  - 401 sans jeton, avec l'ancien jeton par défaut et avec un nom d'utilisateur utilisé comme jeton ;
  - 403 pour le profil Visualisation ;
  - verrouillage après 5 échecs ;
  - jeton local accepté en boucle locale ;
  - commande refusée vers un équipement déconnecté ;
  - écriture Modbus de bout en bout (consigne 9,5 °C relue ; compresseur arrêté → courant 2 A) ;
  - validation des dates, des heures et des identifiants ;
  - SVG piégé nettoyé ;
  - audit tracé avec l'utilisateur réel ;
  - WebSocket refusé sans jeton (401) et accepté avec (101).
- **Persistance après redémarrage** : un synoptique supprimé ne revient pas, un driver arrêté reste arrêté, un driver ajouté et ses tags sont rechargés. Les mots de passe sont migrés en Argon2id.
- **IHM** (Chrome headless) :
  - connexion verrouillée, échec puis succès ;
  - les 7 onglets s'affichent sans erreur JavaScript ;
  - le catalogue est mis à jour sur place ;
  - une consigne envoyée depuis le synoptique est appliquée ;
  - aucun XSS exécuté (libellé, unité et valeur piégés) ;
  - le profil Visualisation est restreint ;
  - une session révoquée ramène à l'écran de connexion.
- **Scripts** : `Arreter-SCADA.bat` obtient un arrêt propre via le jeton local.

### 5.3 Points d'attention pour la mise en service

1. **Recompiler en release** (`cargo build --release`) : `Demarrer-SCADA.bat` utilise ce binaire en priorité.
2. Au premier démarrage sur la base existante, la démonstration est réinsérée **sans écraser** l'existant. Les éléments de démo supprimés **avant** cette version réapparaissent donc une seule fois ; ensuite, une suppression est définitive.
3. **Changer les 3 mots de passe d'usine** : un avertissement apparaît à chaque démarrage tant que ce n'est pas fait.
4. Le driver `MODBUS_GF01` garde l'endpoint enregistré en base (`127.0.0.1:5502`). Si `SCADA_MODBUS_SLAVE_PORT` est modifié, il faut mettre à jour l'endpoint du driver.
5. La suppression d'un driver supprime désormais ses variables (confirmation affichée dans l'IHM).
6. Restent hors périmètre : HTTPS, regroupement des requêtes Modbus, CSP stricte (voir §3) ; les vrais drivers OPC-UA, BACnet/IP et Siemens S7 sont désormais implémentés.

---

## 6. Revue de contrôle et version 1.2.0 (16/10/2026)

Après la mise en service de la 1.1, une seconde revue complète du code a été menée. Elle a relevé de nouveaux défauts et opportunités d'amélioration, tous traités dans la 1.2.0.

### 6.1 Constats de la revue

| ID | Sévérité | Constat | Correction |
|---|---|---|---|
| REV-01 | Moyenne | **Inondation d'alarmes** : une coupure réseau levait une alarme critique *par tag* (6 alarmes pour le groupe froid). | Une seule alarme de communication par driver (`COMM.<driver>`), qui apparaît au premier échec et disparaît au rétablissement. Un arrêt volontaire du driver la fait revenir à la normale. |
| REV-02 | Moyenne | Sur perte de communication, les tags affichaient la valeur de repli du driver (0, `false`) au lieu de la dernière valeur connue. | La dernière valeur est conservée, avec la qualité `BadDisconnect`. |
| REV-03 | Moyenne | **Bagotement** : aucune bande morte d'alarme, d'où des apparitions et disparitions en boucle autour d'un seuil (ex. température extérieure à 15 °C). | Bande morte d'alarme configurable (`alarm_config.deadband`, par défaut max(2 × deadband COV, 1 % du seuil)). |
| REV-04 | Moyenne | Aggravation H → HH silencieuse : la sévérité changeait sans diffusion, sans journal ni nouvel acquittement. | Événement `AGGRAVATION` journalisé ; l'alarme repasse en « non acquittée ». |
| REV-05 | Faible | Les messages d'alarme contenaient la valeur courante : la liste aurait été rediffusée à chaque changement de valeur. | Messages stables ; la valeur de déclenchement est conservée dans `trigger_value`. |
| REV-06 | Moyenne | **Historique par période faux** : sur une journée avec une limite de 1 000 points, seuls les 1 000 derniers points (quelques minutes) étaient renvoyés. | Échantillonnage régulier sur toute la période (`ROW_NUMBER`) ; min, max, moyenne et écart-type calculés en SQL sur tous les points. |
| REV-07 | Moyenne | Modbus : une seule adresse de tag mal saisie, ou une exception de l'automate, coupait toute la scrutation et provoquait des reconnexions en boucle. | Le tag fautif est isolé (qualité `BadDisconnect`, un seul avertissement) ; seule une erreur de transport déclenche la reconnexion. |
| REV-08 | Faible | La persistance des drivers se faisait en tâches détachées : deux modifications rapprochées pouvaient s'écrire dans le désordre. | Écriture attendue (`persist_driver` asynchrone). |
| REV-09 | Moyenne | Impossible de modifier l'endpoint ou la période de scrutation d'un driver sans le supprimer avec ses tags. | `PUT /api/drivers/:id` (avec validation et redémarrage à chaud) et bouton « ⚙️ Paramètres » dans l'IHM. |
| REV-10 | Faible | L'IHM était servie depuis `./static`, relatif au dossier courant : page blanche si l'exécutable était lancé d'ailleurs. | Résolution automatique (dossier courant, puis remontée depuis l'exécutable) et variable `SCADA_STATIC_DIR`. |
| REV-11 | Faible | Pas de politique de sécurité du contenu (CSP). | En-tête CSP : aucune ressource ni connexion vers un site tiers, `frame-ancestors 'none'`. |
| REV-12 | Faible | Journal d'alarmes : opérateur vide (`""`) au lieu de `null` pour les événements système. | Corrigé. |
| REV-13 | Faible | 5 avertissements `clippy` hérités de la version d'origine. | Corrigés : `clippy` ne remonte plus aucun avertissement. |

### 6.2 Améliorations de performance

| Tâche | Effet |
|---|---|
| **Lecture Modbus par blocs** (PRF-07, auparavant hors périmètre) | Les registres contigus d'une même zone sont lus en une trame (125 registres / 2 000 bits maximum, trous de 8 adresses au plus comblés). Le groupe froid passe de 6 trames à 2 par cycle. |
| **WebSocket groupé** | Les mises à jour sont cumulées 100 ms (dernière valeur par tag) et envoyées en un seul message `TagUpdateBatch`. |
| **Moteur de flux** | À chaque changement de valeur, seuls les flux qui lisent le tag sont copiés (au lieu de tous les flux actifs). |

### 6.3 Nouvelles fonctionnalités

- **Mise en sommeil des alarmes** (ISA-18.2 « shelving ») : 1 à 480 minutes, avec commentaire, réveil manuel ou automatique, journalisation et audit. Les alarmes en sommeil sont exclues du bandeau et des compteurs (💤). API : `POST /api/alarms/:id/shelve` et `/unshelve`.
- **Modification des drivers** : `PUT /api/drivers/:id` et bouton « ⚙️ Paramètres ».
- **Filtre du journal d'audit** (500 dernières entrées).
- Compteur d'échantillons de l'historique au format « affichés / total » quand la période est échantillonnée.

### 6.4 Vérifications

- `cargo test` : **35 tests**, tous réussis (dont les nouveaux : seuils et bande morte d'alarme, planification et décodage des blocs Modbus) ; `cargo clippy` sans avertissement.
- Intégration, sur une copie de la base de production avec un serveur lancé depuis un autre dossier :
  - IHM servie et en-tête CSP présent ;
  - une seule alarme de communication pour un driver de 3 tags ;
  - modification d'endpoint (refus si invalide, redémarrage à chaud) ;
  - lecture par blocs exacte ;
  - tag à adresse invalide et adresse hors mémoire isolés sans couper le driver ;
  - historique d'une journée échantillonné de 03:00 à 14:21 (99 points sur 2 171) ;
  - mise en sommeil (durée validée, refus 403 pour le profil Visualisation, journal).
- IHM (Chrome headless) : toute la séquence de la 1.1 rejouée sans erreur JavaScript ni violation de CSP.

### 6.5 Reste à faire (pistes pour une version suivante)

- **Persistance de l'état des alarmes** : elles sont en mémoire et réapparaissent comme « nouvelles » après un redémarrage du processus.
- **HTTPS natif** (aujourd'hui via proxy inverse). *(Note : les vrais drivers OPC-UA, BACnet/IP et Siemens S7 sont désormais réalisés et opérationnels).*
- **CSP stricte** sans `unsafe-inline` : il faut retirer les gestionnaires `onclick` en ligne de l'IHM.
- **Temporisation d'apparition des alarmes** (on-delay) pour filtrer les dépassements très brefs.

---

## 7. Version 1.3.0 (16/10/2026)

### 7.1 Mise en sommeil des alarmes désactivable

| Niveau | Réglage | Effet |
|---|---|---|
| **Système** (administrateur, carte « Paramètres Système ») | `alarm_shelving_enabled` | Désactivée, la fonction réveille **immédiatement** toutes les alarmes en sommeil (événement `FIN_SOMMEIL` au journal), toute nouvelle demande est refusée (403) et les boutons « 💤 Sommeil » disparaissent en direct sur tous les postes ouverts (message WebSocket `SettingsUpdate`). |
| **Système** | `alarm_shelve_max_minutes` (1 à 480) | Durée maximale d'une mise en sommeil ; une valeur réduite raccourcit aussi les mises en sommeil en cours. |
| **Variable** (formulaire des tags, « Sommeil alarme : Interdit ») | `alarm_config.shelving_disabled` | Interdit la mise en sommeil de l'alarme de ce tag, par exemple pour les alarmes de sécurité (ISA-18.2). Si l'alarme dort au moment de l'interdiction, elle est réveillée. |

Les réglages sont persistés (table `meta`, clé `settings`) et tracés dans l'audit. API : `GET /api/system/settings` (tout utilisateur connecté) et `PUT /api/system/settings` (administrateur).

### 7.2 Persistance de l'état des alarmes (piste §6.5)

Les alarmes sont maintenant enregistrées dans la table `alarms_state` à chaque changement : apparition, aggravation, disparition, acquittement, mise en sommeil, réveil, effacement. Un **écrivain unique** garantit l'ordre des écritures. Au redémarrage :

- les acquittements, commentaires et mises en sommeil en cours sont conservés, sans « nouvelle apparition » parasite ;
- les conditions sont réévaluées dès les premières valeurs valides (une alarme dont la condition a disparu passe en « disparue » ou « normale ») ;
- les alarmes devenues sans objet (tag supprimé, driver supprimé ou arrêté) sont écartées.

### 7.3 Correction

| ID | Sévérité | Constat | Correction |
|---|---|---|---|
| REV-14 | Moyenne | « Acquitter tout » n'écrivait **aucune** ligne dans le journal d'alarmes (seul l'audit global était tracé). | Un événement `ACQUITTEMENT` par alarme, avec l'opérateur. |

### 7.4 Vérifications

- `cargo test` : **37 tests**, tous réussis (dont les valeurs par défaut des réglages et la compatibilité avec les alarmes et tags déjà enregistrés) ; `cargo clippy` sans avertissement.
- Intégration sur une copie de la base de production :
  - désactivation, avec réveil immédiat puis refus 403 ;
  - refus 403 pour un superviseur qui tente de modifier les réglages ;
  - durée maximale de 30 min : 60 min refusé, 30 min accepté ;
  - interdiction par tag, avec réveil puis refus, l'alarme de communication restant autorisée ;
  - trois redémarrages successifs : réglages, acquittement et commentaire, mise en sommeil conservés, et aucune réapparition.
- IHM (Chrome headless, deux postes simultanés) : propagation en direct du réglage chez le superviseur, carte des réglages masquée pour lui, et non-régression complète sans erreur.
