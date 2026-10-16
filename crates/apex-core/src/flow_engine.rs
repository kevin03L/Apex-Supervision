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
use tracing::{error, warn};

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
            ast_cache: DashMap::new(),
            engine: OnceLock::new(),
        }
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
                                if tag_state.quality == Quality::BadDisconnect || core.is_acquisition_paused() {
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

pub fn build_industrial_rhai_engine(core: Arc<ScadaCore>) -> Engine {
    let mut engine = Engine::new();
    // Bac à sable : bornes de calcul et de mémoire pour les scripts utilisateurs
    engine.set_max_operations(15_000);
    engine.set_max_string_size(4_096);
    engine.set_max_array_size(1_024);
    engine.set_max_map_size(256);
    engine.set_max_call_levels(16);
    engine.set_max_expr_depths(64, 32);

    let core_for_num = Arc::clone(&core);
    engine.register_fn("tag", move |tag_id: &str| -> f64 {
        core_for_num
            .tags_state
            .get(tag_id)
            .and_then(|s| s.value.as_f64())
            .unwrap_or(0.0)
    });

    let core_for_bool = Arc::clone(&core);
    engine.register_fn("tag_bool", move |tag_id: &str| -> bool {
        core_for_bool
            .tags_state
            .get(tag_id)
            .and_then(|s| s.value.as_bool())
            .unwrap_or(false)
    });

    let core_for_text = Arc::clone(&core);
    engine.register_fn("tag_text", move |tag_id: &str| -> String {
        core_for_text
            .tags_state
            .get(tag_id)
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
                run_flow_from_entry_node(core, &flow, node, &st, &rhai_engine).await;
                executed += 1;
            }
        }
    }

    Ok(executed)
}

async fn evaluate_all_enabled_flows_once(core: &Arc<ScadaCore>, rhai_engine: &Arc<Engine>) {
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
                    if st.quality != Quality::BadDisconnect {
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
    // Seuls les flux qui lisent ce tag sont copiés (évite de cloner tous les graphes à chaque COV)
    let listens_to_tag = |f: &FlowGraph| {
        f.nodes.iter().any(|n| {
            matches!(&n.kind, FlowNodeKind::TagInput { tag_id } if tag_id == &tag_state.tag_id)
        })
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
                let sp = core
                    .tags_state
                    .get(setpoint_tag_id)
                    .and_then(|s| s.value.as_f64())
                    .unwrap_or(21.0);

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
                let active = current_val.as_bool().unwrap_or(true);
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
                        continue;
                    }
                } else {
                    core.flow_runtime
                        .timer_since
                        .insert(current_node.id.clone(), None);
                    continue;
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
                match eval_rhai_script(rhai_engine, script, &current_val, core) {
                    Ok(computed) => next_value = computed,
                    Err(err) => {
                        error!(
                            "Erreur Rhai dans le nœud '{}' : {}",
                            current_node.label, err
                        );
                        continue;
                    }
                }
            }

            FlowNodeKind::TagOutput { target_tag_id } => {
                if target_tag_id != &tag_state.tag_id {
                    let should_write = match core.tags_state.get(target_tag_id) {
                        Some(existing) => next_value.has_significant_change(&existing.value, 0.08),
                        None => true,
                    };
                    if should_write {
                        match core.write_tag(target_tag_id, next_value.clone()).await {
                            Ok(_) => {
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
        assert!(!eval_threshold(1.85, ComparisonOperator::Lt, 1.8, 0.3, false));
        assert!(eval_threshold(1.79, ComparisonOperator::Lt, 1.8, 0.3, false));
        assert!(eval_threshold(2.0, ComparisonOperator::Lt, 1.8, 0.3, true));
        assert!(!eval_threshold(2.15, ComparisonOperator::Lt, 1.8, 0.3, true));
    }

    #[test]
    fn threshold_greater_and_equality() {
        assert!(eval_threshold(25.1, ComparisonOperator::Gt, 25.0, 0.5, false));
        assert!(eval_threshold(24.7, ComparisonOperator::Gt, 25.0, 0.5, true));
        assert!(!eval_threshold(24.4, ComparisonOperator::Gt, 25.0, 0.5, true));
        assert!(eval_threshold(3.0, ComparisonOperator::Eq, 3.0, 0.0, false));
        assert!(eval_threshold(3.1, ComparisonOperator::Neq, 3.0, 0.0, false));
    }
}
