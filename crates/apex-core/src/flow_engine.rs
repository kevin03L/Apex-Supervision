//! # Module `flow_engine` — Moteur d'Exécution de Graphes Logiques (Esprit Node-RED Industriel)
//!
//! Fonctionnalités complètes :
//! - Évaluation événementielle (COV) ET cyclique (Tick 1s pour PID et Temporisateurs TON).
//! - 9 types de nœuds : `TagInput`, `Threshold`, `Hysteresis`, `PidController`, `LogicGate`
//!   (`And`, `Or`, `Xor`, `Not`, `Nand`, `Nor`), `DelayTimer`, `MathFormula`, `RhaiScript`, `TagOutput`.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rhai::{Dynamic, Engine, Scope, AST};
use tracing::{error, info, warn};

use crate::{
    core::ScadaCore,
    models::{
        ComparisonOperator, FlowEdge, FlowExecutionTrace, FlowGraph, FlowNode, FlowNodeKind,
        LogicGateType, Quality, TagState, TagValue,
    },
};

const MAX_FLOW_DEPTH: usize = 20;
/// Nombre maximal de scripts Rhai compilés gardés en cache.
const RHAI_AST_CACHE_MAX: usize = 512;

#[derive(Debug, Clone)]
pub struct PidState {
    pub integral: f64,
    pub prev_error: f64,
    pub last_eval: DateTime<Utc>,
}

pub struct FlowEngineRuntime {
    pub threshold_memory: DashMap<String, bool>,
    pub hysteresis_memory: DashMap<String, bool>,
    pub pid_memory: DashMap<String, PidState>,
    pub gate_inputs: DashMap<String, HashMap<String, bool>>,
    pub timer_since: DashMap<String, Option<DateTime<Utc>>>,
    /// Sorties TagOutput actuellement en échec d'écriture (anti-répétition des avertissements).
    pub failing_outputs: DashMap<String, ()>,
    /// Nœuds RhaiScript actuellement en échec (clé `flux/nœud`) : un seul avertissement par
    /// nœud jusqu'au retour à la normale, au lieu d'une erreur à chaque cycle.
    pub failing_scripts: DashMap<String, ()>,
    /// Dernière valeur écrite par chaque nœud TagOutput (clé `flux/nœud`) : la sortie n'est
    /// réécrite que si elle change au-delà de la bande morte du tag cible. La valeur est
    /// accompagnée du tag visé, pour l'oublier quand l'écriture n'a pas abouti.
    pub last_outputs: DashMap<String, (String, TagValue)>,
    /// Scripts Rhai déjà compilés, indexés par leur texte source.
    ast_cache: DashMap<String, Arc<AST>>,
    /// Moteur Rhai partagé (initialisé au démarrage du worker).
    engine: OnceLock<Arc<Engine>>,
}

impl Default for FlowEngineRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl FlowEngineRuntime {
    pub fn new() -> Self {
        Self {
            threshold_memory: DashMap::new(),
            hysteresis_memory: DashMap::new(),
            pid_memory: DashMap::new(),
            gate_inputs: DashMap::new(),
            timer_since: DashMap::new(),
            failing_outputs: DashMap::new(),
            failing_scripts: DashMap::new(),
            last_outputs: DashMap::new(),
            ast_cache: DashMap::new(),
            engine: OnceLock::new(),
        }
    }

    /// Oublie les mémoires d'exécution propres à un flux (redéploiement ou suppression).
    pub fn forget_flow(&self, flow_id: &str) {
        let prefix = format!("{}/", flow_id);
        self.failing_scripts.retain(|k, _| !k.starts_with(&prefix));
        self.last_outputs.retain(|k, _| !k.starts_with(&prefix));
    }

    /// Oublie la dernière sortie des nœuds qui écrivent sur ce tag : l'écriture a été refusée
    /// ou perdue (coupure de liaison), elle sera refaite à la prochaine évaluation du flux.
    pub fn forget_outputs_for_tag(&self, tag_id: &str) {
        self.last_outputs.retain(|_, (target, _)| target != tag_id);
    }

    pub fn spawn_worker(core: Arc<ScadaCore>) {
        let mut cov_rx = core.subscribe_cov();
        let rhai_engine = shared_engine(&core);

        tokio::spawn(async move {
            let mut cyclic_ticker = tokio::time::interval(Duration::from_millis(1200));

            loop {
                tokio::select! {
                    cov_res = cov_rx.recv() => {
                        match cov_res {
                            Ok(tag_state) => {
                                if tag_state.quality != Quality::Good || core.is_acquisition_paused() {
                                    continue;
                                }
                                evaluate_flows_for_tag(&core, &tag_state, &rhai_engine).await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                warn!("FlowEngine a ignoré {} événements COV", skipped);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                error!("Canal COV fermé : arrêt du moteur de flux");
                                break;
                            }
                        }
                    }
                    _ = cyclic_ticker.tick() => {
                        if core.is_acquisition_paused() {
                            continue;
                        }
                        // Exécution cyclique de maintien pour les boucles PID et temporisateurs
                        evaluate_all_enabled_flows_once(&core, &rhai_engine).await;
                    }
                }
            }
        });
    }
}

/// Bac à sable : bornes de calcul et de mémoire pour les scripts utilisateurs.
fn apply_rhai_sandbox(engine: &mut Engine) {
    engine.set_max_operations(15_000);
    engine.set_max_string_size(4_096);
    engine.set_max_array_size(1_024);
    engine.set_max_map_size(256);
    engine.set_max_call_levels(16);
    engine.set_max_expr_depths(64, 32);
}

/// Vérifie qu'un script Rhai compile (syntaxe et limites du bac à sable), sans l'exécuter.
/// Appelé à l'enregistrement d'un flux pour refuser un script invalide avec le message Rhai.
pub fn check_rhai_script(script: &str) -> Result<(), String> {
    static CHECKER: OnceLock<Engine> = OnceLock::new();
    if script.trim().is_empty() {
        return Err("script vide".to_string());
    }
    let engine = CHECKER.get_or_init(|| {
        let mut engine = Engine::new();
        apply_rhai_sandbox(&mut engine);
        engine
    });
    engine
        .compile(script)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn build_industrial_rhai_engine(core: Arc<ScadaCore>) -> Engine {
    let mut engine = Engine::new();
    apply_rhai_sandbox(&mut engine);

    let core_for_num = Arc::clone(&core);
    engine.register_fn("tag", move |tag_id: &str| -> f64 {
        if core_for_num.is_acquisition_paused() {
            return 0.0;
        }
        core_for_num
            .tags_state
            .get(tag_id)
            .filter(|s| s.quality == Quality::Good)
            .and_then(|s| s.value.as_f64())
            .unwrap_or(0.0)
    });

    let core_for_bool = Arc::clone(&core);
    engine.register_fn("tag_bool", move |tag_id: &str| -> bool {
        if core_for_bool.is_acquisition_paused() {
            return false;
        }
        core_for_bool
            .tags_state
            .get(tag_id)
            .filter(|s| s.quality == Quality::Good)
            .and_then(|s| s.value.as_bool())
            .unwrap_or(false)
    });

    let core_for_text = Arc::clone(&core);
    engine.register_fn("tag_text", move |tag_id: &str| -> String {
        if core_for_text.is_acquisition_paused() {
            return String::new();
        }
        core_for_text
            .tags_state
            .get(tag_id)
            .filter(|s| s.quality == Quality::Good)
            .map(|s| match &s.value {
                TagValue::Text(t) => t.clone(),
                TagValue::Bool(b) => b.to_string(),
                TagValue::Int(i) => i.to_string(),
                TagValue::Float(f) => f.to_string(),
            })
            .unwrap_or_default()
    });

    engine.register_fn("clamp", |val: f64, min_v: f64, max_v: f64| -> f64 {
        val.clamp(min_v, max_v)
    });

    engine
}

/// Force l'exécution immédiate d'un flux spécifique (bouton "Tester le Flux" de l'IHM).
pub async fn trigger_flow_now(core: &Arc<ScadaCore>, flow_id: &str) -> Result<usize, String> {
    if core.is_acquisition_paused() {
        return Err("Acquisition en pause : exécution impossible".to_string());
    }

    let flow = core
        .flows
        .get(flow_id)
        .map(|f| f.clone())
        .ok_or_else(|| format!("Flux '{}' introuvable", flow_id))?;

    let rhai_engine = shared_engine(core);
    let mut executed = 0usize;

    for node in &flow.nodes {
        if let FlowNodeKind::TagInput { tag_id } = &node.kind {
            if let Some(st) = core.tags_state.get(tag_id).map(|s| s.clone()) {
                if st.quality == Quality::Good {
                    run_flow_from_entry_node(core, &flow, node, &st, &rhai_engine).await;
                    executed += 1;
                }
            }
        }
    }

    Ok(executed)
}

async fn evaluate_all_enabled_flows_once(core: &Arc<ScadaCore>, rhai_engine: &Arc<Engine>) {
    if core.is_acquisition_paused() {
        return;
    }
    let active_flows: Vec<FlowGraph> = core
        .flows
        .iter()
        .filter(|f| f.value().enabled)
        .map(|f| f.value().clone())
        .collect();

    for flow in active_flows {
        for node in &flow.nodes {
            if let FlowNodeKind::TagInput { tag_id } = &node.kind {
                if let Some(st) = core.tags_state.get(tag_id).map(|s| s.clone()) {
                    if st.quality == Quality::Good {
                        run_flow_from_entry_node(core, &flow, node, &st, rhai_engine).await;
                    }
                }
            }
        }
    }
}

async fn evaluate_flows_for_tag(
    core: &Arc<ScadaCore>,
    tag_state: &TagState,
    rhai_engine: &Arc<Engine>,
) {
    if tag_state.quality != Quality::Good || core.is_acquisition_paused() {
        return;
    }
    // Seuls les flux qui lisent ce tag sont copiés (évite de cloner tous les graphes à chaque COV)
    let listens_to_tag = |f: &FlowGraph| {
        f.nodes.iter().any(
            |n| matches!(&n.kind, FlowNodeKind::TagInput { tag_id } if tag_id == &tag_state.tag_id),
        )
    };
    let active_flows: Vec<FlowGraph> = core
        .flows
        .iter()
        .filter(|f| f.value().enabled && listens_to_tag(f.value()))
        .map(|f| f.value().clone())
        .collect();

    for flow in active_flows {
        for start_node in &flow.nodes {
            if let FlowNodeKind::TagInput { tag_id } = &start_node.kind {
                if tag_id == &tag_state.tag_id {
                    run_flow_from_entry_node(core, &flow, start_node, tag_state, rhai_engine).await;
                }
            }
        }
    }
}

async fn run_flow_from_entry_node(
    core: &Arc<ScadaCore>,
    flow: &FlowGraph,
    start_node: &FlowNode,
    tag_state: &TagState,
    rhai_engine: &Arc<Engine>,
) {
    if tag_state.quality != Quality::Good || core.is_acquisition_paused() {
        return;
    }

    let mut queue: Vec<(String, String, TagValue, usize)> = vec![(
        start_node.id.clone(),
        start_node.id.clone(),
        tag_state.value.clone(),
        0,
    )];

    while let Some((current_node_id, upstream_node_id, current_val, depth)) = queue.pop() {
        if depth >= MAX_FLOW_DEPTH {
            warn!("Profondeur maximale DAG atteinte sur '{}'", flow.id);
            break;
        }

        let Some(current_node) = flow.nodes.iter().find(|n| n.id == current_node_id) else {
            continue;
        };

        let mut out_port = "out".to_string();
        let mut next_value = current_val.clone();

        match &current_node.kind {
            FlowNodeKind::TagInput { .. } => {}

            FlowNodeKind::Threshold {
                operator,
                threshold,
                hysteresis,
            } => {
                let Some(num_val) = current_val.as_f64() else {
                    continue;
                };
                let prev = core
                    .flow_runtime
                    .threshold_memory
                    .get(&current_node.id)
                    .map(|b| *b)
                    .unwrap_or(false);

                let cond = eval_threshold(num_val, *operator, *threshold, *hysteresis, prev);
                core.flow_runtime
                    .threshold_memory
                    .insert(current_node.id.clone(), cond);

                out_port = if cond {
                    "true".to_string()
                } else {
                    "false".to_string()
                };
                next_value = TagValue::Float(num_val);
            }

            FlowNodeKind::Hysteresis {
                low_threshold,
                high_threshold,
                invert,
            } => {
                let Some(num_val) = current_val.as_f64() else {
                    continue;
                };
                let prev = core
                    .flow_runtime
                    .hysteresis_memory
                    .get(&current_node.id)
                    .map(|b| *b)
                    .unwrap_or(false);

                let raw_state = if num_val <= *low_threshold {
                    true
                } else if num_val >= *high_threshold {
                    false
                } else {
                    prev
                };

                let final_state = if *invert { !raw_state } else { raw_state };
                core.flow_runtime
                    .hysteresis_memory
                    .insert(current_node.id.clone(), raw_state);

                out_port = if final_state {
                    "true".to_string()
                } else {
                    "false".to_string()
                };
                next_value = TagValue::Bool(final_state);
            }

            FlowNodeKind::PidController {
                setpoint_tag_id,
                kp,
                ki,
                kd,
                min_out,
                max_out,
                reverse_acting,
            } => {
                let Some(pv) = current_val.as_f64() else {
                    continue;
                };
                let sp = if !setpoint_tag_id.is_empty() {
                    match core.tags_state.get(setpoint_tag_id) {
                        Some(s) if s.quality == Quality::Good => match s.value.as_f64() {
                            Some(val) => val,
                            None => continue,
                        },
                        Some(_) => {
                            // Consigne présente mais non Good (Uncertain ou BadDisconnect) : ne pas réguler
                            continue;
                        }
                        None => 21.0,
                    }
                } else {
                    21.0
                };

                let now = Utc::now();
                let mut entry = core
                    .flow_runtime
                    .pid_memory
                    .entry(current_node.id.clone())
                    .or_insert_with(|| PidState {
                        integral: 0.0,
                        prev_error: 0.0,
                        last_eval: now,
                    });

                let dt = (now - entry.last_eval).num_milliseconds().max(100) as f64 / 1000.0;
                let dt = dt.clamp(0.1, 10.0);

                let raw_err = sp - pv;
                let error = if *reverse_acting { -raw_err } else { raw_err };

                entry.integral = (entry.integral + error * dt).clamp(-200.0, 200.0);
                let derivative = (error - entry.prev_error) / dt;
                entry.prev_error = error;
                entry.last_eval = now;

                let output = (*kp * error + *ki * entry.integral + *kd * derivative)
                    .clamp(*min_out, *max_out);
                next_value = TagValue::Float((output * 100.0).round() / 100.0);
            }

            FlowNodeKind::LogicGate { gate } => {
                let Some(in_bool) = current_val.as_bool() else {
                    continue;
                };
                let mut inputs_map = core
                    .flow_runtime
                    .gate_inputs
                    .entry(current_node.id.clone())
                    .or_default();
                inputs_map.insert(upstream_node_id.clone(), in_bool);

                let vals: Vec<bool> = inputs_map.values().copied().collect();
                let result_bool = match gate {
                    LogicGateType::And => vals.iter().all(|&b| b),
                    LogicGateType::Or => vals.iter().any(|&b| b),
                    LogicGateType::Xor => vals.iter().filter(|&&b| b).count() % 2 == 1,
                    LogicGateType::Not => !in_bool,
                    LogicGateType::Nand => !vals.iter().all(|&b| b),
                    LogicGateType::Nor => !vals.iter().any(|&b| b),
                };

                out_port = if result_bool {
                    "true".to_string()
                } else {
                    "false".to_string()
                };
                next_value = TagValue::Bool(result_bool);
            }

            FlowNodeKind::DelayTimer { delay_ms } => {
                let active = current_val.as_bool().unwrap_or(false);
                let now = Utc::now();
                if active {
                    let mut entry = core
                        .flow_runtime
                        .timer_since
                        .entry(current_node.id.clone())
                        .or_insert(Some(now));
                    let start = entry.get_or_insert(now);
                    let elapsed = (now - *start).num_milliseconds().max(0) as u64;
                    if elapsed < *delay_ms {
                        next_value = TagValue::Bool(false);
                    } else {
                        next_value = TagValue::Bool(true);
                    }
                } else {
                    core.flow_runtime
                        .timer_since
                        .insert(current_node.id.clone(), None);
                    next_value = TagValue::Bool(false);
                }
            }

            FlowNodeKind::MathFormula { factor, offset } => {
                let Some(num) = current_val.as_f64() else {
                    continue;
                };
                let res = ((num * *factor + *offset) * 100.0).round() / 100.0;
                next_value = TagValue::Float(res);
            }

            FlowNodeKind::RhaiScript { script } => {
                let key = node_key(&flow.id, &current_node.id);
                match eval_rhai_script(rhai_engine, script, &current_val, core) {
                    Ok(computed) => {
                        if core.flow_runtime.failing_scripts.remove(&key).is_some() {
                            info!(
                                "Script Rhai du nœud '{}' (flux '{}') rétabli",
                                current_node.label, flow.id
                            );
                        }
                        next_value = computed;
                    }
                    // Un seul avertissement par nœud en échec, jusqu'au retour à la normale
                    Err(err) => {
                        if core.flow_runtime.failing_scripts.insert(key, ()).is_none() {
                            warn!(
                                "Erreur Rhai dans le nœud '{}' (flux '{}') : {} (avertissement unique jusqu'au rétablissement)",
                                current_node.label, flow.id, err
                            );
                        }
                        continue;
                    }
                }
            }

            FlowNodeKind::TagOutput { target_tag_id } => {
                if target_tag_id != &tag_state.tag_id {
                    // Déduplication sur la dernière valeur écrite par ce nœud (et non sur l'état
                    // relu, filtré par la bande morte du tag : il ne refléterait jamais une petite
                    // variation et la même consigne serait réécrite à chaque cycle).
                    let key = node_key(&flow.id, &current_node.id);
                    let target_meta = core
                        .tags_registry
                        .get(target_tag_id)
                        .map(|t| (t.deadband, t.data_type));
                    let band = target_meta.map(|(d, _)| d).unwrap_or(0.0);
                    // Comparée sous la forme réellement écrite (convertie au type du tag)
                    let candidate = target_meta
                        .and_then(|(_, ty)| next_value.coerce_to(ty).ok())
                        .unwrap_or_else(|| next_value.clone());
                    let should_write = output_needs_write(
                        &candidate,
                        core.flow_runtime
                            .last_outputs
                            .get(&key)
                            .as_deref()
                            .map(|(_, v)| v),
                        band,
                    );
                    if should_write {
                        match core.write_tag(target_tag_id, next_value.clone()).await {
                            Ok(written) => {
                                core.flow_runtime
                                    .last_outputs
                                    .insert(key, (target_tag_id.clone(), written.value));
                                core.flow_runtime.failing_outputs.remove(target_tag_id);
                            }
                            // Un seul avertissement par sortie en échec, jusqu'au retour à la normale
                            Err(err) => {
                                if core
                                    .flow_runtime
                                    .failing_outputs
                                    .insert(target_tag_id.clone(), ())
                                    .is_none()
                                {
                                    warn!(
                                        "FlowEngine n'a pas pu écrire sur '{}' : {} (avertissement unique jusqu'au rétablissement)",
                                        target_tag_id, err
                                    );
                                }
                            }
                        }
                    }
                }
                continue;
            }
        }

        let matching_edges: Vec<&FlowEdge> = flow
            .edges
            .iter()
            .filter(|edge| {
                edge.from_node == current_node.id
                    && (edge.from_port == out_port || edge.from_port == "out")
            })
            .collect();

        for edge in matching_edges {
            core.emit_flow_trace(FlowExecutionTrace {
                flow_id: flow.id.clone(),
                from_node_id: edge.from_node.clone(),
                to_node_id: edge.to_node.clone(),
                port: out_port.clone(),
                value: next_value.clone(),
                timestamp: Utc::now(),
            });

            queue.push((
                edge.to_node.clone(),
                current_node.id.clone(),
                next_value.clone(),
                depth + 1,
            ));
        }
    }
}

/// Clé des mémoires d'exécution propres à un nœud (les identifiants ne contiennent pas « / »).
fn node_key(flow_id: &str, node_id: &str) -> String {
    format!("{}/{}", flow_id, node_id)
}

/// Écart minimal entre deux écritures successives d'une sortie TagOutput.
const OUTPUT_MIN_BAND: f64 = 0.08;

/// Une sortie est (ré)écrite si rien n'a encore été écrit par ce nœud, ou si la nouvelle valeur
/// s'écarte de la dernière écrite d'au moins `max(bande morte du tag, 0,08)`.
/// Les valeurs numériques (Float ou Int) sont comparées numériquement.
fn output_needs_write(next: &TagValue, last_written: Option<&TagValue>, deadband: f64) -> bool {
    let Some(last) = last_written else {
        return true;
    };
    let band = if deadband.is_finite() {
        deadband.abs().max(OUTPUT_MIN_BAND)
    } else {
        OUTPUT_MIN_BAND
    };
    match (next, last) {
        (TagValue::Float(_) | TagValue::Int(_), TagValue::Float(_) | TagValue::Int(_)) => {
            match (next.as_f64(), last.as_f64()) {
                (Some(a), Some(b)) => (a - b).abs() >= band,
                _ => true,
            }
        }
        _ => next != last,
    }
}

fn eval_threshold(
    val: f64,
    op: ComparisonOperator,
    threshold: f64,
    hysteresis: f64,
    prev_state: bool,
) -> bool {
    let h = hysteresis.abs();
    match op {
        ComparisonOperator::Lt => {
            if prev_state {
                val < (threshold + h)
            } else {
                val < threshold
            }
        }
        ComparisonOperator::Lte => {
            if prev_state {
                val <= (threshold + h)
            } else {
                val <= threshold
            }
        }
        ComparisonOperator::Gt => {
            if prev_state {
                val > (threshold - h)
            } else {
                val > threshold
            }
        }
        ComparisonOperator::Gte => {
            if prev_state {
                val >= (threshold - h)
            } else {
                val >= threshold
            }
        }
        ComparisonOperator::Eq => (val - threshold).abs() <= h.max(1e-5),
        ComparisonOperator::Neq => (val - threshold).abs() > h.max(1e-5),
    }
}

fn eval_rhai_script(
    engine: &Engine,
    script: &str,
    input: &TagValue,
    core: &ScadaCore,
) -> Result<TagValue, String> {
    let mut scope = Scope::new();
    match input {
        TagValue::Float(f) => {
            scope.push("val", *f);
        }
        TagValue::Int(i) => {
            scope.push("val", *i as f64);
        }
        TagValue::Bool(b) => {
            scope.push("val", *b);
        }
        TagValue::Text(s) => {
            scope.push("val", s.clone());
        }
    }

    let consigne_val = core
        .tags_state
        .get("GTB.CTA01.Consigne_Temp")
        .and_then(|s| s.value.as_f64())
        .unwrap_or(21.0);
    scope.push("consigne", consigne_val);

    let ast = compiled_script(engine, core, script)?;
    let result: Dynamic = engine
        .eval_ast_with_scope(&mut scope, &ast)
        .map_err(|e| e.to_string())?;

    if let Ok(f) = result.as_float() {
        Ok(TagValue::Float((f * 100.0).round() / 100.0))
    } else if let Ok(i) = result.as_int() {
        Ok(TagValue::Int(i))
    } else if let Ok(b) = result.as_bool() {
        Ok(TagValue::Bool(b))
    } else if let Ok(s) = result.into_string() {
        Ok(TagValue::Text(s))
    } else {
        Err("Type de retour Rhai non supporté".to_string())
    }
}

/// Moteur Rhai unique, partagé par le worker cyclique et par « Exécuter maintenant ».
fn shared_engine(core: &Arc<ScadaCore>) -> Arc<Engine> {
    Arc::clone(
        core.flow_runtime
            .engine
            .get_or_init(|| Arc::new(build_industrial_rhai_engine(Arc::clone(core)))),
    )
}

/// Compile un script une seule fois puis le réutilise depuis le cache.
fn compiled_script(engine: &Engine, core: &ScadaCore, script: &str) -> Result<Arc<AST>, String> {
    if let Some(ast) = core.flow_runtime.ast_cache.get(script) {
        return Ok(Arc::clone(ast.value()));
    }
    let ast = Arc::new(engine.compile(script).map_err(|e| e.to_string())?);
    if core.flow_runtime.ast_cache.len() >= RHAI_AST_CACHE_MAX {
        core.flow_runtime.ast_cache.clear();
    }
    core.flow_runtime
        .ast_cache
        .insert(script.to_string(), Arc::clone(&ast));
    Ok(ast)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_with_hysteresis() {
        // Seuil bas 1.8 bar avec hystérésis 0.3 : bascule à 1.79, retombe seulement au-dessus de 2.1
        assert!(!eval_threshold(
            1.85,
            ComparisonOperator::Lt,
            1.8,
            0.3,
            false
        ));
        assert!(eval_threshold(
            1.79,
            ComparisonOperator::Lt,
            1.8,
            0.3,
            false
        ));
        assert!(eval_threshold(2.0, ComparisonOperator::Lt, 1.8, 0.3, true));
        assert!(!eval_threshold(
            2.15,
            ComparisonOperator::Lt,
            1.8,
            0.3,
            true
        ));
    }

    #[test]
    fn threshold_greater_and_equality() {
        assert!(eval_threshold(
            25.1,
            ComparisonOperator::Gt,
            25.0,
            0.5,
            false
        ));
        assert!(eval_threshold(
            24.7,
            ComparisonOperator::Gt,
            25.0,
            0.5,
            true
        ));
        assert!(!eval_threshold(
            24.4,
            ComparisonOperator::Gt,
            25.0,
            0.5,
            true
        ));
        assert!(eval_threshold(3.0, ComparisonOperator::Eq, 3.0, 0.0, false));
        assert!(eval_threshold(
            3.1,
            ComparisonOperator::Neq,
            3.0,
            0.0,
            false
        ));
    }

    #[test]
    fn output_dedup_uses_last_written_value_and_tag_deadband() {
        let f = TagValue::Float;
        assert!(output_needs_write(&f(20.0), None, 0.5));
        // Bande morte du tag 0,5 : une variation de 0,3 n'est pas réécrite
        assert!(!output_needs_write(&f(20.3), Some(&f(20.0)), 0.5));
        assert!(output_needs_write(&f(20.5), Some(&f(20.0)), 0.5));
        // Bande minimale 0,08
        assert!(!output_needs_write(&f(20.05), Some(&f(20.0)), 0.0));
        assert!(output_needs_write(&f(20.1), Some(&f(20.0)), 0.0));
        // Entier et flottant comparés numériquement
        assert!(!output_needs_write(&f(20.0), Some(&TagValue::Int(20)), 0.0));
        assert!(!output_needs_write(
            &TagValue::Bool(true),
            Some(&TagValue::Bool(true)),
            0.0
        ));
        assert!(output_needs_write(
            &TagValue::Bool(false),
            Some(&TagValue::Bool(true)),
            0.0
        ));
    }

    #[test]
    fn rhai_scripts_are_checked_without_running() {
        assert!(check_rhai_script("clamp(val * 1.5, 0.0, 100.0)").is_ok());
        assert!(check_rhai_script("if val > 3 { true } else { false }").is_ok());
        assert!(check_rhai_script("let x = ;").is_err());
        assert!(check_rhai_script("").is_err());
    }

    fn internal_tag(id: &str, deadband: f64) -> crate::models::RegisteredTag {
        crate::models::RegisteredTag {
            id: id.into(),
            label: id.into(),
            module_source: "INTERNE".into(),
            protocol_address: String::new(),
            data_type: crate::models::TagDataType::Float,
            unit: None,
            writable: true,
            deadband,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: false,
            alarm_config: Default::default(),
        }
    }

    fn internal_bool_tag(id: &str) -> crate::models::RegisteredTag {
        crate::models::RegisteredTag {
            id: id.into(),
            label: id.into(),
            module_source: "INTERNE".into(),
            protocol_address: String::new(),
            data_type: crate::models::TagDataType::Bool,
            unit: None,
            writable: true,
            deadband: 0.0,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: false,
            alarm_config: Default::default(),
        }
    }

    fn node(id: &str, kind: FlowNodeKind) -> FlowNode {
        FlowNode {
            id: id.into(),
            label: id.into(),
            x: 0.0,
            y: 0.0,
            kind,
        }
    }

    fn edge(id: &str, from: &str, to: &str) -> FlowEdge {
        FlowEdge {
            id: id.into(),
            from_node: from.into(),
            from_port: "out".into(),
            to_node: to.into(),
            to_port: "in".into(),
        }
    }

    fn input(tag_id: &str, v: f64) -> TagState {
        TagState {
            tag_id: tag_id.into(),
            value: TagValue::Float(v),
            quality: Quality::Good,
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn tag_output_is_not_rewritten_within_target_deadband() {
        let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
        core.register_tag(internal_tag("INT.In", 0.0), None);
        core.register_tag(internal_tag("INT.Out", 0.5), None);
        let flow = FlowGraph {
            id: "f".into(),
            name: "f".into(),
            description: String::new(),
            enabled: true,
            nodes: vec![
                node(
                    "in",
                    FlowNodeKind::TagInput {
                        tag_id: "INT.In".into(),
                    },
                ),
                node(
                    "out",
                    FlowNodeKind::TagOutput {
                        target_tag_id: "INT.Out".into(),
                    },
                ),
            ],
            edges: vec![edge("e", "in", "out")],
        };
        let engine = shared_engine(&core);
        let out = |core: &ScadaCore| core.tags_state.get("INT.Out").unwrap().value.clone();

        run_flow_from_entry_node(
            &core,
            &flow,
            &flow.nodes[0],
            &input("INT.In", 20.0),
            &engine,
        )
        .await;
        assert_eq!(out(&core), TagValue::Float(20.0));
        // L'opérateur modifie la sortie : le flux ne la réécrit pas tant que sa propre sortie
        // ne varie pas au-delà de la bande morte du tag (plus de réécriture à chaque cycle)
        core.write_tag("INT.Out", TagValue::Float(30.0))
            .await
            .unwrap();
        run_flow_from_entry_node(
            &core,
            &flow,
            &flow.nodes[0],
            &input("INT.In", 20.3),
            &engine,
        )
        .await;
        assert_eq!(out(&core), TagValue::Float(30.0));
        run_flow_from_entry_node(
            &core,
            &flow,
            &flow.nodes[0],
            &input("INT.In", 20.6),
            &engine,
        )
        .await;
        assert_eq!(out(&core), TagValue::Float(20.6));
        // Écriture refusée ou perdue sur le tag cible (exception, coupure) : la mémoire du nœud
        // est oubliée et la même consigne est réécrite à l'évaluation suivante
        core.write_tag("INT.Out", TagValue::Float(30.0))
            .await
            .unwrap();
        core.flow_runtime.forget_outputs_for_tag("INT.Autre");
        assert_eq!(core.flow_runtime.last_outputs.len(), 1);
        core.flow_runtime.forget_outputs_for_tag("INT.Out");
        assert!(core.flow_runtime.last_outputs.is_empty());
        run_flow_from_entry_node(
            &core,
            &flow,
            &flow.nodes[0],
            &input("INT.In", 20.6),
            &engine,
        )
        .await;
        assert_eq!(out(&core), TagValue::Float(20.6));
        // Redéploiement : la mémoire du nœud est oubliée, la sortie est réécrite
        core.flow_runtime.forget_flow("f");
        assert!(core.flow_runtime.last_outputs.is_empty());
    }

    #[tokio::test]
    async fn failing_rhai_node_is_reported_once_until_recovery() {
        let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
        core.register_tag(internal_tag("INT.In", 0.0), None);
        let flow = FlowGraph {
            id: "f".into(),
            name: "f".into(),
            description: String::new(),
            enabled: true,
            nodes: vec![
                node(
                    "in",
                    FlowNodeKind::TagInput {
                        tag_id: "INT.In".into(),
                    },
                ),
                // Erreur d'exécution uniquement pour val > 10 (division d'un entier par zéro)
                node(
                    "rhai",
                    FlowNodeKind::RhaiScript {
                        script: "if val > 10.0 { 1 / 0 } else { val }".into(),
                    },
                ),
            ],
            edges: vec![edge("e", "in", "rhai")],
        };
        let engine = shared_engine(&core);
        let failing = |core: &ScadaCore| core.flow_runtime.failing_scripts.contains_key("f/rhai");
        run_flow_from_entry_node(
            &core,
            &flow,
            &flow.nodes[0],
            &input("INT.In", 12.0),
            &engine,
        )
        .await;
        assert!(failing(&core));
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &input("INT.In", 5.0), &engine)
            .await;
        assert!(!failing(&core), "retour à la normale");
    }

    #[tokio::test]
    async fn flow_does_not_execute_on_uncertain_or_baddisconnect_tag_input() {
        let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
        core.register_tag(internal_tag("INT.In", 0.0), None);
        core.register_tag(internal_tag("INT.Out", 0.0), None);
        let flow = FlowGraph {
            id: "f_quality".into(),
            name: "f_quality".into(),
            description: String::new(),
            enabled: true,
            nodes: vec![
                node(
                    "in",
                    FlowNodeKind::TagInput {
                        tag_id: "INT.In".into(),
                    },
                ),
                node(
                    "out",
                    FlowNodeKind::TagOutput {
                        target_tag_id: "INT.Out".into(),
                    },
                ),
            ],
            edges: vec![edge("e", "in", "out")],
        };
        let engine = shared_engine(&core);

        // Au démarrage, le tag est enregistré avec Quality::Uncertain et valeur 0.0
        let uncertain_input = TagState {
            tag_id: "INT.In".into(),
            value: TagValue::Float(0.0),
            quality: Quality::Uncertain,
            timestamp: Utc::now(),
        };
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &uncertain_input, &engine).await;
        // La sortie ne doit PAS avoir été écrite par le flux
        assert!(core.flow_runtime.last_outputs.is_empty());

        // Avec Quality::BadDisconnect, le flux ne doit pas non plus s'exécuter
        let bad_input = TagState {
            tag_id: "INT.In".into(),
            value: TagValue::Float(100.0),
            quality: Quality::BadDisconnect,
            timestamp: Utc::now(),
        };
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &bad_input, &engine).await;
        assert!(core.flow_runtime.last_outputs.is_empty());

        // Avec Quality::Good, le flux s'exécute et propage la valeur
        let good_input = TagState {
            tag_id: "INT.In".into(),
            value: TagValue::Float(25.0),
            quality: Quality::Good,
            timestamp: Utc::now(),
        };
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &good_input, &engine).await;
        assert_eq!(
            core.tags_state.get("INT.Out").unwrap().value,
            TagValue::Float(25.0)
        );
    }

    #[tokio::test]
    async fn rhai_functions_and_pid_controller_respect_tag_quality_and_pause() {
        let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
        // Tag avec qualité initiale Uncertain
        core.register_tag(internal_tag("INT.Mesure", 0.0), None);
        let engine = shared_engine(&core);

        // 1. Rhai : tag() et tag_bool() renvoient les valeurs de repli tant que la qualité n'est pas Good
        let val: f64 = engine.eval(r#"tag("INT.Mesure")"#).unwrap();
        assert_eq!(val, 0.0);
        let b: bool = engine.eval(r#"tag_bool("INT.Mesure")"#).unwrap();
        assert!(!b);

        // Mise à jour en Quality::Good
        core.tags_state.insert(
            "INT.Mesure".into(),
            TagState {
                tag_id: "INT.Mesure".into(),
                value: TagValue::Float(42.5),
                quality: Quality::Good,
                timestamp: Utc::now(),
            },
        );
        let val: f64 = engine.eval(r#"tag("INT.Mesure")"#).unwrap();
        assert_eq!(val, 42.5);

        // Pause de l'acquisition : tag() doit renvoyer 0.0
        core.set_acquisition_paused("test", true);
        let val: f64 = engine.eval(r#"tag("INT.Mesure")"#).unwrap();
        assert_eq!(val, 0.0);
        core.set_acquisition_paused("test", false);

        // 2. PidController : ne régule pas si le tag de consigne est en Uncertain
        core.register_tag(internal_tag("INT.SP", 0.0), None);
        core.register_tag(internal_tag("INT.PID_Out", 0.0), None);
        let flow_pid = FlowGraph {
            id: "f_pid".into(),
            name: "f_pid".into(),
            description: String::new(),
            enabled: true,
            nodes: vec![
                node(
                    "in",
                    FlowNodeKind::TagInput {
                        tag_id: "INT.Mesure".into(),
                    },
                ),
                node(
                    "pid",
                    FlowNodeKind::PidController {
                        setpoint_tag_id: "INT.SP".into(),
                        kp: 1.0,
                        ki: 0.0,
                        kd: 0.0,
                        min_out: 0.0,
                        max_out: 100.0,
                        reverse_acting: false,
                    },
                ),
                node(
                    "out",
                    FlowNodeKind::TagOutput {
                        target_tag_id: "INT.PID_Out".into(),
                    },
                ),
            ],
            edges: vec![edge("e1", "in", "pid"), edge("e2", "pid", "out")],
        };

        // Entrée mesure Good, mais consigne Uncertain : le bloc PID doit sauter sans écrire
        run_flow_from_entry_node(
            &core,
            &flow_pid,
            &flow_pid.nodes[0],
            &input("INT.Mesure", 42.5),
            &engine,
        )
        .await;
        assert!(core.flow_runtime.last_outputs.is_empty());

        // Consigne passe en Good
        core.tags_state.insert(
            "INT.SP".into(),
            TagState {
                tag_id: "INT.SP".into(),
                value: TagValue::Float(50.0),
                quality: Quality::Good,
                timestamp: Utc::now(),
            },
        );
        run_flow_from_entry_node(
            &core,
            &flow_pid,
            &flow_pid.nodes[0],
            &input("INT.Mesure", 42.5),
            &engine,
        )
        .await;
        assert!(!core.flow_runtime.last_outputs.is_empty());
    }

    #[tokio::test]
    async fn trigger_flow_now_refuses_when_acquisition_paused() {
        let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
        core.register_tag(internal_tag("INT.In", 0.0), None);
        let flow = FlowGraph {
            id: "f_trig".into(),
            name: "f_trig".into(),
            description: String::new(),
            enabled: true,
            nodes: vec![node(
                "in",
                FlowNodeKind::TagInput {
                    tag_id: "INT.In".into(),
                },
            )],
            edges: vec![],
        };
        core.flows.insert(flow.id.clone(), flow);

        // Si acquisition en pause -> erreur immédiate
        core.set_acquisition_paused("test", true);
        let res = trigger_flow_now(&core, "f_trig").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("pause"));

        // Hors pause, mais tag en Uncertain (démarrage) -> 0 nœud exécuté
        core.set_acquisition_paused("test", false);
        let count = trigger_flow_now(&core, "f_trig").await.unwrap();
        assert_eq!(count, 0);

        // Une fois le tag Good -> 1 nœud exécuté
        core.tags_state.insert(
            "INT.In".into(),
            TagState {
                tag_id: "INT.In".into(),
                value: TagValue::Float(10.0),
                quality: Quality::Good,
                timestamp: Utc::now(),
            },
        );
        let count = trigger_flow_now(&core, "f_trig").await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn delay_timer_propagates_false_on_falling_edge_and_before_timeout() {
        let core = ScadaCore::new(None, &crate::core::EngineOptions::default());
        core.register_tag(internal_bool_tag("INT.InBool"), None);
        core.register_tag(internal_bool_tag("INT.OutBool"), None);

        let flow = FlowGraph {
            id: "f_ton".into(),
            name: "f_ton".into(),
            description: String::new(),
            enabled: true,
            nodes: vec![
                node(
                    "in",
                    FlowNodeKind::TagInput {
                        tag_id: "INT.InBool".into(),
                    },
                ),
                node("ton", FlowNodeKind::DelayTimer { delay_ms: 50 }),
                node(
                    "out",
                    FlowNodeKind::TagOutput {
                        target_tag_id: "INT.OutBool".into(),
                    },
                ),
            ],
            edges: vec![edge("e1", "in", "ton"), edge("e2", "ton", "out")],
        };
        let engine = shared_engine(&core);

        let bool_input = |v: bool| TagState {
            tag_id: "INT.InBool".into(),
            value: TagValue::Bool(v),
            quality: Quality::Good,
            timestamp: Utc::now(),
        };
        let out_val = || {
            core.tags_state
                .get("INT.OutBool")
                .map(|s| s.value.clone())
                .unwrap_or(TagValue::Bool(false))
        };

        // 1. Front montant actif : le délai de 50 ms n'est pas écoulé -> sortie false propagée
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &bool_input(true), &engine).await;
        assert_eq!(out_val(), TagValue::Bool(false));

        // 2. Attente de l'écoulement du délai (> 50 ms)
        tokio::time::sleep(Duration::from_millis(60)).await;
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &bool_input(true), &engine).await;
        // Le temporisateur a expiré : la sortie doit basculer à true
        assert_eq!(out_val(), TagValue::Bool(true));

        // 3. Front descendant (active = false) : la sortie doit IMMÉDIATEMENT retomber à false
        run_flow_from_entry_node(&core, &flow, &flow.nodes[0], &bool_input(false), &engine).await;
        assert_eq!(
            out_val(),
            TagValue::Bool(false),
            "DelayTimer n'a pas propagé false lors de la retombée de l'entrée"
        );
    }
}
