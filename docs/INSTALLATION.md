# Apex Supervision — Installation, mise à jour et désinstallation

> **Version alpha (0.4.0-alpha.x)** : à réserver aux essais et à la mise en service accompagnée. Voir l'état de validation de chaque procédure dans [REFONTE_ALPHA.md](REFONTE_ALPHA.md) §12.

Apex Supervision se compose de deux programmes :

| Programme | Rôle |
|---|---|
| `apex-scada` | Moteur GTC/SCADA et serveur de l'IHM web. Il fonctionne en **service** (Windows) ou en **unité systemd** (Linux), sans session ouverte. |
| `apex-scada-admin` | Utilitaire desktop : configuration, état, démarrage et arrêt, journaux, diagnostics, sauvegardes, assistant initial. Le fermer n'arrête jamais le moteur. |

Les opérateurs utilisent l'IHM web depuis un navigateur, à l'adresse `http://<poste>:<port>/` (par défaut `http://localhost:3000/`).

---

## 1. Windows 10 / 11 (64 bits)

### 1.1 Prérequis

- Windows 10 ou 11, 64 bits. Un compte administrateur est nécessaire pour l'installation.
- Aucun composant supplémentaire. L'utilitaire dessine son interface avec DirectX 12, Vulkan ou le rendu logiciel WARP sur les postes et machines virtuelles sans GPU.

### 1.2 Installation

1. Vérifiez l'empreinte de l'installateur :
   `Get-FileHash apex-supervision-<version>-windows-x64-setup.exe` doit afficher la valeur du fichier `.sha256` publié avec lui.
2. Lancez `apex-supervision-<version>-windows-x64-setup.exe` et acceptez l'élévation (UAC).
3. À la fin, laissez cochée la case « Lancer l'assistant de configuration ». L'assistant demande :
   - le **nom de l'instance** ;
   - l'**accès à l'IHM** : ce poste uniquement (recommandé), tout le réseau, ou une adresse précise ;
   - le **port web**, avec un bouton de vérification de disponibilité ;
   - le **démarrage automatique** du service avec Windows ;
   - le **mode sûr**, qui bloque toute commande vers les équipements réels pendant la mise en service ;
   - la **simulation**, réservée à la démonstration ;
   - la rétention de l'historique et l'expiration des sessions ;
   - les emplacements des données, si vous souhaitez les personnaliser ;
   - la **reprise des données** d'une version antérieure, si une base `rust_scada_flow.db` est détectée (elle est **copiée**, jamais modifiée).
4. L'assistant enregistre la configuration, règle le service et démarre le moteur.

Contenu installé :

| Élément | Emplacement |
|---|---|
| Programmes, IHM web, exemples | `C:\Program Files\Apex Supervision\` |
| Configuration et ses sauvegardes | `C:\ProgramData\Apex Supervision\config\` |
| Base de données (configuration métier, historique, comptes) | `C:\ProgramData\Apex Supervision\data\` |
| Journaux | `C:\ProgramData\Apex Supervision\logs\` |
| Jeton de contrôle local | `C:\ProgramData\Apex Supervision\run\` |
| Service | `ApexSupervision` (« Apex Supervision »), compte **LocalService**, redémarrage automatique après un échec |
| Menu Démarrer | « Apex Supervision — Administration », « Assistant de configuration », « Documentation » |

Permissions posées par l'installation :
- `data` et `run` : SYSTEM, Administrateurs et le service uniquement ;
- `logs` : écriture par le service, lecture par les utilisateurs (utilitaire desktop) ;
- `config` : lecture par les utilisateurs et le service, modification par les administrateurs.

L'utilitaire s'exécute en utilisateur standard. Les actions sur le service et la modification de la configuration demandent une confirmation administrateur (UAC). Il propose aussi de **se relancer en administrateur**.

### 1.3 Installation silencieuse (déploiement)

```bat
apex-supervision-<version>-windows-x64-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /LOG="%TEMP%\apex-setup.log"
apex-scada config init --name "Site A" --bind 127.0.0.1 --port 3000
apex-scada service start
```

Ces commandes s'exécutent dans une invite administrateur, depuis `C:\Program Files\Apex Supervision`. En mode silencieux, l'assistant n'est pas lancé : la configuration se crée avec `config init`.

### 1.4 Mise à jour

Relancez l'installateur de la nouvelle version. Il effectue, dans l'ordre :
1. l'arrêt propre du service s'il fonctionne ;
2. le remplacement des programmes et de l'IHM web ;
3. la mise au format courant de la configuration (`apex-scada config migrate` : valeurs conservées, ancienne version sauvegardée) ;
4. le redémarrage du service s'il fonctionnait avant la mise à jour.

**La configuration, la base et les journaux ne sont jamais écrasés.** La base est migrée par le moteur à son démarrage.

### 1.5 Désinstallation

Utilisez « Paramètres → Applications → Apex Supervision → Désinstaller ». La désinstallation :
- arrête et supprime le service ;
- supprime les programmes, les raccourcis et l'IHM web ;
- **demande** s'il faut aussi supprimer la configuration, les données et les journaux. La réponse par défaut est **Non** ; en désinstallation silencieuse, ils sont toujours conservés.

Pour les supprimer plus tard, effacez le dossier `C:\ProgramData\Apex Supervision\`.

---

## 2. Linux (Debian 12, Ubuntu 22.04 LTS, Ubuntu 24.04 LTS)

### 2.1 Prérequis

- Distribution avec **systemd**, architecture x86_64. Le paquet est construit sur Ubuntu 22.04 (glibc 2.35).
- Utilitaire desktop : session X11 ou Wayland, `libxkbcommon0`, et un pilote Vulkan ou OpenGL (`mesa-vulkan-drivers` pour le rendu logiciel). Ces paquets sont **suggérés**, pas imposés, car un serveur sans interface graphique n'utilise que le moteur.

### 2.2 Installation

```bash
sha256sum -c apex-supervision_<version>_amd64.deb.sha256
sudo apt install ./apex-supervision_<version>_amd64.deb
```

Le paquet effectue les opérations suivantes :
- il crée l'utilisateur système `apex-scada` (sans shell ni connexion) et le groupe du même nom ;
- il génère `/etc/apex-supervision/apex-scada.toml` avec des valeurs par défaut sûres : écoute sur `127.0.0.1:3000`, simulation désactivée ;
- il installe, active et démarre l'unité `apex-scada.service`.

| Élément | Emplacement |
|---|---|
| Programmes | `/usr/bin/apex-scada`, `/usr/bin/apex-scada-admin` |
| IHM web, exemples | `/usr/share/apex-supervision/` |
| Configuration et sauvegardes | `/etc/apex-supervision/` (`root:apex-scada`, groupe en écriture) |
| Données | `/var/lib/apex-supervision/` (`apex-scada`, 0750) |
| Journaux | `/var/log/apex-supervision/` et le journal systemd (`journalctl -u apex-scada`) |
| Jeton de contrôle local | `/run/apex-supervision/` |
| Règle polkit | `/usr/share/polkit-1/rules.d/50-apex-scada.rules` |

**Administrateurs du poste** : ajoutez votre compte au groupe `apex-scada`, puis reconnectez-vous :

```bash
sudo usermod -aG apex-scada $USER
```

Les membres du groupe peuvent alors modifier la configuration avec l'utilitaire, et démarrer, arrêter ou redémarrer le service sans mot de passe root (règle polkit). Activer ou désactiver le démarrage automatique reste soumis à authentification.

> Ubuntu 22.04 fournit polkit 0.105, qui ignore les règles JavaScript : sur cette version, `systemctl` demande le mot de passe administrateur pour chaque action.

Configuration initiale sans interface graphique (serveur) :

```bash
sudo apex-scada config init --force --config /etc/apex-supervision/apex-scada.toml \
     --name "Site A" --bind 0.0.0.0 --port 3000
sudo chgrp apex-scada /etc/apex-supervision/apex-scada.toml && sudo chmod 0660 /etc/apex-supervision/apex-scada.toml
sudo systemctl restart apex-scada
```

Avec une session graphique, lancez « Apex Supervision — Administration » depuis le menu des applications, ou `apex-scada-admin --wizard`.

### 2.3 Port inférieur à 1024

Le service ne dispose d'aucune capacité système. Pour écouter sur le port 80 :

```bash
sudo systemctl edit apex-scada
# [Service]
# AmbientCapabilities=CAP_NET_BIND_SERVICE
# CapabilityBoundingSet=CAP_NET_BIND_SERVICE
```

Une solution plus sûre consiste à placer un proxy HTTPS (nginx, Caddy) devant le port 3000.

### 2.4 Mise à jour

```bash
sudo apt install ./apex-supervision_<nouvelle-version>_amd64.deb
```

Le service est arrêté, les programmes sont remplacés, la configuration est mise au format courant (valeurs conservées, sauvegarde dans `/etc/apex-supervision/backups/`), puis le service est redémarré.

### 2.5 Désinstallation

| Commande | Effet |
|---|---|
| `sudo apt remove apex-supervision` | Supprime les programmes et l'unité. **Conserve** la configuration, les données et les journaux. |
| `sudo apt purge apex-supervision` | Supprime **définitivement** aussi `/etc/apex-supervision`, `/var/lib/apex-supervision`, `/var/log/apex-supervision`, ainsi que l'utilisateur et le groupe `apex-scada`. |

---

## 3. Mode portable (développement, démonstration)

Tout est rangé dans un seul dossier, sans service ni droits administrateur :

```bash
apex-scada config init --portable --simulation   # crée ./apex-scada.toml (simulation active)
apex-scada run                                    # Ctrl+C pour un arrêt propre
apex-scada-admin --config ./apex-scada.toml       # utilitaire sur cette instance
```

Le moteur utilise le mode portable dès qu'un fichier `apex-scada.toml` se trouve dans le dossier courant ou à côté de l'exécutable. Il l'utilise aussi lorsqu'un fichier est désigné par `--config` ou par la variable `APEX_SCADA_CONFIG`.

---

## 4. Reprise des données d'une version antérieure (≤ 1.3)

Les versions 1.x enregistraient tout dans `rust_scada_flow.db`, dans le dossier de lancement. Pour reprendre ces données, moteur arrêté :

```bash
apex-scada migrate-legacy --from "<dossier ou fichier rust_scada_flow.db>"
```

La base est **copiée** dans le répertoire des données, et la source n'est jamais modifiée. Si une base existe déjà à la destination, `--force` la remplace après l'avoir sauvegardée en `.bak-<date>`. L'assistant de configuration propose cette reprise lorsqu'il détecte une ancienne base.

---

## 5. Diagnostic rapide

| Question | Commande |
|---|---|
| Version installée | `apex-scada version` |
| Configuration valide ? Port libre ? | `apex-scada config check` |
| Emplacements et configuration effective | `apex-scada config show` |
| État du service | Windows : `apex-scada service status` · Linux : `systemctl status apex-scada` |
| Journaux | Utilitaire, onglet « Journaux » · Linux : `journalctl -u apex-scada -e` |
| Moteur joignable ? | `curl http://127.0.0.1:3000/api/health` |

L'onglet « Diagnostics » de l'utilitaire enchaîne toutes ces vérifications et produit un rapport à joindre à une demande d'assistance.
