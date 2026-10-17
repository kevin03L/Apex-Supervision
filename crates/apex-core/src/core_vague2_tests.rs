#[tokio::test]
async fn vague2_initialisation_explicite_interne_et_equipement() {
    let core=ScadaCore::new(None,&engine_options(false));
    let t=tag(1.0,0.0);
    assert_eq!(core.register_tag(t.clone(),Some(TagValue::Float(21.0))).state.unwrap().quality,Quality::Good);
    let mut equipment=t.clone(); equipment.id="E".into(); equipment.module_source="DRV".into();
    core.drivers_config.insert("DRV".into(),driver("DRV",ProtocolType::ModbusTcp,vec![]));
    assert_eq!(core.register_tag(equipment,Some(TagValue::Float(21.0))).state.unwrap().quality,Quality::Uncertain);
    let mut synthetic=t; synthetic.id="S".into();
    assert_eq!(core.register_tag(synthetic,None).state.unwrap().quality,Quality::Uncertain);
}

#[tokio::test]
async fn vague2_restauration_memoire_historisee() {
    let pool=vague2_db::open(None).await.unwrap();
    let core=ScadaCore::new(Some(pool.clone()),&engine_options(false));
    core.register_tag(tag(1.0,0.0),None);
    sqlx::query("INSERT INTO tag_history (tag_id,value_json,quality,recorded_at) VALUES ('T','21.5','Good','2026-10-17T00:00:00Z')").execute(&pool).await.unwrap();
    core.restore_internal_values(&pool).await;
    assert_eq!(core.tags_state.get("T").unwrap().value,TagValue::Float(21.5));
    assert_eq!(core.tags_state.get("T").unwrap().quality,Quality::Good);
    core.shutdown();core.drain_sqlite_writers().await;pool.close().await;
}

#[tokio::test]
async fn vague2_desescalade_apres_acquittement_diffusee() {
    let core=ScadaCore::new(None,&engine_options(false));
    let mut t=tag(1.0,0.0);t.alarm_config.severity=AlarmSeverity::High;core.register_tag(t,None);
    core.apply_alarm_transition("T","T","T",Some(("Défaut".into(),AlarmSeverity::Critical)),Some(AlarmLevel::HighHigh),TagValue::Float(30.0));
    core.apply_alarm_transition("T","T","T",Some(("Défaut".into(),AlarmSeverity::High)),Some(AlarmLevel::High),TagValue::Float(20.0));
    core.acknowledge_alarm("T","test",None).unwrap();
    let mut rx=core.subscribe_alarms();
    core.apply_alarm_transition("T","T","T",Some(("Défaut".into(),AlarmSeverity::High)),Some(AlarmLevel::High),TagValue::Float(20.0));
    assert!(rx.try_recv().is_ok());
    assert_eq!(core.alarms.get("T").unwrap().severity,AlarmSeverity::High);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vague2_planning_refus_immediat_non_ecrase() {
    let core=ScadaCore::new(None,&engine_options(false));core.schedules.clear();
    let t=driver_tag("T","DRV","cmd",true);
    core.register_tag(t,None);
    let mut drv=driver("DRV",ProtocolType::Mqtt,vec![]);drv.stats.running=true;drv.stats.connected=true;
    core.drivers_config.insert("DRV".into(),drv);
    let (tx,mut rx)=mpsc::channel(1);core.driver_writers.insert("DRV".into(),tx);
    core.schedules.insert("jour".into(),ScheduleRule {id:"jour".into(),name:"Jour".into(),target_tag_id:"T".into(),start_time:"00:00".into(),end_time:"23:59".into(),full_day:true,active_value:TagValue::Float(1.0),inactive_value:TagValue::Float(0.0),enabled:true,days:all_week_days(),last_applied_state:None});
    ScadaCore::spawn_scheduler_worker(Arc::clone(&core));
    let cmd=tokio::time::timeout(Duration::from_secs(2),rx.recv()).await.unwrap().unwrap();
    core.record_driver_write_rejected("DRV",&cmd.tag_id,"Refus immédiat simulé");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(core.schedules.get("jour").unwrap().last_applied_state,None);
    core.shutdown();
}

#[tokio::test]
async fn vague2_ingestion_non_finie_conserve_derniere_valeur() {
    let core=ScadaCore::new(None,&engine_options(false));
    core.register_tag(tag(1.0,0.0),Some(TagValue::Float(12.0)));
    for v in [f64::NAN,f64::INFINITY,f64::NEG_INFINITY] {
        core.process_incoming_state(TagState {tag_id:"T".into(),value:TagValue::Float(v),quality:Quality::Good,timestamp:Utc::now()});
        let state=core.tags_state.get("T").unwrap();
        assert_eq!(state.quality,Quality::BadDisconnect);
        assert_eq!(state.value,TagValue::Float(12.0));
    }
    core.tags_registry.get_mut("T").unwrap().scale_factor=f64::MAX;
    core.process_incoming_state(TagState {tag_id:"T".into(),value:TagValue::Float(10.0),quality:Quality::Good,timestamp:Utc::now()});
    assert_eq!(core.tags_state.get("T").unwrap().value,TagValue::Float(12.0));
    core.tags_registry.get_mut("T").unwrap().data_type=TagDataType::Int;
    core.process_incoming_state(TagState {tag_id:"T".into(),value:TagValue::Int(10),quality:Quality::Good,timestamp:Utc::now()});
    assert_eq!(core.tags_state.get("T").unwrap().quality,Quality::BadDisconnect);
}

#[tokio::test]
async fn vague2_validation_echelle_et_planning() {
    let core=ScadaCore::new(None,&engine_options(false));
    for (offset,band) in [(f64::NAN,0.0),(f64::INFINITY,0.0),(0.0,-1.0),(0.0,f64::NAN)] {
        let mut t=tag(1.0,offset);t.deadband=band;
        assert!(core.upsert_tag("test",t).await.is_err());
    }
    core.register_tag(tag(1.0,0.0),None);
    let rule=ScheduleRule {id:"essai".into(),name:"Essai".into(),target_tag_id:"T".into(),start_time:"08:00".into(),end_time:"18:00".into(), full_day: false,active_value:TagValue::Text("erreur".into()),inactive_value:TagValue::Float(0.0),enabled:true,days:vec![1],last_applied_state:None};
    assert!(core.save_schedule("test",rule.clone()).await.is_err());
    let mut rule=rule; rule.active_value=TagValue::Float(1.0); rule.inactive_value=TagValue::Float(f64::INFINITY);
    assert!(core.save_schedule("test",rule).await.is_err());
}

#[tokio::test]
async fn vague2_alarme_reaggravee_reveille_et_reacquitte() {
    let core=ScadaCore::new(None,&engine_options(false));
    let mut t=tag(1.0,0.0);t.alarm_config.severity=AlarmSeverity::High;
    core.register_tag(t,None);
    let transition=|level,sev| core.apply_alarm_transition("T","T","T",Some(("Défaut".into(),sev)),Some(level),TagValue::Float(20.0));
    transition(AlarmLevel::HighHigh,AlarmSeverity::Critical);
    transition(AlarmLevel::High,AlarmSeverity::High);
    core.acknowledge_alarm("T","test",None).unwrap();
    core.shelve_alarm("T","test",10,None).unwrap();
    transition(AlarmLevel::HighHigh,AlarmSeverity::Critical);
    let a=core.alarms.get("T").unwrap();
    assert_eq!(a.state,AlarmLifecycleState::ActiveUnack);
    assert!(a.shelved_until.is_none()); assert!(a.shelved_by.is_none()); drop(a);
    core.acknowledge_alarm("T","test",None).unwrap();
    let mut rx=core.subscribe_alarms();
    transition(AlarmLevel::High,AlarmSeverity::High);
    assert!(rx.try_recv().is_ok());
    assert_eq!(core.alarms.get("T").unwrap().severity,AlarmSeverity::High);
    core.apply_alarm_transition("T","T","T",None,None,TagValue::Float(0.0));
    assert!(core.shelve_alarm("T","test",10,None).is_err());
}

#[tokio::test]
async fn vague2_impulsion_extinction_sans_reponse_http() {
    let core=ScadaCore::new(None,&engine_options(false));
    let mut t=tag(1.0,0.0);t.data_type=TagDataType::Bool;
    core.register_tag(t,Some(TagValue::Bool(false)));
    core.pulse_tag("T").await.unwrap();
    assert_eq!(core.tags_state.get("T").unwrap().value,TagValue::Bool(true));
    assert!(core.pulse_tag("T").await.is_err());
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(core.tags_state.get("T").unwrap().value,TagValue::Bool(false));
}

#[tokio::test]
async fn vague2_import_memoires_francaises() {
    let core=ScadaCore::new(None,&engine_options(false));
    let mut bundle=core.export_project_bundle();
    let mut t=tag(1.0,0.0); t.module_source="INTERNE".into();
    bundle.tags=vec![t];bundle.drivers.clear();
    core.import_project_bundle("test",bundle).await.unwrap();
    assert!(core.tags_registry.contains_key("T"));
}

#[tokio::test]
async fn vague2_drainage_ingestion_historique_et_journal() {
    let pool=vague2_db::open(None).await.unwrap();
    let core=ScadaCore::new(Some(pool.clone()),&engine_options(false));
    let mut t=tag(1.0,0.0);t.historize=true;core.register_tag(t,None);
    for i in 1..=50 {
        core.ingest_tx.send(TagState {tag_id:"T".into(),value:TagValue::Float(i as f64),quality:Quality::Good,timestamp:Utc::now()}).await.unwrap();
        core.record_alarm_journal("T","High","ESSAI","Essai de drainage",None);
    }
    core.shutdown();core.drain_sqlite_writers().await;
    let n:i64=sqlx::query_scalar("SELECT COUNT(*) FROM tag_history WHERE tag_id='T'").fetch_one(&pool).await.unwrap();
    assert_eq!(n,50);
    let n:i64=sqlx::query_scalar("SELECT COUNT(*) FROM alarms_journal WHERE tag_id='T'").fetch_one(&pool).await.unwrap();
    assert_eq!(n,50);pool.close().await;
}

async fn exec_vague2_suppression_utilisateur_persistance_concurrente() {
    let pool=vague2_db::open(None).await.unwrap();
    let core=ScadaCore::new(Some(pool.clone()),&engine_options(false));
    let mut user=factory_admin_account();user.username="supprime".into();
    core.users.insert(user.username.clone(),user.clone());core.persist_user(&user).await;
    let lock=core.users_persist_lock.lock().await;
    let deleting=Arc::clone(&core);
    let deletion=tokio::spawn(async move { deleting.delete_user("admin","supprime").await.unwrap(); });
    // Laisser la suppression entrer dans la file FIFO du mutex avant la persistance.
    tokio::task::yield_now().await;
    let c=Arc::clone(&core);let stale=user.clone();
    let persistence=tokio::spawn(async move { c.persist_user(&stale).await; });
    tokio::task::yield_now().await;
    drop(lock);deletion.await.unwrap();persistence.await.unwrap();
    core.persist_user(&user).await;
    let n:i64=sqlx::query_scalar("SELECT COUNT(*) FROM users_accounts WHERE username='supprime'").fetch_one(&pool).await.unwrap();
    assert_eq!(n,0);
    core.shutdown();core.drain_sqlite_writers().await;
    let restored=load_json_column(&pool,"SELECT user_json AS js FROM users_accounts").await;
    assert!(!restored.iter().any(|s| s.contains("supprime")));pool.close().await;
}

#[tokio::test]
async fn vague2_suppression_utilisateur_persistance_concurrente() {
    exec_vague2_suppression_utilisateur_persistance_concurrente().await;
}

#[tokio::test]
async fn vague2_planning_journee_complete_explicite() {
    let core=ScadaCore::new(None,&engine_options(false));
    core.register_tag(tag(1.0,0.0),None);
    let mut rule=ScheduleRule {id:"journee".into(),name:"Journée".into(),target_tag_id:"T".into(),start_time:"00:00".into(),end_time:"00:00".into(),full_day:false,active_value:TagValue::Float(1.0),inactive_value:TagValue::Float(0.0),enabled:true,days:vec![1],last_applied_state:None};
    assert!(!rule.is_active_at(1,"12:00"));
    rule.full_day=true;
    let saved=core.save_schedule("test",rule).await.unwrap();
    for time in ["00:00","12:00","23:59"] { assert!(saved.is_active_at(1,time)); }
    assert!(!saved.is_active_at(2,"00:00"));
}

#[tokio::test]
async fn vague2_mode_sur_defense_en_profondeur_driver() {
    let mut opts = engine_options(false);
    opts.safe_mode = true;
    let core = ScadaCore::new(None, &opts);
    let mut drv = driver("DRV_SAFE", ProtocolType::ModbusTcp, vec![]);
    drv.stats.running = true;
    drv.stats.connected = true;
    core.drivers_config.insert("DRV_SAFE".into(), drv);

    let cmd = WriteCommand {
        tag_id: "TAG1".into(),
        protocol_address: "HR:0".into(),
        value: TagValue::Float(10.0),
    };

    if core.safe_mode {
        core.record_driver_write_rejected("DRV_SAFE", &cmd.tag_id, "Mode sûr actif côté driver");
    }

    let drv_updated = core.drivers_config.get("DRV_SAFE").unwrap();
    assert_eq!(drv_updated.stats.frames_err, 1);
    assert!(drv_updated
        .stats
        .last_write_rejected
        .as_ref()
        .unwrap()
        .contains("Mode sûr actif"));
}
