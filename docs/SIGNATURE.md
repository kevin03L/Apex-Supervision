# Politique de signature de code / Code Signing Policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org)

## English Summary

Apex Supervision is an open-source SCADA / BMS platform written in Rust. This document outlines the project's code signing policy, security commitments, roles, and build pipeline procedures required by the SignPath Foundation.

- **Project Roles**:
  - *Committers and reviewers*: `kevin03L` (repository member)
  - *Approvers*: `kevin03L` (project owner)
- **Privacy & Telemetry**:
  - The software transmits no telemetry, analytics, or diagnostic data to third parties.
  - No automated update checks or background phone-home requests are performed.
  - Network communications are strictly limited to industrial devices and services configured by the end-user operator (e.g. Modbus TCP, MQTT brokers, OPC-UA servers, BACnet/IP devices, Siemens S7 PLCs).
- **Signing Process**:
  - Code signing is exclusively executed within GitHub Actions CI on official git release tags (`v*`).
  - No build artifact produced outside the GitHub Actions CI pipeline or on local machines is signed with the SignPath certificate.
  - Every signing request submitted to SignPath requires manual approval by the project owner.
- **Signed Artifacts**:
  - `apex-scada.exe` (SCADA server engine)
  - `apex-scada-admin.exe` (desktop management utility)
  - Inno Setup uninstaller executable
  - Inno Setup installer executable (`apex-supervision-*-windows-x64-setup.exe`)

---

## 1. Attribution

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org)

Apex Supervision bénéficie du programme gratuit de signature de code pour projets open source proposé par la fondation SignPath. Les certificats Authenticode délivrés permettent d'assurer l'intégrité des binaires distribués, l'identification de l'éditeur (« SignPath Foundation ») et la compatibilité avec les exigences de sécurité de Windows (Smart App Control, Windows Defender SmartScreen).

## 2. Rôles et responsabilités

Les responsabilités relatives au code source, à l'infrastructure d'intégration continue et à l'approbation des signatures sont attribuées ainsi :

| Rôle | Personne ou compte | Périmètre |
|---|---|---|
| *Committers and reviewers* | `kevin03L` | Développement, relecture des contributions (pull requests), maintien de l'intégration continue et des règles de sécurité. |
| *Approvers* | `kevin03L` | Propriétaire du projet : validation finale des versions, pose des étiquettes de publication (`v*`) et approbation manuelle de chaque demande de signature sur SignPath. |

Toute modification apportée au code source ou aux configurations de compilation fait l'objet d'un contrôle strict avant fusion dans la branche principale.

## 3. Confidentialité et respect de la vie privée

Apex Supervision est conçu pour une exploitation en environnement industriel et tertiaire :

- **Absence de télémétrie** : le logiciel ne collecte et ne transmet aucune métrique d'utilisation, statistique, rapport de plantage ou donnée technique vers un quelconque serveur tiers ou service cloud.
- **Absence de vérification de mise à jour automatique** : aucune requête sortante de type « phone-home » ou vérification de nouvelle version n'est exécutée en arrière-plan.
- **Flux réseau strictement maîtrisés** : les seules communications réseau initiées par le logiciel s'adressent exclusivement aux équipements industriels et services expressément configurés par l'exploitant dans sa configuration locale (automates programmables industriels, brokers MQTT, serveurs OPC-UA, centrales de traitement d'air, compteurs d'énergie, etc.) ou aux clients locaux autorisés accédant à l'IHM web de supervision (port HTTP local sécurisé derrière un proxy inverse).

## 4. Procédé de signature et traçabilité

Pour garantir la provenance et l'intégrité absolue des binaires distribués :

1. **Source publique vérifiée** : seuls les commits balisés par une étiquette de version stable ou préliminaire (`v*`) sur le dépôt public officiel font l'objet d'un processus de signature.
2. **Signature exclusive par la CI GitHub Actions** : aucune signature officielle n'est effectuée sur un poste de développement personnel ou une machine locale. La chaîne de compilation et les demandes de signature s'exécutent intégralement sur les exécuteurs sécurisés de GitHub Actions.
3. **Approbation manuelle systématique** : chaque demande de signature soumise à la plateforme SignPath fait l'objet d'une vérification et d'une approbation manuelle par le propriétaire (`kevin03L`) avant l'application de la signature Authenticode.
4. **Horodatage conforme RFC 3161** : tous les exécutables sont horodatés lors de la signature afin que leur validité perdure après l'échéance du certificat.

## 5. Périmètre des fichiers signés

La politique de signature couvre l'ensemble des fichiers exécutables Windows distribués avec la solution :

1. `apex-scada.exe` : moteur d'exécution temps réel et service d'arrière-plan SCADA ;
2. `apex-scada-admin.exe` : utilitaire desktop de configuration et d'administration du service ;
3. Le désinstalleur d'Apex Supervision (généré lors de la préparation Inno Setup) ;
4. L'installateur final autonome d'Apex Supervision (`apex-supervision-<version>-windows-x64-setup.exe`).
