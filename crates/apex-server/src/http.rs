//! Serveur web : routes REST, WebSocket temps réel, authentification et en-têtes de sécurité.
//!
//! - Toute l'API et le WebSocket exigent une session (RBAC Administrateur / Superviseur / Visualisation).
//! - Seuls `/api/health` et `/api/auth/login` sont publics.
//! - L'IHM web (fichiers statiques) est servie à la même origine que l'API.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path as FsPath,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use apex_core::{
    core::{ScadaCore, APP_VERSION},
    flow_engine::trigger_flow_now,
    models::{
        AckAlarmRequest, AlarmInstance, AlarmJournalEntry, AuditLogEntry, ChangePasswordRequest,
        DashboardLayout, DriverConfig, FlowGraph, HistoryResponse, ImportSymbolLibraryRequest,
        LoginRequest, LoginResponse, ProjectBackupBundle, ProtocolType, RegisteredTag, ScadaError,
        ScheduleRule, SymbolLibraryItem, SystemStatus, TagFullView, TagState, UpsertUserRequest,
        UserAccount, UserPublicView, UserRole, WriteTagRequest, WsServerMessage,
    },
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, DefaultBodyLimit, Path, Query, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Extension, Json, Router,
};
use chrono::{DateTime, Utc};
use tower_http::services::ServeDir;
use tracing::info;

/// Informations d'exécution publiées (en boucle locale) pour l'utilitaire desktop.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeInfo {
    pub instance_name: String,
    /// Empreinte de la configuration du fichier chargée au démarrage (avant les surcharges
    /// d'environnement) : l'utilitaire la compare au fichier sur disque pour savoir si un
    /// redémarrage est nécessaire.
    pub config_fingerprint: String,
    /// Surcharges `SCADA_*` appliquées (compatibilité 1.x).
    pub env_overrides: Vec<String>,
    pub config_file: String,
    pub data_dir: String,
    pub log_dir: String,
    pub web_dir: String,
    /// `service` (gestionnaire de services) ou `manual`.
    pub run_mode: String,
    pub pid: u32,
    pub safe_mode: bool,
    pub simulation_enabled: bool,
    pub full_version: String,
}

#[derive(Clone)]
pub struct AppState {
    pub core: Arc<ScadaCore>,
    pub runtime: Arc<RuntimeInfo>,
}

/// Erreur d'API : convertit une erreur du moteur en réponse HTTP JSON.
pub struct ApiError(pub ScadaError);

impl From<ScadaError> for ApiError {
    fn from(e: ScadaError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::json!({
            "error": true,
            "message": self.0.to_string(),
        });
        (status, Json(body)).into_response()
    }
}

/// Utilisateur authentifié, injecté dans la requête par le middleware d'authentification.
#[derive(Clone)]
struct CurrentUser(UserAccount);

/// Jeton brut de la requête (utile pour la déconnexion et le changement de mot de passe).
#[derive(Clone)]
struct AuthToken(String);

/// Taille maximale d'une archive projet importée : l'export, sans limite, duplique les tags
/// (dans chaque driver et dans le catalogue) et embarque les SVG des synoptiques.
pub const PROJECT_IMPORT_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Taille maximale d'une bibliothèque de symboles importée (SVG, draw.io, JSON).
pub const SYMBOL_IMPORT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Construit le routeur complet (API protégée, routes publiques, IHM statique).
///
/// Les autres routes gardent la limite par défaut d'axum (2 Mo par corps de requête).
pub fn router(state: AppState, web_dir: &FsPath) -> Router {
    // Routes protégées : session obligatoire (contrôle des rôles dans chaque handler)
    let protected = Router::new()
        // --- AUTHENTIFICATION & UTILISATEURS ---
        .route("/api/auth/logout", post(api_auth_logout))
        .route("/api/auth/me", get(api_auth_me))
        .route("/api/auth/password", post(api_auth_change_password))
        .route("/api/users", get(api_get_users).post(api_upsert_user))
        .route("/api/users/:username", delete(api_delete_user))
        // --- BIBLIOTHÈQUES DE SYMBOLES ---
        .route("/api/symbols", get(api_get_symbols))
        .route(
            "/api/symbols/import",
            post(api_import_symbols)
                .layer(DefaultBodyLimit::max(SYMBOL_IMPORT_MAX_BYTES))
                .layer(middleware::from_fn(supervisor_before_body)),
        )
        .route(
            "/api/symbols/install-pack/:pack_id",
            post(api_install_symbol_pack),
        )
        .route("/api/symbols/:id", delete(api_delete_symbol))
        // --- CONTRÔLE SYSTÈME ---
        .route("/api/system/status", get(api_system_status))
        .route(
            "/api/system/settings",
            get(api_get_settings).put(api_update_settings),
        )
        .route("/api/system/pause", post(api_system_pause))
        .route("/api/system/standby", post(api_system_standby))
        .route("/api/system/shutdown", post(api_system_shutdown))
        .route("/api/system/restart", post(api_system_restart))
        .route("/api/system/export", get(api_system_export))
        .route(
            "/api/system/import",
            post(api_system_import)
                .layer(DefaultBodyLimit::max(PROJECT_IMPORT_MAX_BYTES))
                .layer(middleware::from_fn(admin_before_body)),
        )
        .route("/api/audit", get(api_get_audit_logs))
        // --- TAG DATABASE ---
        .route("/api/tags", get(api_get_tags).post(api_upsert_tag))
        .route("/api/tags/:id", delete(api_delete_tag))
        .route("/api/tags/:id/write", post(api_write_tag))
        // --- DRIVERS ---
        .route("/api/drivers", get(api_get_drivers).post(api_add_driver))
        .route(
            "/api/drivers/:id",
            delete(api_delete_driver).put(api_update_driver),
        )
        .route("/api/drivers/:id/start", post(api_start_driver))
        .route("/api/drivers/:id/stop", post(api_stop_driver))
        .route("/api/drivers/:id/restart", post(api_restart_driver))
        .route("/api/drivers/:id/browse", post(api_browse_driver))
        // --- SYNOPTIQUES ---
        .route(
            "/api/dashboards",
            get(api_get_dashboards).post(api_save_dashboard),
        )
        .route("/api/dashboards/:id", delete(api_delete_dashboard))
        // --- FLUX ---
        .route("/api/flows", get(api_get_flows).post(api_save_flow))
        .route("/api/flows/:id", delete(api_delete_flow))
        .route("/api/flows/:id/trigger", post(api_trigger_flow))
        // --- ALARMES ---
        .route("/api/alarms", get(api_get_alarms))
        .route("/api/alarms/journal", get(api_get_alarm_journal))
        .route("/api/alarms/:id/ack", post(api_ack_alarm))
        .route("/api/alarms/:id/shelve", post(api_shelve_alarm))
        .route("/api/alarms/:id/unshelve", post(api_unshelve_alarm))
        .route("/api/alarms/ack-all", post(api_ack_all_alarms))
        .route("/api/alarms/clear-normal", post(api_clear_normal_alarms))
        // --- PLANIFICATEUR ---
        .route(
            "/api/schedules",
            get(api_get_schedules).post(api_save_schedule),
        )
        .route("/api/schedules/:id", delete(api_delete_schedule))
        // --- HISTORIAN ---
        .route("/api/history/purge", post(api_purge_history))
        .route("/api/history/:tag_id", get(api_get_history))
        .route("/api/history/:tag_id/csv", get(api_export_history_csv))
        // --- WEBSOCKET TEMPS RÉEL ---
        .route("/ws/realtime", get(ws_realtime_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        // Routes publiques
        .route("/api/health", get(api_health))
        .route("/api/auth/login", post(api_auth_login))
        .merge(protected)
        // IHM statique (même origine que l'API : aucun CORS nécessaire)
        .fallback_service(ServeDir::new(web_dir))
        .layer(middleware::from_fn(security_headers_middleware))
        .with_state(state)
}

// ============================================================================
// MIDDLEWARES : AUTHENTIFICATION & EN-TÊTES DE SÉCURITÉ
// ============================================================================

fn extract_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-scada-token")
        .or_else(|| headers.get(header::AUTHORIZATION))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Le navigateur ne peut pas ajouter d'en-tête à une connexion WebSocket : le jeton y est
/// accepté en paramètre `?token=` (et uniquement pour le WebSocket).
fn query_token(req: &Request) -> Option<String> {
    if !req.uri().path().starts_with("/ws/") {
        return None;
    }
    req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .map(|t| t.to_string())
    })
}

/// En-têtes ajoutés par un proxy inverse (nginx, Caddy, Apache, IIS/ARR...).
const PROXY_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
];

/// Requête émise depuis le poste lui-même : pair en boucle locale **et** aucun en-tête de proxy
/// inverse. Derrière un proxy installé sur le même poste, toutes les requêtes arrivent de
/// 127.0.0.1 : l'adresse du pair ne suffit donc pas à prouver qu'elles sont locales.
fn is_direct_local_peer(peer: &SocketAddr, headers: &HeaderMap) -> bool {
    peer.ip().to_canonical().is_loopback()
        && !PROXY_HEADERS.iter().any(|h| headers.contains_key(*h))
}

/// Seules routes ouvertes au jeton de contrôle local (utilitaire desktop) : arrêt propre et
/// lecture de l'état du moteur.
const LOCAL_TOKEN_ROUTES: &[(&str, &str)] = &[
    ("POST", "/api/system/shutdown"),
    ("GET", "/api/system/status"),
];

fn session_required() -> ScadaError {
    ScadaError::Unauthorized("Session absente ou expirée : veuillez vous reconnecter".to_string())
}

/// Acteur d'une requête présentant le jeton de contrôle local. Le jeton n'est valable que
/// depuis le poste lui-même (boucle locale, sans proxy) et pour [`LOCAL_TOKEN_ROUTES`] : il
/// n'ouvre jamais l'administration, et l'acteur est l'utilisateur interne « Utilitaire local ».
fn local_control_user(req: &Request, peer: &SocketAddr) -> Result<UserAccount, ScadaError> {
    if !is_direct_local_peer(peer, req.headers()) {
        // Même réponse qu'un jeton inconnu : rien n'indique qu'il s'agit du jeton local
        return Err(session_required());
    }
    let method = req.method().as_str();
    let path = req.uri().path();
    if !LOCAL_TOKEN_ROUTES
        .iter()
        .any(|(m, p)| *m == method && *p == path)
    {
        return Err(ScadaError::Forbidden(
            "Le jeton de contrôle local n'autorise que l'arrêt propre du moteur et la lecture de son état"
                .to_string(),
        ));
    }
    Ok(ScadaCore::local_utility_account())
}

async fn auth_middleware(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = extract_token(req.headers()).or_else(|| query_token(&req));
    let user = if state.core.is_local_control_token(token.as_deref()) {
        local_control_user(&req, &peer)?
    } else {
        state
            .core
            .resolve_user(token.as_deref())
            .ok_or_else(session_required)?
    };
    req.extensions_mut().insert(CurrentUser(user));
    req.extensions_mut()
        .insert(AuthToken(token.unwrap_or_default()));
    Ok(next.run(req).await)
}

/// En-têtes de durcissement : pas de sniffing MIME, pas d'intégration en iframe (anti-clickjacking),
/// pas de fuite de référent, et pas de mise en cache des réponses API.
async fn security_headers_middleware(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if is_api {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    } else {
        // Politique de sécurité du contenu : aucune ressource ni connexion vers un site tiers.
        // `unsafe-inline` reste nécessaire tant que l'IHM utilise des gestionnaires onclick en ligne.
        h.insert(
            "content-security-policy",
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data: blob:; connect-src 'self' ws: wss:; object-src 'none'; \
                 base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
            ),
        );
    }
    res
}

/// Routes à gros corps (import de projet, de symboles) : le rôle est contrôlé AVANT la lecture
/// du corps, sinon un compte Visualisation ferait lire et analyser jusqu'à la limite élargie
/// par requête avant de recevoir son 403.
async fn admin_before_body(req: Request, next: Next) -> Result<Response, ApiError> {
    let user = req
        .extensions()
        .get::<CurrentUser>()
        .ok_or_else(|| ScadaError::Unauthorized("Session requise".into()))?;
    require_admin(user)?;
    Ok(next.run(req).await)
}

async fn supervisor_before_body(req: Request, next: Next) -> Result<Response, ApiError> {
    let user = req
        .extensions()
        .get::<CurrentUser>()
        .ok_or_else(|| ScadaError::Unauthorized("Session requise".into()))?;
    require_supervisor(user)?;
    Ok(next.run(req).await)
}

fn require_admin(user: &CurrentUser) -> Result<(), ScadaError> {
    ScadaCore::require_admin(&user.0)
}

fn require_supervisor(user: &CurrentUser) -> Result<(), ScadaError> {
    ScadaCore::require_supervisor_or_admin(&user.0)
}

fn is_viewer(user: &CurrentUser) -> bool {
    user.0.role == UserRole::Viewer
}

// ============================================================================
// HANDLERS PUBLICS : SANTÉ & CONNEXION
// ============================================================================

/// État de santé (public). Depuis le poste local uniquement (boucle locale sans en-tête de
/// proxy inverse), les informations d'exécution (configuration chargée, répertoires, mode) sont
/// ajoutées pour l'utilitaire desktop.
async fn api_health(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let st = state.core.get_system_status();
    let mut body = serde_json::json!({
        "status": "ok",
        "version": APP_VERSION,
        "uptime_seconds": st.uptime_seconds,
        "acquisition_paused": st.acquisition_paused,
        "running_drivers": st.running_drivers,
    });
    // Détail d'exécution (chemins, PID, mode) : seulement pour le porteur du jeton de contrôle
    // local, en connexion directe. L'adresse seule ne suffit pas : un proxy inverse local qui
    // n'ajoute pas d'en-tête de transfert, ou une page web (DNS rebinding), arrivent aussi de
    // la boucle locale, mais ne peuvent pas lire le fichier du jeton.
    if is_direct_local_peer(&peer, &headers)
        && state
            .core
            .is_local_control_token(extract_token(&headers).as_deref())
    {
        body["runtime"] = serde_json::to_value(state.runtime.as_ref()).unwrap_or_default();
        body["started_at"] = serde_json::to_value(st.started_at).unwrap_or_default();
        body["total_drivers"] = st.total_drivers.into();
        body["active_alarms"] = st.active_alarms.into();
    }
    Json(body)
}

async fn api_auth_login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let resp = state.core.login(req).await?;
    Ok(Json(resp))
}

// ============================================================================
// HANDLERS REST : AUTHENTIFICATION & UTILISATEURS
// ============================================================================

async fn api_auth_logout(
    State(state): State<AppState>,
    Extension(token): Extension<AuthToken>,
) -> Json<serde_json::Value> {
    state.core.logout(&token.0);
    Json(serde_json::json!({ "logged_out": true }))
}

async fn api_auth_me(Extension(user): Extension<CurrentUser>) -> Json<UserPublicView> {
    Json(user.0.to_public_view())
}

async fn api_auth_change_password(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Extension(token): Extension<AuthToken>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .core
        .change_own_password(&user.0, &token.0, &req.current_password, &req.new_password)
        .await?;
    Ok(Json(serde_json::json!({ "changed": true })))
}

async fn api_get_users(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<UserPublicView>>, ApiError> {
    require_admin(&user)?;
    Ok(Json(state.core.get_all_users()))
}

async fn api_upsert_user(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Extension(token): Extension<AuthToken>,
    Json(req): Json<UpsertUserRequest>,
) -> Result<Json<UserPublicView>, ApiError> {
    require_admin(&user)?;
    let view = state
        .core
        .upsert_user(&user.0.username, req, Some(&token.0))
        .await?;
    Ok(Json(view))
}

async fn api_delete_user(
    Path(username): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&user)?;
    state.core.delete_user(&user.0.username, &username).await?;
    Ok(Json(serde_json::json!({ "deleted": username })))
}

// ============================================================================
// HANDLERS REST : BIBLIOTHÈQUES DE SYMBOLES INDUSTRIELS
// ============================================================================

async fn api_get_symbols(State(state): State<AppState>) -> Json<Vec<SymbolLibraryItem>> {
    Json(state.core.get_all_symbols())
}

async fn api_import_symbols(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<ImportSymbolLibraryRequest>,
) -> Result<Json<Vec<SymbolLibraryItem>>, ApiError> {
    require_supervisor(&user)?;
    let imported = state
        .core
        .import_symbol_library(&user.0.username, req)
        .await?;
    Ok(Json(imported))
}

async fn api_install_symbol_pack(
    Path(pack_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<SymbolLibraryItem>>, ApiError> {
    require_supervisor(&user)?;
    let items = state
        .core
        .install_builtin_symbol_pack(&user.0.username, &pack_id)
        .await?;
    Ok(Json(items))
}

async fn api_delete_symbol(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_supervisor(&user)?;
    state.core.delete_symbol(&user.0.username, &id).await?;
    Ok(Json(serde_json::json!({ "deleted": id })))
}

// ============================================================================
// HANDLERS REST : SYSTÈME (RÉSERVÉS AU GROUPE ADMINISTRATEUR)
// ============================================================================

async fn api_get_settings(
    State(state): State<AppState>,
) -> Json<apex_core::models::SystemSettings> {
    Json(state.core.get_settings())
}

async fn api_update_settings(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<apex_core::models::SystemSettings>,
) -> Result<Json<apex_core::models::SystemSettings>, ApiError> {
    require_admin(&user)?;
    Ok(Json(
        state.core.update_settings(&user.0.username, req).await?,
    ))
}

async fn api_system_status(State(state): State<AppState>) -> Json<SystemStatus> {
    Json(state.core.get_system_status())
}

#[derive(serde::Deserialize)]
struct PauseRequest {
    paused: bool,
}

async fn api_system_pause(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<PauseRequest>,
) -> Result<Json<SystemStatus>, ApiError> {
    require_admin(&user)?;
    state
        .core
        .set_acquisition_paused(&user.0.username, req.paused);
    Ok(Json(state.core.get_system_status()))
}

/// Met le moteur SCADA hors tension (arrêt de tous les drivers + pause acquisition) tout en gardant l'IHM prête pour un rallumage.
async fn api_system_standby(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<SystemStatus>, ApiError> {
    require_admin(&user)?;
    state.core.power_off_all_systems(&user.0.username);
    Ok(Json(state.core.get_system_status()))
}

/// Arrête complètement le processus serveur.
async fn api_system_shutdown(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&user)?;
    state.core.record_audit(
        &user.0.username,
        "SERVEUR",
        "SHUTDOWN",
        "Apex Supervision",
        "Extinction complète du processus serveur demandée",
    );
    state.core.restart_requested.store(false, Ordering::SeqCst);
    let notify = Arc::clone(&state.core.shutdown_notify);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        notify.notify_waiters();
    });
    Ok(Json(serde_json::json!({
        "status": "shutting_down",
        "message": "Arrêt propre du serveur Apex Supervision en cours..."
    })))
}

/// Rallume le moteur SCADA et redémarre les drivers actifs.
async fn api_system_restart(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<SystemStatus>, ApiError> {
    require_admin(&user)?;
    info!(
        "Redémarrage / Rallumage du moteur SCADA demandé par '{}'",
        user.0.username
    );
    state.core.restart_all_systems(&user.0.username).await;
    Ok(Json(state.core.get_system_status()))
}

async fn api_system_export(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ProjectBackupBundle>, ApiError> {
    require_admin(&user)?;
    state.core.record_audit(
        &user.0.username,
        "SYSTÈME",
        "EXPORT_PROJECT",
        "Backup JSON",
        "Export de l'archive projet",
    );
    Ok(Json(state.core.export_project_bundle()))
}

async fn api_system_import(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(bundle): Json<ProjectBackupBundle>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&user)?;
    let report = state
        .core
        .import_project_bundle(&user.0.username, bundle)
        .await?;
    Ok(Json(report))
}

async fn api_get_audit_logs(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<AuditLogEntry>>, ApiError> {
    require_supervisor(&user)?;
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(100);
    let logs = state.core.query_audit_logs(limit).await?;
    Ok(Json(logs))
}

// ============================================================================
// HANDLERS REST : TAGS & DRIVERS (ADMIN & SUPERVISEUR)
// ============================================================================

async fn api_get_tags(State(state): State<AppState>) -> Json<Vec<TagFullView>> {
    Json(state.core.get_all_tags())
}

async fn api_upsert_tag(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(tag): Json<RegisteredTag>,
) -> Result<Json<TagFullView>, ApiError> {
    require_supervisor(&user)?;
    let view = state.core.upsert_tag(&user.0.username, tag).await?;
    Ok(Json(view))
}

async fn api_delete_tag(
    Path(tag_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_supervisor(&user)?;
    state.core.delete_tag(&user.0.username, &tag_id).await?;
    Ok(Json(serde_json::json!({ "deleted": tag_id })))
}

async fn api_write_tag(
    Path(tag_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(payload): Json<WriteTagRequest>,
) -> Result<Json<TagState>, ApiError> {
    require_supervisor(&user)?;
    let val_str = serde_json::to_string(&payload.value).unwrap_or_default();
    let updated = state.core.write_tag(&tag_id, payload.value).await?;
    let note = payload
        .operator_note
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .map(|n| format!(" — note : {}", n.trim()))
        .unwrap_or_default();
    state.core.record_audit(
        &user.0.username,
        "CONSIGNE",
        "WRITE_TAG",
        &tag_id,
        &format!("Nouvelle valeur commandée : {}{}", val_str, note),
    );
    Ok(Json(updated))
}

async fn api_get_drivers(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<DriverConfig>>, ApiError> {
    require_supervisor(&user)?;
    Ok(Json(state.core.get_all_drivers()))
}

async fn api_add_driver(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(config): Json<DriverConfig>,
) -> Result<Json<Vec<TagFullView>>, ApiError> {
    require_supervisor(&user)?;
    if state.core.drivers_config.contains_key(&config.id) {
        return Err(ScadaError::BadRequest(format!(
            "Un driver '{}' existe déjà : supprimez-le ou choisissez un autre identifiant",
            config.id
        ))
        .into());
    }
    if config.protocol.is_simulated() && !state.core.simulation_enabled {
        return Err(ScadaError::BadRequest(simulation_only_message(&config.protocol)).into());
    }
    let views = state
        .core
        .register_and_start_driver(&user.0.username, config)
        .await?;
    Ok(Json(views))
}

/// Refus d'un driver réservé à la simulation alors qu'elle est désactivée.
fn simulation_only_message(protocol: &ProtocolType) -> String {
    let what = match protocol {
        ProtocolType::Simulator => "Le simulateur GTB est un driver de démonstration".to_string(),
        other => format!("Le protocole {other:?} n'a pas encore de driver réel (valeurs simulées)"),
    };
    format!(
        "{what} : il n'est disponible que lorsque la simulation est active \
         ([simulation] enabled = true dans la configuration du moteur)"
    )
}

async fn api_update_driver(
    Path(driver_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<apex_core::models::DriverUpdateRequest>,
) -> Result<Json<DriverConfig>, ApiError> {
    require_supervisor(&user)?;
    let cfg = state
        .core
        .update_driver(&user.0.username, &driver_id, req)
        .await?;
    Ok(Json(cfg))
}

async fn api_start_driver(
    Path(driver_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<DriverConfig>, ApiError> {
    require_supervisor(&user)?;
    let cfg = state
        .core
        .start_or_restart_driver(&user.0.username, &driver_id)
        .await?;
    Ok(Json(cfg))
}

async fn api_stop_driver(
    Path(driver_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<DriverConfig>, ApiError> {
    require_supervisor(&user)?;
    let cfg = state.core.stop_driver(&user.0.username, &driver_id).await?;
    Ok(Json(cfg))
}

async fn api_restart_driver(
    Path(driver_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<DriverConfig>, ApiError> {
    require_supervisor(&user)?;
    let cfg = state
        .core
        .start_or_restart_driver(&user.0.username, &driver_id)
        .await?;
    Ok(Json(cfg))
}

async fn api_delete_driver(
    Path(driver_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_supervisor(&user)?;
    let removed_tags = state
        .core
        .delete_driver(&user.0.username, &driver_id)
        .await?;
    Ok(Json(
        serde_json::json!({ "deleted": driver_id, "deleted_tags": removed_tags }),
    ))
}

async fn api_browse_driver(
    Path(driver_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<TagFullView>>, ApiError> {
    require_supervisor(&user)?;
    // Refusée (400) pour les drivers réels et sans simulation active : voir discover_driver_tags
    let views = state
        .core
        .discover_driver_tags(&user.0.username, &driver_id)
        .await?;
    Ok(Json(views))
}

// ============================================================================
// HANDLERS REST : DASHBOARDS, FLUX, ALARMES, SCHEDULES & HISTORIAN
// ============================================================================

async fn api_get_dashboards(State(state): State<AppState>) -> Json<Vec<DashboardLayout>> {
    Json(state.core.get_all_dashboards())
}

async fn api_save_dashboard(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(layout): Json<DashboardLayout>,
) -> Result<Json<DashboardLayout>, ApiError> {
    require_supervisor(&user)?;
    let saved = state.core.save_dashboard(&user.0.username, layout).await?;
    Ok(Json(saved))
}

async fn api_delete_dashboard(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_supervisor(&user)?;
    state.core.delete_dashboard(&user.0.username, &id).await?;
    Ok(Json(serde_json::json!({ "deleted": id })))
}

async fn api_get_flows(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<FlowGraph>>, ApiError> {
    require_supervisor(&user)?;
    Ok(Json(state.core.get_all_flows()))
}

async fn api_save_flow(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(flow): Json<FlowGraph>,
) -> Result<Json<FlowGraph>, ApiError> {
    require_supervisor(&user)?;
    let saved = state.core.save_flow(&user.0.username, flow).await?;
    Ok(Json(saved))
}

async fn api_delete_flow(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_supervisor(&user)?;
    state.core.delete_flow(&user.0.username, &id).await?;
    Ok(Json(serde_json::json!({ "deleted": id })))
}

async fn api_trigger_flow(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_supervisor(&user)?;
    let count = trigger_flow_now(&state.core, &id)
        .await
        .map_err(ScadaError::FlowError)?;
    state.core.record_audit(
        &user.0.username,
        "FLOW_ENGINE",
        "TRIGGER_FLOW",
        &id,
        &format!("Exécution manuelle ({} entrée(s))", count),
    );
    Ok(Json(serde_json::json!({ "triggered_nodes": count })))
}

async fn api_get_alarms(State(state): State<AppState>) -> Json<Vec<AlarmInstance>> {
    Json(state.core.get_all_alarms())
}

async fn api_get_alarm_journal(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<AlarmJournalEntry>>, ApiError> {
    require_supervisor(&user)?;
    let list = state.core.query_alarm_journal(150).await?;
    Ok(Json(list))
}

async fn api_ack_alarm(
    Path(alarm_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<AckAlarmRequest>,
) -> Result<Json<AlarmInstance>, ApiError> {
    require_supervisor(&user)?;
    // L'opérateur tracé est toujours l'utilisateur authentifié (traçabilité ISA-18.2)
    let comment = req.comment.filter(|c| !c.trim().is_empty());
    let updated = state
        .core
        .acknowledge_alarm(&alarm_id, &user.0.username, comment)?;
    Ok(Json(updated))
}

async fn api_shelve_alarm(
    Path(alarm_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<apex_core::models::ShelveAlarmRequest>,
) -> Result<Json<AlarmInstance>, ApiError> {
    require_supervisor(&user)?;
    let updated = state
        .core
        .shelve_alarm(&alarm_id, &user.0.username, req.minutes, req.comment)?;
    Ok(Json(updated))
}

async fn api_unshelve_alarm(
    Path(alarm_id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<AlarmInstance>, ApiError> {
    require_supervisor(&user)?;
    Ok(Json(
        state.core.unshelve_alarm(&alarm_id, &user.0.username)?,
    ))
}

async fn api_ack_all_alarms(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<AlarmInstance>>, ApiError> {
    require_supervisor(&user)?;
    Ok(Json(state.core.acknowledge_all_alarms(&user.0.username)))
}

async fn api_clear_normal_alarms(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<AlarmInstance>>, ApiError> {
    require_supervisor(&user)?;
    Ok(Json(state.core.clear_normal_alarms(&user.0.username)))
}

async fn api_get_schedules(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Vec<ScheduleRule>>, ApiError> {
    require_supervisor(&user)?;
    Ok(Json(state.core.get_all_schedules()))
}

async fn api_save_schedule(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(rule): Json<ScheduleRule>,
) -> Result<Json<ScheduleRule>, ApiError> {
    require_supervisor(&user)?;
    let saved = state.core.save_schedule(&user.0.username, rule).await?;
    Ok(Json(saved))
}

async fn api_delete_schedule(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_supervisor(&user)?;
    state.core.delete_schedule(&user.0.username, &id).await?;
    Ok(Json(serde_json::json!({ "deleted": id })))
}

/// Limite de points et période optionnelle `[from, to]` d'une lecture d'historique.
type HistoryParams = (u32, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Paramètres de lecture de l'historique : `limit`, `from` et `to` (ISO 8601 / RFC 3339).
fn parse_history_params(
    params: &HashMap<String, String>,
    default_limit: u32,
) -> Result<HistoryParams, ScadaError> {
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(default_limit);
    let parse_date = |key: &str| -> Result<Option<DateTime<Utc>>, ScadaError> {
        match params.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(raw) => DateTime::parse_from_rfc3339(raw)
                .map(|d| Some(d.with_timezone(&Utc)))
                .map_err(|_| {
                    ScadaError::BadRequest(format!(
                        "Date '{}' invalide (format attendu : 2026-10-16T08:00:00Z)",
                        raw
                    ))
                }),
        }
    };
    let from = parse_date("from")?;
    let to = parse_date("to")?;
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(ScadaError::BadRequest(
                "La date de début doit précéder la date de fin".to_string(),
            ));
        }
    }
    Ok((limit, from, to))
}

async fn api_get_history(
    Path(tag_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<HistoryResponse>, ApiError> {
    require_supervisor(&user)?;
    let (limit, from, to) = parse_history_params(&params, 200)?;
    let res = state
        .core
        .query_tag_history(&tag_id, limit, from, to)
        .await?;
    Ok(Json(res))
}

async fn api_purge_history(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&user)?;
    let deleted = state.core.purge_history(&user.0.username).await?;
    Ok(Json(serde_json::json!({ "purged_rows": deleted })))
}

async fn api_export_history_csv(
    Path(tag_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<impl IntoResponse, ApiError> {
    require_supervisor(&user)?;
    let (limit, from, to) = parse_history_params(&params, 5000)?;
    let hist = state
        .core
        .query_tag_history(&tag_id, limit, from, to)
        .await?;
    let mut csv = String::from("Timestamp_UTC;Tag_ID;Value;Quality\n");
    for pt in hist.points {
        let val_str = match pt.value {
            apex_core::models::TagValue::Float(f) => format!("{:.3}", f),
            apex_core::models::TagValue::Int(i) => i.to_string(),
            apex_core::models::TagValue::Bool(b) => (if b { 1 } else { 0 }).to_string(),
            // Neutralise l'injection de formules dans Excel (=, +, -, @)
            apex_core::models::TagValue::Text(s) => {
                let s = s.replace([';', '\n', '\r'], " ");
                if s.starts_with(['=', '+', '-', '@']) {
                    format!("'{}", s)
                } else {
                    s
                }
            }
        };
        csv.push_str(&format!(
            "{};{};{};{}\n",
            pt.timestamp, hist.tag_id, val_str, pt.quality
        ));
    }

    let filename = format!(
        "attachment; filename=\"historian_{}.csv\"",
        apex_core::security::slugify_id(&tag_id)
    );
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, filename),
        ],
        csv,
    ))
}

// ============================================================================
// WEBSOCKET TEMPS RÉEL (/ws/realtime?token=...)
// ============================================================================

async fn ws_realtime_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Extension(token): Extension<AuthToken>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_realtime_socket(socket, state, user, token.0))
}

/// Instantané complet de l'état, filtré selon le rôle : le profil Visualisation ne reçoit ni
/// la configuration des drivers, ni les flux, ni les plannings.
fn build_snapshot(state: &AppState, viewer: bool) -> WsServerMessage {
    WsServerMessage::InitialSnapshot {
        tags: state.core.get_all_tags(),
        flows: if viewer {
            vec![]
        } else {
            state.core.get_all_flows()
        },
        alarms: state.core.get_all_alarms(),
        drivers: if viewer {
            vec![]
        } else {
            state.core.get_all_drivers()
        },
        schedules: if viewer {
            vec![]
        } else {
            state.core.get_all_schedules()
        },
        status: state.core.get_system_status(),
    }
}

async fn send_ws(socket: &mut WebSocket, msg: &WsServerMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => socket.send(Message::Text(json)).await.is_ok(),
        Err(_) => true,
    }
}

async fn handle_realtime_socket(
    mut socket: WebSocket,
    state: AppState,
    user: CurrentUser,
    token: String,
) {
    use tokio::sync::broadcast::error::RecvError;

    let viewer = is_viewer(&user);
    let mut cov_rx = state.core.subscribe_cov();
    let mut alarm_rx = state.core.subscribe_alarms();
    let mut drv_rx = state.core.subscribe_drivers();
    let mut trace_rx = state.core.subscribe_flow_traces();
    let mut settings_rx = state.core.subscribe_settings();
    // Vérifie périodiquement que la session est toujours valide (déconnexion, compte désactivé)
    let mut session_check = tokio::time::interval(Duration::from_secs(30));
    session_check.tick().await;
    // Regroupement des COV : dernière valeur par tag, envoyée en un seul message toutes les 100 ms
    let mut pending: HashMap<String, TagState> = HashMap::new();
    let mut flush = tokio::time::interval(Duration::from_millis(100));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    if !send_ws(&mut socket, &build_snapshot(&state, viewer)).await {
        return;
    }

    loop {
        tokio::select! {
            cov_res = cov_rx.recv() => {
                match cov_res {
                    Ok(tag_state) => {
                        pending.insert(tag_state.tag_id.clone(), tag_state);
                    }
                    // Client trop lent : des mises à jour ont été perdues, on resynchronise tout
                    Err(RecvError::Lagged(_)) => {
                        cov_rx = cov_rx.resubscribe();
                        pending.clear();
                        if !send_ws(&mut socket, &build_snapshot(&state, viewer)).await {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            _ = flush.tick(), if !pending.is_empty() => {
                let batch: Vec<TagState> = pending.drain().map(|(_, st)| st).collect();
                if !send_ws(&mut socket, &WsServerMessage::TagUpdateBatch(batch)).await {
                    break;
                }
            }
            alarm_res = alarm_rx.recv() => {
                match alarm_res {
                    Ok(alarms) => {
                        if !send_ws(&mut socket, &WsServerMessage::AlarmUpdate(alarms)).await {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        if !send_ws(&mut socket, &WsServerMessage::AlarmUpdate(state.core.get_all_alarms())).await {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            drv_res = drv_rx.recv() => {
                match drv_res {
                    Ok(drivers) if !viewer => {
                        if !send_ws(&mut socket, &WsServerMessage::DriverListUpdate(drivers)).await {
                            break;
                        }
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
            trace_res = trace_rx.recv() => {
                match trace_res {
                    Ok(trace) if !viewer => {
                        if !send_ws(&mut socket, &WsServerMessage::FlowTrace(trace)).await {
                            break;
                        }
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
            settings_res = settings_rx.recv() => {
                match settings_res {
                    Ok(settings) => {
                        if !send_ws(&mut socket, &WsServerMessage::SettingsUpdate(settings)).await {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
            _ = session_check.tick() => {
                if state.core.resolve_user(Some(&token)).is_none() {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
            client_msg = socket.recv() => {
                match client_msg {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = state.core.shutdown_notify.notified() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refus_hors_simulation_sur_une_ligne_et_adapte_au_protocole() {
        for protocol in [
            ProtocolType::Simulator,
            ProtocolType::OpcUa,
            ProtocolType::BacnetIp,
            ProtocolType::SiemensS7,
        ] {
            let message = simulation_only_message(&protocol);
            assert!(!message.contains('\n'), "{message}");
            assert!(!message.contains("  "), "{message}");
            assert!(message.contains("[simulation] enabled = true"), "{message}");
        }
        let simulator = simulation_only_message(&ProtocolType::Simulator);
        assert!(simulator.contains("simulateur GTB"), "{simulator}");
        assert!(!simulator.contains("driver réel"), "{simulator}");
        assert!(simulation_only_message(&ProtocolType::OpcUa).contains("OpcUa"));
    }
}
