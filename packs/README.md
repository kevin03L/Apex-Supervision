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

## États animés des symboles

L'IHM pose sur le conteneur du symbole `etat-marche` (vert `#10b981`), `etat-arret` (bleu `#38bdf8`), `etat-defaut` (rouge `#ef4444`) ou `etat-inconnu` (orange `#f59e0b`). Le défaut provient d'une variable booléenne choisie dans le Studio (`defaut_tag_id`) ou d'une alarme active sur la variable principale. Une qualité différente de `Good` ou une variable non liée donne l'état inconnu. Utilisez `currentColor` pour les parties actives.

Dans le SVG, `sym-marche` apparaît en marche, `sym-arret` à l'arrêt ou en état inconnu, et `sym-defaut` apparaît en défaut avec un clignotement à 1 Hz. `sym-flux` anime le trait pointillé (`stroke-dasharray`) en marche, `sym-vibre` vibre légèrement et `sym-rotatable` tourne. `sym-niveau` reçoit un remplissage vertical compris entre 0 et 100 % par `--apex-niveau` ; les propriétés `min` et `max` du widget règlent l'échelle (0 et 100 par défaut). Placez `style="transform-origin:Xpx Ypx;"` sur `sym-niveau` et `sym-rotatable`, au bas de la zone remplie pour le niveau.

Un élément porte au plus une de ces classes, sans imbrication entre elles, sauf `sym-rotatable` dans `sym-marche`. Sans état fourni par l'IHM, tous les éléments restent visibles, sans animation, et le niveau est à 100 %. La préférence de réduction du mouvement arrête les animations et laisse visibles les éléments d'état. Pour le corps métallique, utilisez des dégradés `linearGradient` ou `radialGradient` avec les couleurs `#0b1120`, `#111827`, `#1e293b`, `#273449`, `#334155`, `#475569`, `#64748b`, `#94a3b8`, `#cbd5e1`, `#e2e8f0`, `#ffffff`. Les identifiants internes commencent par l'identifiant du symbole suivi d'un tiret ; référencez un dégradé par `fill="url(#identifiant)"`. Évitez les filtres SVG, coûteux sur un synoptique chargé. Le détail de la convention est dans [`dist/CONVENTION-ETATS.md`](../dist/CONVENTION-ETATS.md).

Les modules du projet sont diffusés sous licence **GPL-3.0**.
