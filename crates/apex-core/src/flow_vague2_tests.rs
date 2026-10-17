// Inclus dans le module de tests du moteur : les essais parcourent des graphes complets.
#[tokio::test]
async fn vague2_secours_pompe_branche_conditionnelle() {
    let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
    let flow = core.flows.get("flow-secours-pompe").unwrap().clone();
    let target = "IND.CHAUF.Pompe_P2_Secours";
    core.register_tag(internal_bool_tag(target), Some(TagValue::Bool(false)));
    let engine = shared_engine(&core);
    let mut traces = core.subscribe_flow_traces();
    for (pressure, expected, written) in [(2.5, false, false), (1.0, true, true), (2.5, true, false)] {
        while traces.try_recv().is_ok() {}
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &input("IND.CHAUF.Pression_Reseau", pressure), &engine).await;
        assert_eq!(core.tags_state.get(target).unwrap().value, TagValue::Bool(expected));
        let mut reached_script = false;
        while let Ok(trace) = traces.try_recv() { reached_script |= trace.to_node_id == "n-rhai-p2-on"; }
        assert_eq!(reached_script, written);
    }
}

fn chain(kind: FlowNodeKind) -> FlowGraph {
    FlowGraph { id: "vague2".into(), name: "Essai".into(), description: String::new(), enabled: true,
        nodes: vec![node("in", FlowNodeKind::TagInput { tag_id: "A".into() }), node("decision", kind),
            node("gate", FlowNodeKind::LogicGate { gate: LogicGateType::And }),
            node("timer", FlowNodeKind::DelayTimer { delay_ms: 0 }),
            node("out", FlowNodeKind::TagOutput { target_tag_id: "SORTIE".into() })],
        edges: vec![edge("a", "in", "decision"), edge("b", "decision", "gate"), edge("c", "gate", "timer"), edge("d", "timer", "out")] }
}

#[tokio::test]
async fn vague2_decisions_port_out_propage_front_descendant() {
    for kind in [FlowNodeKind::Threshold { operator: ComparisonOperator::Gt, threshold: 10.0, hysteresis: 0.0 },
        FlowNodeKind::Hysteresis { low_threshold: 5.0, high_threshold: 10.0, invert: true }] {
        let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
        core.register_tag(internal_bool_tag("SORTIE"), None);
        let flow = chain(kind);
        for (v, expected) in [(15.0,true), (0.0,false)] {
            run_flow_from_entry_node(&core,&flow,&flow.nodes[0],&input("A",v),&shared_engine(&core)).await;
            assert_eq!(core.tags_state.get("SORTIE").unwrap().value, TagValue::Bool(expected));
        }
    }
}

#[tokio::test]
async fn vague2_portes_attendent_toutes_entrees_et_perdent_qualite() {
    let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
    core.flows.clear();
    core.register_tag(internal_bool_tag("A"), Some(TagValue::Bool(true)));
    core.register_tag(internal_bool_tag("B"), None);
    core.register_tag(internal_bool_tag("SORTIE"), None);
    let mut flow = chain(FlowNodeKind::LogicGate { gate: LogicGateType::And });
    flow.nodes.push(node("second", FlowNodeKind::TagInput { tag_id: "B".into() }));
    flow.edges.push(edge("second-edge", "second", "gate"));
    core.flows.insert(flow.id.clone(),flow);
    let engine=shared_engine(&core);
    evaluate_all_enabled_flows_once(&core,&engine).await;
    assert!(core.flow_runtime.last_outputs.is_empty());
    core.write_tag("B",TagValue::Bool(true)).await.unwrap();
    evaluate_all_enabled_flows_once(&core,&engine).await;
    assert_eq!(core.tags_state.get("SORTIE").unwrap().value,TagValue::Bool(true));
    core.tags_state.get_mut("B").unwrap().quality=Quality::BadDisconnect;
    core.write_tag("SORTIE",TagValue::Bool(false)).await.unwrap();
    core.flow_runtime.last_outputs.clear();
    evaluate_all_enabled_flows_once(&core,&engine).await;
    assert!(core.flow_runtime.last_outputs.is_empty());
    assert_eq!(core.tags_state.get("SORTIE").unwrap().value,TagValue::Bool(false));
}

#[tokio::test]
async fn vague2_rhai_indisponible_arrete_chaine() {
    let core=ScadaCore::new(None,&crate::core::EngineOptions::default());
    core.register_tag(internal_bool_tag("SORTIE"), None);
    for script in ["tag(\"MANQUANT\") < 1.8", "!tag_bool(\"MANQUANT\")", "tag_text(\"MANQUANT\") == \"\"", "clamp(1.0, 50.0, 10.0) > 0.0"] {
        let flow=chain(FlowNodeKind::RhaiScript { script:script.into() });
        run_flow_from_entry_node(&core,&flow,&flow.nodes[0],&input("A",1.0),&shared_engine(&core)).await;
        assert!(core.flow_runtime.last_outputs.is_empty(), "{script}");
    }
}

#[tokio::test]
async fn vague2_branche_profonde_ne_supprime_pas_branche_parallele() {
    let core=ScadaCore::new(None,&crate::core::EngineOptions::default());
    core.register_tag(internal_bool_tag("SORTIE"),None);
    let mut flow=chain(FlowNodeKind::LogicGate { gate:LogicGateType::And });
    flow.nodes.push(node("boucle",FlowNodeKind::MathFormula { factor:1.0,offset:0.0 }));
    flow.edges.push(edge("boucle-start","in","boucle"));
    flow.edges.push(edge("boucle-back","boucle","boucle"));
    run_flow_from_entry_node(&core,&flow,&flow.nodes[0],&input("A",1.0),&shared_engine(&core)).await;
    assert_eq!(core.tags_state.get("SORTIE").unwrap().value,TagValue::Bool(true));
}
