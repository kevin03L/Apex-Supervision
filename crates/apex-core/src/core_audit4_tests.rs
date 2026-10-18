#[tokio::test]
async fn audit4_sources_et_bornes() {
    let core = ScadaCore::new(None, &engine_options(false));
    let mut t = tag(1.0, 0.0);
    t.module_source = "inconnue".into();
    assert_eq!(core.upsert_tag("test", t.clone()).await.unwrap_err().http_status(), 400);
    core.register_tag(t, None);
    assert_eq!(core.write_tag("T", TagValue::Float(2.0)).await.unwrap_err().http_status(), 400);

    let mut t = tag(1.0, 0.0);
    t.consigne_min = Some(10.0);
    t.consigne_max = Some(20.0);
    core.upsert_tag("test", t.clone()).await.unwrap();
    assert_eq!(core.write_tag("T", TagValue::Float(21.0)).await.unwrap_err().http_status(), 400);
    assert!(core.write_tag("T", TagValue::Float(10.0)).await.is_ok());
    let bundle = core.export_project_bundle();
    let exported = bundle.tags.iter().find(|v| v.id == "T").unwrap();
    assert_eq!((exported.consigne_min, exported.consigne_max), (Some(10.0), Some(20.0)));
    let json = serde_json::to_value(exported).unwrap();
    assert_eq!(json["consigne_min"], 10.0);
    let mut old = json.as_object().unwrap().clone();
    old.remove("consigne_min"); old.remove("consigne_max");
    old.remove("fraicheur_max_s"); old.remove("ecriture_sans_retour");
    let restored: RegisteredTag = serde_json::from_value(serde_json::Value::Object(old)).unwrap();
    assert!(restored.consigne_min.is_none() && restored.fraicheur_max_s.is_none());
    assert!(!restored.ecriture_sans_retour);
    t.consigne_min = Some(21.0);
    assert_eq!(core.upsert_tag("test", t.clone()).await.unwrap_err().http_status(), 400);
    t.consigne_min = Some(f64::INFINITY);
    assert_eq!(core.upsert_tag("test", t).await.unwrap_err().http_status(), 400);
}

#[tokio::test]
async fn audit4_qualite_et_refus_diffuse() {
    let core = ScadaCore::new(None, &engine_options(false));
    let t = driver_tag("CMD", "DRV", "topic", true);
    core.drivers_config.insert("DRV".into(), driver("DRV", ProtocolType::Mqtt, vec![t.clone()]));
    core.register_tag(t, None);
    core.tags_state.get_mut("CMD").unwrap().quality = Quality::BadDisconnect;
    assert_eq!(core.write_tag("CMD", TagValue::Float(1.0)).await.unwrap_err().http_status(), 400);
    core.tags_registry.get_mut("CMD").unwrap().ecriture_sans_retour = true;
    let err = core.write_tag("CMD", TagValue::Float(1.0)).await.unwrap_err();
    assert!(!err.to_string().contains("Qualité invalide"));
    core.tags_registry.get_mut("CMD").unwrap().ecriture_sans_retour = false;
    core.tags_state.get_mut("CMD").unwrap().quality = Quality::Uncertain;
    let err = core.write_tag("CMD", TagValue::Float(1.0)).await.unwrap_err();
    assert!(!err.to_string().contains("Qualité invalide"));
    let mut rx = core.subscribe_write_rejected();
    core.record_driver_write_rejected("DRV", "CMD", "refus simulé");
    assert_eq!(rx.try_recv().unwrap(), ("CMD".into(), "refus simulé".into()));
}

#[tokio::test]
async fn audit4_fraicheur_et_republication() {
    let core = ScadaCore::new(None, &engine_options(false));
    let mut t = tag(1.0, 0.0);
    t.fraicheur_max_s = Some(1);
    core.register_tag(t, None);
    core.process_incoming_state(TagState { tag_id: "T".into(), value: TagValue::Float(7.0),
        quality: Quality::Good, timestamp: Utc::now() });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if core.tags_state.get("T").unwrap().quality == Quality::Uncertain { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.unwrap();
    core.process_incoming_state(TagState { tag_id: "T".into(), value: TagValue::Float(7.0),
        quality: Quality::Good, timestamp: Utc::now() });
    assert_eq!(core.tags_state.get("T").unwrap().quality, Quality::Good);
}

#[tokio::test]
async fn audit4_impulsion_reessaye_et_reste_active() {
    let core = ScadaCore::new(None, &engine_options(false));
    let mut t = driver_tag("P", "DRV", "topic", true);
    t.data_type = TagDataType::Bool;
    core.drivers_config.insert("DRV".into(), driver("DRV", ProtocolType::Mqtt, vec![t.clone()]));
    {
        let mut d = core.drivers_config.get_mut("DRV").unwrap();
        d.stats.running = true;
        d.stats.connected = true;
    }
    core.register_tag(t, None);
    let (tx, mut rx) = mpsc::channel(1);
    core.driver_writers.insert("DRV".into(), tx);
    core.pulse_tag("P").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(core.active_pulses.contains_key("P"));
    assert!(core.pulse_tag("P").await.unwrap_err().to_string().contains("en cours"));
    assert_eq!(rx.recv().await.unwrap().value, TagValue::Bool(true));
    let mut reset = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(reset.value, TagValue::Bool(false));
    reset.confirm();
    tokio::time::timeout(Duration::from_secs(2), async {
        while core.active_pulses.contains_key("P") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn audit4_impulsion_reessaye_apres_refus_driver() {
    let core = ScadaCore::new(None, &engine_options(false));
    let mut t = driver_tag("P", "DRV", "topic", true);
    t.data_type = TagDataType::Bool;
    core.drivers_config.insert("DRV".into(), driver("DRV", ProtocolType::Mqtt, vec![t.clone()]));
    {
        let mut d = core.drivers_config.get_mut("DRV").unwrap();
        d.stats.running = true;
        d.stats.connected = true;
    }
    core.register_tag(t, None);
    let (tx, mut rx) = mpsc::channel(2);
    core.driver_writers.insert("DRV".into(), tx);
    core.pulse_tag("P").await.unwrap();
    assert_eq!(rx.recv().await.unwrap().value, TagValue::Bool(true));
    let rejected = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
    assert_eq!(rejected.value, TagValue::Bool(false));
    drop(rejected);
    assert!(core.active_pulses.contains_key("P"));
    let mut retry = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(retry.value, TagValue::Bool(false));
    retry.confirm();
    tokio::time::timeout(Duration::from_secs(2), async {
        while core.active_pulses.contains_key("P") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn audit4_modules_reinstallation_import_libre_et_packs_historiques() {
    let core = ScadaCore::new(None, &engine_options(false));
    core.import_symbol_pack("admin", &test_module("test_pack-ancien")).await.unwrap();
    let result = core.import_symbol_pack("admin", &test_module("test_pack-nouveau")).await.unwrap();
    assert_eq!(result.removed, 1);
    assert!(result.conserves.is_empty());
    assert!(!core.symbols_library.contains_key("test_pack-ancien"));

    let imported = core.import_symbol_library("test", crate::models::ImportSymbolLibraryRequest {
        format: "scada_json".into(), library_name: "Essai libre".into(), category: "Essai".into(),
        raw_content: r#"[{"id":"symbole-libre","name":"Libre","svg_content":"<svg viewBox=\"0 0 10 10\"><circle cx=\"5\" cy=\"5\" r=\"4\"/></svg>"}]"#.into(),
        animate_rotation: false,
    }).await.unwrap();
    assert_eq!(imported[0].pack_id.as_deref(), Some("import-essai-libre"));
    assert!(core.installed_symbol_packs().iter().any(|p| p.id == "import-essai-libre"));
    for id in ["iso_10628_pid", "isa_5_1_instr", "iec_60617_cvc"] {
        assert!(ScadaCore::builtin_pack_symbols(id).iter().all(|s| s.pack_id.as_deref() == Some(id)
            && s.pack_version.as_deref() == Some("1.0.0")));
    }
    let mut legacy = ScadaCore::builtin_pack_symbols("iso_10628_pid")[0].clone();
    legacy.pack_id = None;
    core.symbols_library.insert(legacy.id.clone(), legacy.clone());
    core.install_builtin_symbol_pack("admin", "iso_10628_pid").await.unwrap();
    assert_eq!(core.symbols_library.get(&legacy.id).unwrap().pack_id.as_deref(), Some("iso_10628_pid"));
}

#[tokio::test]
async fn audit4_suppression_sql_echouee_garde_la_variable() {
    let pool = vague2_db::open(None).await.unwrap();
    let core = ScadaCore::new(Some(pool.clone()), &engine_options(false));
    let mut t = tag(1.0, 0.0);
    t.consigne_min = Some(5.0);
    t.fraicheur_max_s = Some(2);
    t.ecriture_sans_retour = true;
    core.upsert_tag("test", t).await.unwrap();
    let saved: String = sqlx::query_scalar("SELECT tag_json FROM tags_catalog WHERE id = 'T'")
        .fetch_one(&pool).await.unwrap();
    let saved: RegisteredTag = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved.consigne_min, Some(5.0));
    assert_eq!(saved.fraicheur_max_s, Some(2));
    assert!(saved.ecriture_sans_retour);
    sqlx::query("DROP TABLE tags_catalog").execute(&pool).await.unwrap();
    assert!(core.delete_tag("test", "T").await.is_err());
    assert!(core.tags_registry.contains_key("T"));
    assert!(core.tags_state.contains_key("T"));
}

#[tokio::test]
async fn audit4_module_conserve_symbole_utilise() {
    let core = ScadaCore::new(None, &engine_options(false));
    core.import_symbol_pack("admin", &test_module("test_pack-ancien")).await.unwrap();
    core.dashboards.insert("syn-test".into(), DashboardLayout {
        id: "syn-test".into(), name: "Essai".into(), category: "Essai".into(),
        description: String::new(), grid_size: 10,
        widgets: vec![DashboardWidget { id: "widget".into(), x: 0.0, y: 0.0,
            width: 100.0, height: 100.0, widget_type: WidgetType::CustomSvgSymbol,
            bound_tag_id: None, title: "Ancien".into(),
            properties: serde_json::json!({"symbol_id":"test_pack-ancien"}) }],
        canvas_width: None, canvas_height: None, updated_at: Utc::now(),
    });
    let result = core.import_symbol_pack("admin", &test_module("test_pack-nouveau")).await.unwrap();
    assert_eq!(result.removed, 0);
    assert_eq!(result.conserves, vec!["test_pack-ancien".to_string()]);
    assert!(core.symbols_library.contains_key("test_pack-ancien"));
}

#[tokio::test]
async fn audit4_module_reinstallation_supprime_en_base() {
    let pool = vague2_db::open(None).await.unwrap();
    let core = ScadaCore::new(Some(pool.clone()), &engine_options(false));
    core.import_symbol_pack("admin", &test_module("test_pack-ancien"))
        .await.unwrap();
    core.import_symbol_pack("admin", &test_module("test_pack-nouveau"))
        .await.unwrap();
    let old: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM symbol_libraries WHERE id = 'test_pack-ancien'")
        .fetch_one(&pool).await.unwrap();
    let new: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM symbol_libraries WHERE id = 'test_pack-nouveau'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!((old, new), (0, 1));
}
