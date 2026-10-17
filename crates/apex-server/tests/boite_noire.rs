//! Tests d'intégration « boîte noire » du serveur : le binaire `apex-scada` compilé est lancé
//! dans un dossier temporaire neuf (mode portable, simulation et mode sûr actifs, ports libres)
//! puis interrogé uniquement par HTTP et WebSocket, comme le feraient l'IHM et l'utilitaire.
//!
//! Aucun test ne touche à une base existante ni n'envoie de commande à un équipement : le seul
//! « automate » joint est l'automate Modbus embarqué de la simulation, sur la boucle locale,
//! et le mode sûr refuse toute consigne vers les drivers non simulés.

mod common;

use std::{net::TcpListener, time::Duration};

use common::{
    read_log_dir, request, run_to_exit, write_config, ws_connect, Server, Settings, TempDir, ADMIN,
    EXIT_TIMEOUT, VISU,
};
use serde_json::json;

/// Tag d'un driver réel (Modbus TCP, groupe froid de la démonstration), inscriptible.
const MODBUS_SETPOINT: &str = "MODBUS.GF01.Consigne_Eau_Glacee";
/// Tag inscriptible du simulateur GTB (driver simulé, pilotable en mode sûr).
const SIMULATED_SETPOINT: &str = "GTB.CTA01.Consigne_Temp";

// ============================================================================
// SANTÉ
// ============================================================================

#[test]
fn health_est_public_et_detaille_en_boucle_locale() {
    let mut srv = Server::start("health");
    let r = srv.call("GET", "/api/health", None, None);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));

    let body = r.json();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["uptime_seconds"].is_u64());

    // Sans le jeton de contrôle local, aucun détail d'exécution, même en boucle locale (proxy
    // inverse local sans en-tête de transfert, page web par DNS rebinding)
    assert!(
        body.get("runtime").is_none() && body.get("started_at").is_none(),
        "détails d'exécution publiés sans jeton : {body}"
    );
    let r = srv.call("GET", "/api/health", Some("jeton-invente"), None);
    assert!(r.json().get("runtime").is_none(), "jeton inconnu accepté");

    // Détails d'exécution réservés au porteur du jeton local, en boucle locale (utilitaire)
    let local = srv.control_token();
    let body = srv.call("GET", "/api/health", Some(&local), None).json();
    let rt = &body["runtime"];
    assert!(rt.is_object(), "runtime absent : {body}");
    assert_eq!(rt["pid"], srv.pid());
    assert_eq!(rt["run_mode"], "manual");
    assert_eq!(rt["safe_mode"], true);
    assert_eq!(rt["simulation_enabled"], true);
    assert_eq!(rt["instance_name"], "Essai d'intégration");
    assert_eq!(
        rt["config_file"].as_str(),
        Some(srv.config.display().to_string().as_str())
    );
    assert!(rt["config_fingerprint"]
        .as_str()
        .is_some_and(|f| f.len() == 16));
    assert!(rt["data_dir"]
        .as_str()
        .is_some_and(|d| d.starts_with(&srv.dir().display().to_string())));
    assert!(body["started_at"].is_string());
    assert!(body["total_drivers"].as_u64().is_some_and(|n| n >= 2));

    // Derrière un proxy inverse installé sur le poste, les requêtes arrivent aussi de la boucle
    // locale : un en-tête de proxy suffit à retirer les détails d'exécution.
    for header in [
        ("X-Forwarded-For", "203.0.113.7"),
        ("Forwarded", "for=203.0.113.7;proto=https"),
        ("X-Real-IP", "203.0.113.7"),
    ] {
        let r = srv.call_with_headers("GET", "/api/health", Some(&local), &[header]);
        assert_eq!(r.status, 200, "{}", r.text());
        let body = r.json();
        assert_eq!(body["status"], "ok");
        assert!(
            body.get("runtime").is_none() && body.get("started_at").is_none(),
            "détails d'exécution publiés via le proxy ({}) : {body}",
            header.0
        );
    }

    // IHM : revalidée à chaque chargement, pour ne jamais garder un ancien fichier après une
    // mise à jour ; politique de sécurité du contenu présente
    for path in ["/", "/app.js", "/styles.css"] {
        let r = srv.call("GET", path, None, None);
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("cache-control"), Some("no-cache"), "{path}");
        assert!(r.header("content-security-policy").is_some(), "{path}");
    }

    // Une route inconnue de l'API ne doit pas passer pour un succès
    let r = srv.call("GET", "/api/inexistant", None, None);
    assert_ne!(r.status, 200);
}

// ============================================================================
// AUTHENTIFICATION
// ============================================================================

#[test]
fn authentification_jeton_obligatoire() {
    let srv = Server::start("auth");

    // API refusée sans jeton, ou avec un jeton inconnu
    for path in ["/api/tags", "/api/system/status", "/api/auth/me"] {
        let r = srv.call("GET", path, None, None);
        assert_eq!(r.status, 401, "{path} sans jeton : {}", r.text());
        assert_eq!(r.json()["error"], true);
    }
    let r = srv.call("GET", "/api/tags", Some("jeton-inconnu"), None);
    assert_eq!(r.status, 401);

    // Échec de connexion : mot de passe faux et compte inconnu, même réponse
    let bad = srv.call(
        "POST",
        "/api/auth/login",
        None,
        Some(&json!({ "username": "admin", "password": "mauvais" })),
    );
    assert_eq!(bad.status, 401, "{}", bad.text());
    assert!(bad.json()["token"].is_null());
    let unknown = srv.call(
        "POST",
        "/api/auth/login",
        None,
        Some(&json!({ "username": "inconnu", "password": "x" })),
    );
    assert_eq!(unknown.status, 401);
    assert_eq!(bad.json()["message"], unknown.json()["message"]);

    // Connexion de l'administrateur (compte d'usine)
    let r = srv.call(
        "POST",
        "/api/auth/login",
        None,
        Some(&json!({ "username": ADMIN.0, "password": ADMIN.1 })),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let body = r.json();
    let token = body["token"].as_str().expect("jeton").to_string();
    assert!(token.len() >= 32, "jeton trop court : {token}");
    assert_eq!(body["user"]["username"], "admin");
    assert_eq!(body["user"]["role"], "Admin");
    assert!(
        body["user"].get("password_hash").is_none(),
        "le hachage ne doit pas être publié"
    );

    let me = srv.call("GET", "/api/auth/me", Some(&token), None);
    assert_eq!(me.status, 200);
    assert_eq!(me.json()["username"], "admin");
    // Forme « Authorization: Bearer » acceptée aussi
    let r = srv.call("GET", "/api/tags", Some(&format!("Bearer {token}")), None);
    assert_eq!(r.status, 200);

    // Après déconnexion, le jeton n'est plus valable
    let r = srv.call("POST", "/api/auth/logout", Some(&token), None);
    assert_eq!(r.status, 200);
    let r = srv.call("GET", "/api/auth/me", Some(&token), None);
    assert_eq!(r.status, 401);
}

#[test]
fn verrouillage_resiste_aux_tentatives_concurrentes() {
    let srv = Server::start("verrouillage");
    let port = srv.port();

    // 30 tentatives réellement simultanées avec un mot de passe faux : au plus 5 vérifications
    // (401), toutes les autres sont refusées par le verrouillage (403) avant Argon2.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(30));
    let workers: Vec<_> = (0..30)
        .map(|_| {
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                request(
                    port,
                    "POST",
                    "/api/auth/login",
                    None,
                    Some(&json!({ "username": "visu", "password": "mauvais" })),
                )
                .expect("requête de connexion")
                .status
            })
        })
        .collect();
    let statuses: Vec<u16> = workers
        .into_iter()
        .map(|w| w.join().expect("fil de test"))
        .collect();
    let unauthorized = statuses.iter().filter(|s| **s == 401).count();
    let locked = statuses.iter().filter(|s| **s == 403).count();
    assert_eq!(unauthorized, 5, "{statuses:?}");
    assert_eq!(locked, 25, "{statuses:?}");

    // Même le bon mot de passe est refusé pendant le verrouillage
    let r = srv.call(
        "POST",
        "/api/auth/login",
        None,
        Some(&json!({ "username": VISU.0, "password": VISU.1 })),
    );
    assert_eq!(r.status, 403, "{}", r.text());
    // Les autres comptes ne sont pas touchés
    srv.login(ADMIN);
}

#[test]
fn reinitialisation_du_mot_de_passe_ferme_les_sessions() {
    let srv = Server::start("reinit-mdp");
    let admin = srv.login(ADMIN);
    let other_admin_session = srv.login(ADMIN);
    let visu = srv.login(VISU);
    assert_eq!(
        srv.call("GET", "/api/auth/me", Some(&visu), None).status,
        200
    );

    // L'administrateur réinitialise le mot de passe de visu (compte supposé compromis)
    let r = srv.call(
        "POST",
        "/api/users",
        Some(&admin),
        Some(&json!({
            "username": "visu",
            "display_name": "Poste Opérateur Visualisation",
            "role": "Viewer",
            "password": "Nouveau-visu-2026",
        })),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let r = srv.call("GET", "/api/auth/me", Some(&visu), None);
    assert_eq!(
        r.status,
        401,
        "session de visu toujours ouverte : {}",
        r.text()
    );
    srv.login(("visu", "Nouveau-visu-2026"));

    // Sur son propre compte, l'administrateur garde sa session courante, pas les autres
    let r = srv.call(
        "POST",
        "/api/users",
        Some(&admin),
        Some(&json!({
            "username": "admin",
            "display_name": "Administrateur Système SCADA",
            "role": "Admin",
            "password": "Nouveau-admin-2026",
        })),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        srv.call("GET", "/api/auth/me", Some(&admin), None).status,
        200
    );
    let r = srv.call("GET", "/api/auth/me", Some(&other_admin_session), None);
    assert_eq!(r.status, 401, "autre session admin toujours ouverte");

    // Une modification sans nouveau mot de passe ne ferme aucune session
    let visu = srv.login(("visu", "Nouveau-visu-2026"));
    let r = srv.call(
        "POST",
        "/api/users",
        Some(&admin),
        Some(&json!({
            "username": "visu",
            "display_name": "Poste de garde",
            "role": "Viewer",
            "department": "Garde",
        })),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        srv.call("GET", "/api/auth/me", Some(&visu), None).status,
        200
    );
}

// ============================================================================
// RBAC
// ============================================================================

#[test]
fn rbac_visualisation_refuse_les_actions_admin() {
    let srv = Server::start("rbac");
    let visu = srv.login(VISU);

    let me = srv.call("GET", "/api/auth/me", Some(&visu), None);
    assert_eq!(me.status, 200);
    assert_eq!(me.json()["role"], "Viewer");

    // Lecture autorisée
    assert_eq!(srv.call("GET", "/api/tags", Some(&visu), None).status, 200);

    // Actions réservées : refus 403, sans effet
    let r = srv.call("POST", "/api/system/shutdown", Some(&visu), None);
    assert_eq!(r.status, 403, "arrêt par visu : {}", r.text());
    let r = srv.call(
        "POST",
        "/api/users",
        Some(&visu),
        Some(&json!({
            "username": "intrus",
            "display_name": "Intrus",
            "role": "Admin",
            "password": "Intrus-2026-xyz",
        })),
    );
    assert_eq!(
        r.status,
        403,
        "création d'utilisateur par visu : {}",
        r.text()
    );
    assert_eq!(srv.call("GET", "/api/users", Some(&visu), None).status, 403);
    let r = srv.call(
        "POST",
        &format!("/api/tags/{SIMULATED_SETPOINT}/write"),
        Some(&visu),
        Some(&json!({ "value": 21.0 })),
    );
    assert_eq!(r.status, 403, "consigne par visu : {}", r.text());

    // Le serveur tourne toujours et l'utilisateur n'a pas été créé
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(srv.call("GET", "/api/health", None, None).status, 200);
    let admin = srv.login(ADMIN);
    let users = srv.call("GET", "/api/users", Some(&admin), None);
    assert_eq!(users.status, 200);
    let users = users.json();
    let names: Vec<&str> = users
        .as_array()
        .expect("liste des utilisateurs")
        .iter()
        .filter_map(|u| u["username"].as_str())
        .collect();
    assert!(!names.contains(&"intrus"), "{names:?}");
    assert!(names.contains(&"visu"), "{names:?}");
}

// ============================================================================
// MODE SÛR
// ============================================================================

#[test]
fn mode_sur_refuse_les_consignes_vers_un_driver_reel() {
    let srv = Server::start("mode-sur");
    let admin = srv.login(ADMIN);

    let tags = srv.call("GET", "/api/tags", Some(&admin), None).json();
    let modbus = tags
        .as_array()
        .expect("liste des tags")
        .iter()
        .find(|t| t["id"] == MODBUS_SETPOINT)
        .unwrap_or_else(|| panic!("tag {MODBUS_SETPOINT} absent de la démonstration"));
    assert_eq!(modbus["module_source"], "MODBUS_GF01");
    assert_eq!(modbus["writable"], true);

    let status = srv
        .call("GET", "/api/system/status", Some(&admin), None)
        .json();
    assert_eq!(status["safe_mode"], true);

    // Consigne vers le driver Modbus (non simulé) : refusée par le mode sûr
    let r = srv.call(
        "POST",
        &format!("/api/tags/{MODBUS_SETPOINT}/write"),
        Some(&admin),
        Some(&json!({ "value": 8.5, "operator_note": "test d'intégration" })),
    );
    assert_eq!(r.status, 403, "{}", r.text());
    let msg = r.json()["message"].as_str().unwrap_or_default().to_string();
    assert!(msg.contains("Mode sûr"), "{msg}");

    // Le jeton de contrôle local ne permet aucune consigne (il ne sert qu'à l'arrêt propre)
    let r = srv.call(
        "POST",
        &format!("/api/tags/{MODBUS_SETPOINT}/write"),
        Some(&srv.control_token()),
        Some(&json!({ "value": 9.0 })),
    );
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(r.text().contains("jeton de contrôle local"), "{}", r.text());

    // Un driver simulé reste pilotable : le refus est bien propre au mode sûr. Le simulateur
    // peut ne pas être encore connecté juste après le démarrage : on réessaie quelques secondes.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let r = srv.call(
            "POST",
            &format!("/api/tags/{SIMULATED_SETPOINT}/write"),
            Some(&admin),
            Some(&json!({ "value": 21.5 })),
        );
        if r.status == 200 {
            break;
        }
        let text = r.text();
        assert!(
            !text.contains("Mode sûr"),
            "driver simulé refusé par le mode sûr : {text}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "consigne simulée toujours refusée : {} {text}",
            r.status
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    assert!(
        read_log_dir(srv.dir()).contains("MODE SÛR actif"),
        "avertissement du mode sûr absent des journaux"
    );
}

// ============================================================================
// WEBSOCKET TEMPS RÉEL
// ============================================================================

#[test]
fn websocket_exige_un_jeton_puis_diffuse_les_donnees() {
    let srv = Server::start("websocket");

    // Sans jeton, ou avec un jeton inconnu : mise à niveau refusée (401)
    for path in ["/ws/realtime", "/ws/realtime?token=inconnu"] {
        match ws_connect(srv.port(), path).expect("connexion TCP") {
            Ok(_) => panic!("{path} : WebSocket accepté sans session valide"),
            Err(r) => assert_eq!(r.status, 401, "{path} : {}", r.text()),
        }
    }

    // Avec jeton administrateur : instantané complet, puis mises à jour temps réel
    let admin = srv.login(ADMIN);
    let mut ws = ws_connect(srv.port(), &format!("/ws/realtime?token={admin}"))
        .expect("connexion TCP")
        .unwrap_or_else(|r| panic!("WebSocket refusé : {} {}", r.status, r.text()));
    let first = ws
        .next_json(Duration::from_secs(15))
        .expect("lecture du premier message")
        .expect("connexion fermée avant l'instantané");
    assert_eq!(first["event"], "InitialSnapshot", "{first}");
    let data = &first["data"];
    assert!(data["tags"].as_array().is_some_and(|t| !t.is_empty()));
    assert!(data["drivers"].as_array().is_some_and(|d| d.len() >= 2));
    assert_eq!(data["status"]["safe_mode"], true);

    // Le simulateur GTB produit des changements de valeur : un lot doit arriver
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "aucune mise à jour TagUpdateBatch reçue");
        let msg = ws
            .next_json(left)
            .expect("lecture d'un message temps réel")
            .expect("connexion fermée par le serveur");
        if msg["event"] == "TagUpdateBatch" {
            let batch = msg["data"].as_array().expect("lot de mises à jour");
            assert!(!batch.is_empty());
            assert!(batch[0]["tag_id"].is_string(), "{msg}");
            break;
        }
    }
    ws.close();

    // Profil Visualisation : instantané filtré (ni drivers, ni flux, ni plannings)
    let visu = srv.login(VISU);
    let mut ws = ws_connect(srv.port(), &format!("/ws/realtime?token={visu}"))
        .expect("connexion TCP")
        .unwrap_or_else(|r| panic!("WebSocket visu refusé : {} {}", r.status, r.text()));
    let first = ws
        .next_json(Duration::from_secs(15))
        .expect("lecture")
        .expect("instantané");
    assert_eq!(first["event"], "InitialSnapshot");
    assert!(first["data"]["tags"]
        .as_array()
        .is_some_and(|t| !t.is_empty()));
    assert_eq!(first["data"]["drivers"], json!([]));
    assert_eq!(first["data"]["flows"], json!([]));
    assert_eq!(first["data"]["schedules"], json!([]));
    ws.close();
}

// ============================================================================
// CODES DE SORTIE
// ============================================================================

#[test]
fn code_3_configuration_invalide() {
    // Configuration valide en tout point, sauf le port web (0 : refusé par la validation)
    let base = Settings::new();
    let port_nul = base
        .render()
        .replace(&format!("port = {}\n", base.port), "port = 0\n");
    assert!(port_nul.contains("port = 0\n"));
    let cases = [
        ("port-nul", port_nul),
        // Fichier TOML illisible
        (
            "syntaxe",
            "config_version = 1\n[server\nport = 1\n".to_string(),
        ),
        // Paramètre inconnu (faute de frappe) : refusé plutôt qu'ignoré
        (
            "champ-inconnu",
            "config_version = 1\n[server]\nportt = 4000\n".to_string(),
        ),
        // Format plus récent que le moteur
        ("version", "config_version = 99\n".to_string()),
    ];
    for (name, content) in cases {
        let dir = TempDir::new(&format!("code3-{name}"));
        let config = write_config(&dir.path, &content);
        let out = run_to_exit(&dir.path, &config, "engine");
        assert_eq!(
            out.code,
            Some(3),
            "{name} : code inattendu\n{}\n{}",
            out.stdout,
            out.stderr
        );
        assert!(
            out.stderr.contains("Démarrage impossible"),
            "{name} : {}",
            out.stderr
        );
        // L'échec survient avant toute base, verrou ou communication
        assert!(
            !dir.path.join("data").join("apex-scada.db").exists(),
            "{name} : base créée malgré la configuration invalide"
        );
        assert!(
            dir.path
                .join("logs")
                .join("apex-scada-startup.log")
                .is_file(),
            "{name} : erreur de démarrage non consignée"
        );
    }
}

#[test]
fn code_4_deuxieme_instance_meme_dossier() {
    let srv = Server::start("code4");
    let out = run_to_exit(srv.dir(), &srv.config, "second");
    assert_eq!(out.code, Some(4), "{}\n{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("autre instance"),
        "message attendu : {}",
        out.stderr
    );
    // La première instance n'est pas perturbée
    let r = srv.call("GET", "/api/health", None, None);
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["status"], "ok");
}

#[test]
fn code_5_port_occupe() {
    // Le port est tenu par le test pendant toute l'exécution du moteur
    let holder = TcpListener::bind("127.0.0.1:0").expect("réservation d'un port");
    let mut settings = Settings::new();
    settings.port = holder.local_addr().expect("adresse").port();

    let dir = TempDir::new("code5");
    let config = write_config(&dir.path, &settings.render());
    let out = run_to_exit(&dir.path, &config, "engine");
    drop(holder);
    assert_eq!(out.code, Some(5), "{}\n{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("impossible d'écouter"),
        "message attendu : {}",
        out.stderr
    );
    // Le port est contrôlé avant l'ouverture de la base et le démarrage des drivers
    assert!(!dir.path.join("data").join("apex-scada.db").exists());
}

// ============================================================================
// ARRÊT PROPRE
// ============================================================================

#[test]
fn jeton_de_controle_local_reserve_a_l_arret_propre() {
    let srv = Server::start("jeton-local");
    let token = srv.control_token();

    // Lecture de l'état : autorisée (utilitaire desktop)
    let r = srv.call("GET", "/api/system/status", Some(&token), None);
    assert_eq!(r.status, 200, "{}", r.text());

    // Aucune autre route : ni administration, ni lecture des données, ni WebSocket
    for (method, path, body) in [
        ("GET", "/api/users", None),
        ("GET", "/api/auth/me", None),
        ("GET", "/api/tags", None),
        ("GET", "/api/system/export", None),
        (
            "POST",
            "/api/users",
            Some(json!({
                "username": "intrus",
                "display_name": "Intrus",
                "role": "Admin",
                "password": "Intrus-2026-xyz",
            })),
        ),
        ("POST", "/api/system/restart", Some(json!({}))),
    ] {
        let r = srv.call(method, path, Some(&token), body.as_ref());
        assert_eq!(
            r.status,
            403,
            "{method} {path} avec le jeton local : {}",
            r.text()
        );
    }
    match ws_connect(srv.port(), &format!("/ws/realtime?token={token}")).expect("connexion TCP") {
        Ok(_) => panic!("WebSocket accepté avec le jeton de contrôle local"),
        Err(r) => assert_eq!(r.status, 403, "{}", r.text()),
    }

    // Via un proxy inverse (en-tête de transfert) : le jeton n'est plus reconnu du tout
    for header in [
        ("X-Forwarded-For", "203.0.113.7"),
        ("Forwarded", "for=203.0.113.7"),
        ("X-Real-IP", "203.0.113.7"),
    ] {
        let r = srv.call_with_headers("GET", "/api/system/status", Some(&token), &[header]);
        assert_eq!(
            r.status,
            401,
            "jeton local accepté via {} : {}",
            header.0,
            r.text()
        );
        let r = srv.call_with_headers("POST", "/api/system/shutdown", Some(&token), &[header]);
        assert_eq!(
            r.status,
            401,
            "arrêt accepté via {} : {}",
            header.0,
            r.text()
        );
    }

    // L'utilisateur n'a pas été créé et le moteur tourne toujours
    let admin = srv.login(ADMIN);
    let users = srv.call("GET", "/api/users", Some(&admin), None).json();
    assert!(
        !users.to_string().contains("intrus"),
        "compte créé avec le jeton local : {users}"
    );
    assert_eq!(srv.call("GET", "/api/health", None, None).status, 200);
}

#[test]
fn arret_propre_par_jeton_de_controle_local() {
    let mut srv = Server::start("arret");
    let token_file = srv.dir().join("run").join("control.token");
    assert!(token_file.is_file(), "jeton de contrôle non publié");
    let token = srv.control_token();

    // Sans jeton : refusé, le moteur continue
    let r = srv.call("POST", "/api/system/shutdown", None, None);
    assert_eq!(r.status, 401);
    assert_eq!(srv.call("GET", "/api/health", None, None).status, 200);

    // L'arrêt ne dépend pas du compte « admin » : un second administrateur le désactive
    let admin = srv.login(ADMIN);
    let r = srv.call(
        "POST",
        "/api/users",
        Some(&admin),
        Some(&json!({
            "username": "admin2",
            "display_name": "Second administrateur",
            "role": "Admin",
            "password": "Admin2-2026-xyz",
        })),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let admin2 = srv.login(("admin2", "Admin2-2026-xyz"));
    let r = srv.call(
        "POST",
        "/api/users",
        Some(&admin2),
        Some(&json!({
            "username": "admin",
            "display_name": "Administrateur Système SCADA",
            "role": "Admin",
            "enabled": false,
        })),
    );
    assert_eq!(r.status, 200, "{}", r.text());

    // Jeton de contrôle local (boucle locale) : arrêt accepté
    let r = srv.call("POST", "/api/system/shutdown", Some(&token), None);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["status"], "shutting_down");

    let status = srv
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("le moteur ne s'est pas arrêté\n{}", srv.logs()));
    assert_eq!(status.code(), Some(0), "{}", srv.logs());

    let logs = srv.logs();
    assert!(logs.contains("arrêté proprement"), "{logs}");
    assert!(
        read_log_dir(srv.dir()).contains("arrêté proprement"),
        "message d'arrêt absent du journal fichier"
    );
    assert!(!token_file.exists(), "jeton de contrôle non supprimé");
    // Le port est libéré : plus aucune réponse
    assert!(request(srv.port(), "GET", "/api/health", None, None).is_err());

    // Après relance, le journal d'audit attribue l'arrêt à l'acteur interne « Utilitaire local »
    srv.restart();
    let admin2 = srv.login(("admin2", "Admin2-2026-xyz"));
    let audit = srv.call("GET", "/api/audit?limit=200", Some(&admin2), None);
    assert_eq!(audit.status, 200, "{}", audit.text());
    let audit = audit.json();
    let shutdown = audit
        .as_array()
        .expect("journal d'audit")
        .iter()
        .find(|e| e["action"] == "SHUTDOWN")
        .unwrap_or_else(|| panic!("arrêt absent du journal d'audit : {audit}"));
    assert_eq!(shutdown["actor"], "Utilitaire local", "{shutdown}");
}
