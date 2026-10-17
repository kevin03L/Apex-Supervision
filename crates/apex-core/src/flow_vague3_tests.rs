// Inclus dans le module de tests de flow_engine pour la vague 3.
#[tokio::test]
async fn vague3_rhai_suppression_injection_consigne_en_dur() {
    let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
    core.register_tag(internal_tag("SORTIE", 0.0), None);
    let engine = shared_engine(&core);

    let direct_flow = |kind| FlowGraph {
        id: "f_rhai".into(),
        name: "Rhai Test".into(),
        description: String::new(),
        enabled: true,
        nodes: vec![
            node("in", FlowNodeKind::TagInput { tag_id: "A".into() }),
            node("calc", kind),
            node("out", FlowNodeKind::TagOutput { target_tag_id: "SORTIE".into() }),
        ],
        edges: vec![edge("e1", "in", "calc"), edge("e2", "calc", "out")],
    };

    // 1. Un script qui tente d'accéder à la variable `consigne` échoue (plus d'injection globale)
    let flow_bare = direct_flow(FlowNodeKind::RhaiScript {
        script: "consigne + 1.0".into(),
    });
    run_flow_from_entry_node(&core, &flow_bare, &flow_bare.nodes[0], &input("A", 1.0), &engine).await;
    assert!(core.flow_runtime.last_outputs.is_empty());

    // 2. Un script accédant via tag("GTB.CTA01.Consigne_Temp") échoue si la variable est absente ou dégradée
    let flow_tag = direct_flow(FlowNodeKind::RhaiScript {
        script: "let c = tag(\"GTB.CTA01.Consigne_Temp\"); c + 1.0".into(),
    });
    // Variable absente : aucun repli vers 21.0
    run_flow_from_entry_node(&core, &flow_tag, &flow_tag.nodes[0], &input("A", 1.0), &engine).await;
    assert!(core.flow_runtime.last_outputs.is_empty());

    // Variable présente mais BadDisconnect : échec
    let c_tag = internal_tag("GTB.CTA01.Consigne_Temp", 0.0);
    core.register_tag(c_tag, Some(TagValue::Float(21.0)));
    core.tags_state.get_mut("GTB.CTA01.Consigne_Temp").unwrap().quality = Quality::BadDisconnect;
    run_flow_from_entry_node(&core, &flow_tag, &flow_tag.nodes[0], &input("A", 1.0), &engine).await;
    assert!(core.flow_runtime.last_outputs.is_empty());

    // Variable Good : exécution réussie
    core.tags_state.get_mut("GTB.CTA01.Consigne_Temp").unwrap().quality = Quality::Good;
    core.tags_state.get_mut("GTB.CTA01.Consigne_Temp").unwrap().value = TagValue::Float(25.0);
    run_flow_from_entry_node(&core, &flow_tag, &flow_tag.nodes[0], &input("A", 1.0), &engine).await;
    assert_eq!(core.tags_state.get("SORTIE").unwrap().value, TagValue::Float(26.0));
}

#[tokio::test]
async fn vague3_pid_et_math_formule_rejet_nan_inf_sans_empoisonnement() {
    let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
    core.register_tag(internal_tag("SORTIE", 0.0), Some(TagValue::Float(0.0)));
    core.register_tag(internal_tag("SP", 0.0), Some(TagValue::Float(50.0)));
    let engine = shared_engine(&core);

    // 1. MathFormula rejetant non-finis
    let flow_math_nan = chain(FlowNodeKind::MathFormula { factor: f64::NAN, offset: 0.0 });
    run_flow_from_entry_node(&core, &flow_math_nan, &flow_math_nan.nodes[0], &input("A", 10.0), &engine).await;
    assert!(core.flow_runtime.last_outputs.is_empty());

    // 2. RhaiScript produisant NaN ou Inf
    for script in ["0.0 / 0.0", "1.0 / 0.0", "-1.0 / 0.0"] {
        let flow_rhai = chain(FlowNodeKind::RhaiScript { script: script.into() });
        run_flow_from_entry_node(&core, &flow_rhai, &flow_rhai.nodes[0], &input("A", 1.0), &engine).await;
        assert!(core.flow_runtime.last_outputs.is_empty(), "Rhai script {script} n'a pas été bloqué");
    }

    // 3. PidController recevant NaN ne doit pas empoisonner son état
    let flow_pid = FlowGraph {
        id: "flow_pid_nan".into(),
        name: "PID NaN Test".into(),
        description: String::new(),
        enabled: true,
        nodes: vec![
            node("in", FlowNodeKind::TagInput { tag_id: "A".into() }),
            node("pid", FlowNodeKind::PidController {
                setpoint_tag_id: "SP".into(),
                kp: 1.0,
                ki: 0.5,
                kd: 0.0,
                min_out: 0.0,
                max_out: 100.0,
                reverse_acting: false,
            }),
            node("out", FlowNodeKind::TagOutput { target_tag_id: "SORTIE".into() }),
        ],
        edges: vec![edge("e1", "in", "pid"), edge("e2", "pid", "out")],
    };

    // Injection directe d'un float NaN dans l'entrée du PID
    run_flow_from_entry_node(&core, &flow_pid, &flow_pid.nodes[0], &input("A", f64::NAN), &engine).await;
    assert!(core.flow_runtime.last_outputs.is_empty(), "Le PID a propagé une valeur suite à une entrée NaN");

    // L'état mémorisé ne doit pas être NaN
    let key = node_key(&flow_pid.id, "pid");
    if let Some(entry) = core.flow_runtime.pid_memory.get(&key) {
        assert!(entry.integral.is_finite(), "Intégrale empoisonnée par NaN");
        assert!(entry.prev_error.is_finite(), "prev_error empoisonnée par NaN");
    }

    // Cycle suivant avec une valeur saine : le PID fonctionne normalement
    run_flow_from_entry_node(&core, &flow_pid, &flow_pid.nodes[0], &input("A", 40.0), &engine).await;
    let out_val = core.tags_state.get("SORTIE").unwrap().value.as_f64().unwrap();
    assert!(out_val.is_finite(), "Sortie PID non finie");
    assert!(out_val > 0.0, "Le PID devrait produire une sortie positive");
}

#[tokio::test]
async fn vague3_pid_dt_reel_sans_plancher_100ms() {
    let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
    core.register_tag(internal_tag("PID_OUT", 0.0), None);
    core.register_tag(internal_tag("SP", 0.0), Some(TagValue::Float(10.0)));
    let engine = shared_engine(&core);

    let flow_pid = FlowGraph {
        id: "flow_pid_dt".into(),
        name: "PID dt test".into(),
        description: String::new(),
        enabled: true,
        nodes: vec![
            node("in", FlowNodeKind::TagInput { tag_id: "A".into() }),
            node("pid", FlowNodeKind::PidController {
                setpoint_tag_id: "SP".into(),
                kp: 0.0,
                ki: 1.0,
                kd: 0.0,
                min_out: 0.0,
                max_out: 100.0,
                reverse_acting: false,
            }),
            node("out", FlowNodeKind::TagOutput { target_tag_id: "PID_OUT".into() }),
        ],
        edges: vec![edge("e1", "in", "pid"), edge("e2", "pid", "out")],
    };

    let key = node_key(&flow_pid.id, "pid");

    // Initialisation
    run_flow_from_entry_node(&core, &flow_pid, &flow_pid.nodes[0], &input("A", 0.0), &engine).await;

    // Simule 5 évaluations rapprochées espacées de 10 ms (total 50 ms)
    for _ in 0..5 {
        if let Some(mut entry) = core.flow_runtime.pid_memory.get_mut(&key) {
            entry.last_eval = Utc::now() - chrono::Duration::milliseconds(10);
        }
        run_flow_from_entry_node(&core, &flow_pid, &flow_pid.nodes[0], &input("A", 0.0), &engine).await;
    }

    let integral = core.flow_runtime.pid_memory.get(&key).unwrap().integral;
    // Avec error = 10.0 et 5 passes de 10 ms (0.01s) :
    // - Code corrigé (dt ~ 0.010s) : intégrale totale ~ 5 * (10.0 * 0.010) = 0.50 (± 0.1)
    // - Ancien code avec plancher dt >= 100 ms (0.1s) : intégrale = 5 * (10.0 * 0.10) = 5.0
    assert!(
        integral < 1.5,
        "Sur-intégration constatée : intégrale = {integral} (devrait être ~0.50, l'ancien code donnait >= 5.0)"
    );
}
