// Inclus dans le module de tests de core pour la vague 3.
#[tokio::test]
async fn vague3_pulse_tag_suppression_variable_pendant_impulsion_ne_boucle_pas_infini() {
    let core = ScadaCore::new(None, &engine_options(false));
    let mut t = tag(1.0, 0.0);
    t.id = "INT.PulseTemp".into();
    t.data_type = TagDataType::Bool;
    t.writable = true;
    core.register_tag(t, Some(TagValue::Bool(false)));

    // Lance l'impulsion
    let core_c = Arc::clone(&core);
    tokio::spawn(async move {
        let _ = core_c.pulse_tag("INT.PulseTemp").await;
    });

    // Attendre que l'impulsion démarre (active_pulses contient le tag)
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(core.active_pulses.contains_key("INT.PulseTemp"));

    // Supprime la variable pendant l'impulsion
    core.delete_tag("admin", "INT.PulseTemp").await.unwrap();

    // Attendre la fin du délai d'impulsion (1s) + court répit
    tokio::time::sleep(Duration::from_millis(1300)).await;

    // La tâche asynchrone ne doit pas boucler à l'infini : active_pulses doit être nettoyé
    assert!(
        !core.active_pulses.contains_key("INT.PulseTemp"),
        "active_pulses n'a pas été nettoyé : fuite de tâche ou boucle infinie"
    );
}

#[tokio::test]
async fn vague3_logout_et_revocation_casse_insensible_bearer() {
    let core = ScadaCore::new(None, &engine_options(false));
    let username = "sup_test";
    core.users.insert(
        username.into(),
        UserAccount {
            username: username.into(),
            display_name: "Sup".into(),
            role: UserRole::Supervisor,
            password_hash: "dummy".into(),
            department: "Exploitation".into(),
            enabled: true,
            created_at: Utc::now(),
            last_login_at: None,
        },
    );

    let token = "token_secret_123";
    let key = security::session_key(token);
    core.sessions.insert(
        key.clone(),
        Session {
            username: username.into(),
            created_at: Utc::now(),
            last_seen: Utc::now(),
        },
    );

    // 1. Résolution de session avec "bearer <token>" (minuscules)
    assert!(core.resolve_user(Some("bearer token_secret_123")).is_some());

    // 2. Déconnexion avec "bearer <token>" (minuscules)
    core.logout("bearer token_secret_123").await;

    // La session doit avoir été supprimée
    assert!(
        core.sessions.get(&key).is_none(),
        "La session n'a pas été déconnectée avec 'bearer ' en minuscules"
    );
    assert!(core.resolve_user(Some("bearer token_secret_123")).is_none());

    // 3. Test de revoke_user_sessions_except avec keep en minuscules
    let token1 = "token_keep_1";
    let token2 = "token_drop_2";
    let key1 = security::session_key(token1);
    let key2 = security::session_key(token2);
    core.sessions.insert(
        key1.clone(),
        Session {
            username: username.into(),
            created_at: Utc::now(),
            last_seen: Utc::now(),
        },
    );
    core.sessions.insert(
        key2.clone(),
        Session {
            username: username.into(),
            created_at: Utc::now(),
            last_seen: Utc::now(),
        },
    );

    core.revoke_user_sessions_except(username, Some("bearer token_keep_1")).await;
    assert!(
        core.sessions.contains_key(&key1),
        "La session active dans keep a été révoquée par erreur"
    );
    assert!(
        !core.sessions.contains_key(&key2),
        "La session secondaire n'a pas été révoquée"
    );
}
