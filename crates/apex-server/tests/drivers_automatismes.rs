//! Tests d'intégration « boîte noire » des garde-fous drivers et automatismes : le binaire
//! `apex-scada` est lancé dans un dossier temporaire neuf (mode portable, ports libres, mode sûr
//! actif) puis interrogé uniquement par HTTP.
//!
//! Aucune commande n'est envoyée à un équipement : les refus testés interviennent avant tout
//! envoi, et le mode sûr bloque de toute façon les consignes vers les drivers non simulés.

#[allow(dead_code)]
mod common;

use common::{Server, Settings, ADMIN};
use serde_json::{json, Value};

/// Driver Modbus TCP de la démonstration (automate embarqué de la simulation).
const MODBUS_DRIVER: &str = "MODBUS_GF01";

fn tag_count(srv: &Server, token: &str) -> usize {
    srv.call("GET", "/api/tags", Some(token), None)
        .json()
        .as_array()
        .map(Vec::len)
        .unwrap_or_default()
}

fn message(r: &common::Response) -> String {
    r.json()["message"].as_str().unwrap_or_default().to_string()
}

#[test]
fn auto_decouverte_refusee_sur_un_driver_reel() {
    let srv = Server::start("decouverte");
    let admin = srv.login(ADMIN);
    let before = tag_count(&srv, &admin);

    let r = srv.call(
        "POST",
        &format!("/api/drivers/{MODBUS_DRIVER}/browse"),
        Some(&admin),
        None,
    );
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("Auto-Découverte"), "{}", r.text());
    assert_eq!(tag_count(&srv, &admin), before, "aucune variable inventée");
}

#[test]
fn variable_modbus_inscriptible_en_lecture_seule_refusee() {
    let srv = Server::start("tag-ro");
    let admin = srv.login(ADMIN);
    let tag = |address: &str, writable: bool| -> Value {
        json!({
            "id": "MODBUS.GF01.Test_Adresse",
            "label": "Test",
            "module_source": MODBUS_DRIVER,
            "protocol_address": address,
            "data_type": "Float",
            "unit": null,
            "writable": writable,
            "deadband": 0.0,
            "scale_factor": 1.0,
            "scale_offset": 0.0,
            "historize": false,
            "alarm_config": { "enabled": false }
        })
    };
    for address in ["IR:4:FLOAT32", "DI:0"] {
        let r = srv.call("POST", "/api/tags", Some(&admin), Some(&tag(address, true)));
        assert_eq!(r.status, 400, "{address} : {}", r.text());
        assert!(message(&r).contains("lecture seule"), "{}", r.text());
    }
    // En lecture seule, la même adresse est acceptée
    let r = srv.call(
        "POST",
        "/api/tags",
        Some(&admin),
        Some(&tag("IR:4:FLOAT32", false)),
    );
    assert_eq!(r.status, 200, "{}", r.text());
}

#[test]
fn flux_avec_script_rhai_invalide_refuse() {
    let srv = Server::start("rhai");
    let admin = srv.login(ADMIN);
    let flow = json!({
        "id": "flux-test-rhai",
        "name": "Test Rhai",
        "description": "",
        "enabled": true,
        "nodes": [{
            "id": "n1", "label": "Calcul", "x": 0.0, "y": 0.0,
            "kind": { "node_type": "RhaiScript", "script": "let x = ;" }
        }],
        "edges": []
    });
    let r = srv.call("POST", "/api/flows", Some(&admin), Some(&flow));
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("Rhai"), "{}", r.text());
    let flows = srv.call("GET", "/api/flows", Some(&admin), None).json();
    assert!(
        !flows
            .as_array()
            .expect("liste des flux")
            .iter()
            .any(|f| f["id"] == "flux-test-rhai"),
        "flux invalide enregistré"
    );
}

#[test]
fn import_sans_simulation_ecarte_les_drivers_simules() {
    let settings = Settings {
        simulation: false,
        ..Settings::new()
    };
    let srv = Server::start_with("import-sans-simu", settings);
    let admin = srv.login(ADMIN);

    let sim_tag = json!({
        "id": "OPC_DEMO.Temp",
        "label": "Température",
        "module_source": "OPC_DEMO",
        "protocol_address": "ns=2;s=Temp",
        "data_type": "Float",
        "unit": null,
        "writable": false,
        "deadband": 0.0,
        "scale_factor": 1.0,
        "scale_offset": 0.0,
        "historize": false,
        "alarm_config": { "enabled": false }
    });
    let bundle = json!({
        "exported_at": "2026-10-17T00:00:00Z",
        "version": "1.1.0",
        "drivers": [{
            "id": "OPC_DEMO",
            "name": "OPC-UA de démonstration",
            "protocol": "OpcUa",
            "endpoint": "opc.tcp://127.0.0.1:4840",
            "enabled": true,
            "tags": [sim_tag.clone()]
        }],
        "tags": [sim_tag],
        "dashboards": [],
        "flows": [],
        "schedules": []
    });
    let r = srv.call("POST", "/api/system/import", Some(&admin), Some(&bundle));
    assert_eq!(r.status, 200, "{}", r.text());
    let report = r.json();
    assert_eq!(report["drivers"], 0, "{report}");
    assert_eq!(report["ignored_drivers"], 1, "{report}");
    assert_eq!(report["ignored_tags"], 1, "{report}");

    let drivers = srv.call("GET", "/api/drivers", Some(&admin), None).json();
    assert!(
        !drivers
            .as_array()
            .expect("liste des drivers")
            .iter()
            .any(|d| d["id"] == "OPC_DEMO"),
        "driver simulé enregistré sans simulation"
    );
    let tags = srv.call("GET", "/api/tags", Some(&admin), None).json();
    assert!(
        !tags
            .as_array()
            .expect("liste des tags")
            .iter()
            .any(|t| t["id"] == "OPC_DEMO.Temp"),
        "variable d'un driver simulé enregistrée sans simulation"
    );
}
