# Modules complémentaires de symboles

Les modules sont publiés séparément avec chaque version d'Apex Supervision. Téléchargez l'archive `apex-symboles-<version>.zip` depuis les [Releases](https://github.com/kevin03L/Apex-Supervision/releases), vérifiez son empreinte `.sha256`, puis installez chaque fichier JSON dans **Bibliothèque → Modules complémentaires → Installer un module (.json)**. Un administrateur peut désinstaller un module. Si un synoptique utilise ses symboles, une confirmation de désinstallation forcée est nécessaire.

Modules disponibles :

- `cvc_gtb.json` : CVC / GTB bâtiment ;
- `electricite_energie.json` : électricité et énergie ;
- `industrie_manufacturiere.json` : industrie manufacturière ;
- `instrumentation_isa.json` : instrumentation ISA ;
- `process_hydraulique.json` : process hydraulique ;
- `securite_incendie.json` : sécurité incendie ;
- `utilites_industrielles.json` : utilités industrielles.

Un module personnel est un fichier JSON UTF-8 de cette forme :

```json
{
  "format": "apex-symboles",
  "version": "1.0.0",
  "id": "mon_module",
  "titre": "Mon module",
  "description": "Symboles de démonstration",
  "norme": "Usage interne",
  "symbols": [{
    "id": "mon_module-exemple",
    "name": "Exemple",
    "category": "Mon module",
    "standard": "Usage interne",
    "default_width": 100,
    "default_height": 100,
    "animate_rotation": false,
    "colorize_on_state": true,
    "svg_content": "<svg viewBox=\"0 0 100 100\"><circle cx=\"50\" cy=\"50\" r=\"30\"/></svg>"
  }]
}
```

Chaque identifiant de symbole doit commencer par l'identifiant du module suivi d'un tiret. Les SVG sont assainis à l'import. Une réinstallation met à jour les symboles portant les mêmes identifiants.

## Convention des états animés des symboles

Elle est partagée entre les dessins (`packs/*.json`) et l'IHM (`static/synoptic.js`, `static/styles.css`).
Toutes les règles passent par des **classes** et des **variables CSS** : aucun script dans les SVG.

### États fournis par l'IHM
L'IHM pose sur l'enveloppe du symbole (`.custom-svg-wrapper`) **une** classe d'état et la couleur
(`color`), que le dessin reprend par `currentColor` :

| Classe sur l'enveloppe | Condition | Couleur (`currentColor`) |
|---|---|---|
| `etat-marche` | variable principale vraie, ou numérique au-dessus du seuil | vert `#10b981` |
| `etat-arret` | variable principale fausse, ou à zéro | bleu `#38bdf8` |
| `etat-defaut` | variable de défaut liée vraie (propriété `defaut_tag_id`), ou alarme active sur la variable principale | rouge `#ef4444` |
| `etat-inconnu` | qualité différente de `Good`, ou variable non liée | orange `#f59e0b` |

Elle pose aussi la variable CSS **`--apex-niveau`**, nombre de 0 à 1, calculée à partir de la valeur
numérique et des propriétés `min` et `max` du widget (0 et 100 par défaut, valeur bornée).

### Classes utilisables dans les dessins

| Classe | Effet piloté par l'IHM |
|---|---|
| `sym-rotatable` | (existant) tourne en `etat-marche` ; `style="transform-origin:CXpx CYpx;"` obligatoire |
| `sym-marche` | visible seulement en `etat-marche` (flamme, flèches de débit, voyant vert, vanne ouverte) |
| `sym-arret` | visible seulement en `etat-arret` ou `etat-inconnu` (vanne fermée, opercule baissé) |
| `sym-defaut` | visible seulement en `etat-defaut`, clignote (1 Hz) |
| `sym-flux` | trait en pointillés animés (écoulement) en `etat-marche`, fixe sinon ; à poser sur un `path` ou une `line` avec `stroke-dasharray` |
| `sym-niveau` | remplissage vertical proportionnel à `--apex-niveau` (`transform: scaleY(...)`) ; `style="transform-origin:Xpx BASpx;"` obligatoire, BAS étant le bas de la zone remplie |
| `sym-vibre` | légère vibration en `etat-marche` (compresseur, crible vibrant) |

Règles :
- un élément porte au plus **une** de ces classes, et les classes ne s'imbriquent pas entre elles, sauf
  `sym-rotatable` à l'intérieur d'un `sym-marche` ;
- un symbole sans état particulier n'en utilise aucune et reste valable (couleur seule) ;
- en l'absence de l'IHM (aperçu, export), le dessin reste lisible : par défaut, toutes les parties sont
  visibles et le niveau est à 100 % ;
- avec la préférence système « réduire les animations » (`prefers-reduced-motion: reduce`), les animations
  s'arrêtent, mais seules les parties de l'état courant restent visibles (une vanne ne doit pas paraître
  à la fois ouverte et fermée).

### Rendu 2,5D
- Corps d'équipement : dégradés métalliques neutres (`linearGradient` ou `radialGradient`), avec les
  couleurs autorisées : `#0b1120`, `#111827`, `#1e293b`, `#273449`, `#334155`, `#475569`, `#64748b`,
  `#94a3b8`, `#cbd5e1`, `#e2e8f0`, `#ffffff` (opacité libre).
- Parties actives (moteur, liquide, flamme, voyant, contour d'état) en `currentColor`, avec
  `fill-opacity` et `stroke-opacity` libres.
- Ombre légère possible (ellipse sombre sous l'équipement) ; pas de `filter`, trop coûteux sur un
  synoptique de 200 symboles.
- Les `id` internes (dégradés, `clipPath`) commencent par l'id du symbole suivi d'un tiret, et sont
  référencés par `fill="url(#…)"` (référence interne uniquement).

Les modules du projet sont diffusés sous licence **GPL-3.0**.
