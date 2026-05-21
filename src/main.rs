//! Agent Anywhere — 多智能体 ACP 网页聊天平台
//!
//! 架构：
//!   浏览器 ⇄ WebSocket(/ws) ⇄ Rust 服务 ⇄ 子进程 ACP Agent (stdio JSON-RPC)
//!
//! 功能：
//!   - 多智能体配置（agents.json），可动态增删改
//!   - 对话与智能体解耦，每个会话可独立选择/切换智能体
//!   - SQLite 持久化对话记录
//!   - 环境变量密码认证 + Cookie 会话
//!   - 掉线/重连/刷新自动恢复
//!
//! 环境变量（或 .env 文件）：
//!   ACP_DEMO_PASSWORD   登录密码（默认 admin）
//!   ACP_DEMO_SECRET     Cookie 签名密钥（可选）
//!   ACP_DEMO_ADDR       监听地址（默认 127.0.0.1:3000）
//!   KIRO_CLI            kiro-cli 路径（默认 PATH）

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{FromRef, State, ws::{Message, WebSocket, WebSocketUpgrade}},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, delete, put},
};
use axum_extra::extract::cookie::{Cookie, Key, SignedCookieJar};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::process::{Child, Command};
use tokio::sync::mpsc::unbounded_channel;
use tower_http::services::{ServeDir, ServeFile};

mod agents;
mod db;
mod pool;

use agents::{AgentConfig, AgentRegistry};
use db::Database;
use pool::AgentPool;

// ─── Types ────────────────────────────────────────────────────────────────────

/// 服务器 → 浏览器的事件
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ServerEvent {
    Ready { agent: String, session: String, agent_id: String },
    Chunk { text: String, agent_id: Option<String> },
    Thought { text: String },
    Tool { kind: String, title: String, input: Option<serde_json::Value> },
    End { stop_reason: String },
    Error { message: String },
    History { messages: Vec<db::StoredMessage> },
    /// ACP configOptions 从 agent 获取到的配置选项
    ConfigOptions { options: serde_json::Value },
}

/// 浏览器 → 服务器的消息
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMessage {
    /// 发送消息，附带当前选择的 agent_id
    Prompt {
        text: String,
        agent_id: Option<String>,
    },
    /// 修改 agent 配置选项（ACP session/set_config_option）
    SetConfigOption {
        config_id: String,
        value: String,
    },
}

/// 应用共享状态
#[derive(Clone)]
struct AppState {
    db: Arc<Database>,
    agents: Arc<AgentRegistry>,
    pool: Arc<AgentPool>,
    cookie_key: Key,
    password_hash: String,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.cookie_key.clone()
    }
}

const SESSION_COOKIE: &str = "acp_session";

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let password = std::env::var("ACP_DEMO_PASSWORD").unwrap_or_else(|_| {
        eprintln!("⚠ 未设置 ACP_DEMO_PASSWORD，使用默认密码 'admin'");
        "admin".to_string()
    });
    let password_hash = hash_password(&password);

    let cookie_key = match std::env::var("ACP_DEMO_SECRET") {
        Ok(secret) => {
            let mut key_bytes = [0u8; 64];
            let hash = sha2::Sha512::digest(secret.as_bytes());
            key_bytes.copy_from_slice(&hash);
            Key::from(&key_bytes)
        }
        Err(_) => Key::generate(),
    };

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // 加载智能体配置
    let agents_path = manifest_dir.join("agents.json");
    let agents = Arc::new(AgentRegistry::load(&agents_path));
    println!("📋 已加载 {} 个智能体配置", agents.list().len());

    let db_path = manifest_dir.join("acp_demo.db");
    let db = Database::new(db_path.to_str().unwrap()).expect("无法初始化 SQLite 数据库");
    let db = Arc::new(db);

    let pool = Arc::new(AgentPool::new(db.clone()));

    let state = AppState { db, agents, pool, cookie_key, password_hash };

    let dist_dir = manifest_dir.join("static/dist");
    let index_file = dist_dir.join("index.html");
    let static_service = ServeDir::new(&dist_dir).fallback(ServeFile::new(&index_file));

    let app = Router::new()
        // Auth
        .route("/api/login", post(api_login))
        .route("/api/me", get(api_me))
        .route("/api/logout", post(api_logout))
        // Agents CRUD
        .route("/api/agents", get(api_list_agents))
        .route("/api/agents", post(api_add_agent))
        .route("/api/agents/{id}", put(api_update_agent))
        .route("/api/agents/{id}", delete(api_delete_agent))
        // Conversations
        .route("/api/conversations", get(api_conversations))
        .route("/api/conversations", post(api_new_conversation))
        .route("/api/conversations/current", get(api_current_conversation))
        .route("/api/conversations/{id}/switch", post(api_switch_conversation))
        .route("/api/conversations/{id}/agent", put(api_set_conversation_agent))
        .route("/api/conversations/{id}/config", get(api_conversation_config))
        .route("/api/conversations/{id}", delete(api_delete_conversation))
        // WebSocket
        .route("/ws", get(ws_handler))
        .fallback_service(static_service)
        .with_state(state);

    let addr = std::env::var("ACP_DEMO_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("绑定监听地址失败");
    println!("🌐 Agent Anywhere running: http://{addr}");

    let _vite = if should_spawn_vite() {
        spawn_vite_dev(&manifest_dir.join("frontend"))
    } else {
        if !index_file.exists() {
            println!("⚠ 前端产物未找到: {}", index_file.display());
        }
        None
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("服务器异常退出");
}

// ─── Auth ─────────────────────────────────────────────────────────────────────

fn hash_password(password: &str) -> String {
    hex::encode(sha2::Sha256::digest(password.as_bytes()))
}

fn verify_session(jar: &SignedCookieJar) -> Option<String> {
    jar.get(SESSION_COOKIE).map(|c| c.value().to_string())
}

#[derive(Deserialize)]
struct LoginRequest { password: String }

#[derive(Serialize)]
struct LoginResponse { ok: bool, user_id: Option<String>, message: Option<String> }

async fn api_login(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Json(body): Json<LoginRequest>,
) -> (SignedCookieJar, Json<LoginResponse>) {
    if hash_password(&body.password) != state.password_hash {
        return (jar, Json(LoginResponse { ok: false, user_id: None, message: Some("密码错误".into()) }));
    }
    let user_id = state.db.get_or_create_user().unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let cookie = Cookie::build((SESSION_COOKIE, user_id.clone()))
        .path("/").http_only(true)
        .same_site(axum_extra::extract::cookie::SameSite::Lax)
        .max_age(time::Duration::days(30))
        .build();
    (jar.add(cookie), Json(LoginResponse { ok: true, user_id: Some(user_id), message: None }))
}

async fn api_me(jar: SignedCookieJar) -> Json<serde_json::Value> {
    match verify_session(&jar) {
        Some(user_id) => Json(serde_json::json!({ "authenticated": true, "user_id": user_id })),
        None => Json(serde_json::json!({ "authenticated": false })),
    }
}

async fn api_logout(jar: SignedCookieJar) -> (SignedCookieJar, Json<serde_json::Value>) {
    let cookie = Cookie::build((SESSION_COOKIE, "")).path("/").max_age(time::Duration::ZERO).build();
    (jar.remove(cookie), Json(serde_json::json!({ "ok": true })))
}

// ─── Agents API ───────────────────────────────────────────────────────────────

async fn api_list_agents(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "agents": state.agents.list() }))
}

async fn api_add_agent(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Json(agent): Json<AgentConfig>,
) -> (StatusCode, Json<serde_json::Value>) {
    if verify_session(&jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" })));
    }
    match state.agents.add(agent) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))),
    }
}

async fn api_update_agent(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut agent): Json<AgentConfig>,
) -> (StatusCode, Json<serde_json::Value>) {
    if verify_session(&jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" })));
    }
    agent.id = id; // 确保 URL 中的 id 优先
    match state.agents.update(agent) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))),
    }
}

async fn api_delete_agent(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if verify_session(&jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" })));
    }
    match state.agents.remove(&id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))),
    }
}

// ─── Conversations API ────────────────────────────────────────────────────────

async fn api_conversations(
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> (StatusCode, Json<serde_json::Value>) {
    let user_id = match verify_session(&jar) {
        Some(id) => id,
        None => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" }))),
    };
    match state.db.list_conversations(&user_id) {
        Ok(convs) => (StatusCode::OK, Json(serde_json::json!({ "conversations": convs }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

#[derive(Deserialize)]
struct NewConversationRequest {
    agent_id: Option<String>,
}

async fn api_new_conversation(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Json(body): Json<NewConversationRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let user_id = match verify_session(&jar) {
        Some(id) => id,
        None => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" }))),
    };
    match state.db.new_conversation(&user_id, body.agent_id.as_deref()) {
        Ok(id) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "conversation_id": id }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

async fn api_current_conversation(
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> (StatusCode, Json<serde_json::Value>) {
    let user_id = match verify_session(&jar) {
        Some(id) => id,
        None => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" }))),
    };
    match state.db.get_active_conversation(&user_id) {
        Ok(Some(conv)) => {
            let messages = state.db.get_messages(&conv.id).unwrap_or_default();
            (StatusCode::OK, Json(serde_json::json!({ "conversation": conv, "messages": messages })))
        }
        Ok(None) => (StatusCode::OK, Json(serde_json::json!({ "conversation": null, "messages": [] }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

async fn api_switch_conversation(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let user_id = match verify_session(&jar) {
        Some(id) => id,
        None => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" }))),
    };
    match state.db.switch_conversation(&user_id, &id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

#[derive(Deserialize)]
struct SetAgentRequest { agent_id: String }

async fn api_set_conversation_agent(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<SetAgentRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if verify_session(&jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" })));
    }
    match state.db.set_conversation_agent(&id, &body.agent_id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

/// 获取对话当前 agent 的 configOptions。
/// 来源优先级：进程池缓存（agent 已在运行）→ fallback（应用 DB 中保存的选择）。
async fn api_conversation_config(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if verify_session(&jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" })));
    }
    let conv = match state.db.get_conversation(&id) {
        Ok(Some(c)) => c,
        _ => return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "对话不存在" }))),
    };
    let agent_id = conv.agent_id.unwrap_or_default();
    if agent_id.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!({ "options": null })));
    }

    // 1) 进程池缓存（agent 已启动，包含了 agent 返回的真实 configOptions / models）
    if let Some(state_snapshot) = state.pool.get_options_state(&id, &agent_id).await {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "options": state_snapshot.to_json() })),
        );
    }

    // 2) Fallback：从 agents.json 读取，应用 DB 中保存的用户选择
    let agent = match state.agents.get(&agent_id) {
        Some(a) => a,
        None => return (StatusCode::OK, Json(serde_json::json!({ "options": null }))),
    };
    let Some(mut fb) = agent.fallback_config_options.clone() else {
        return (StatusCode::OK, Json(serde_json::json!({ "options": null })));
    };

    if let Ok(saved) = state.db.get_agent_config_selections(&agent_id) {
        for (cfg_id, val) in &saved {
            for opt in fb.iter_mut() {
                if opt.id.0.as_ref() == cfg_id {
                    if let agent_client_protocol::schema::SessionConfigKind::Select(sel) =
                        &mut opt.kind
                    {
                        sel.current_value =
                            agent_client_protocol::schema::SessionConfigValueId::new(val.as_str());
                    }
                    break;
                }
            }
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({ "options": serde_json::to_value(&fb).unwrap_or(serde_json::Value::Null) })),
    )
}

async fn api_delete_conversation(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if verify_session(&jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "未登录" })));
    }
    match state.db.delete_conversation(&id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

// ─── WebSocket ────────────────────────────────────────────────────────────────

async fn ws_handler(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    let user_id = match verify_session(&jar) {
        Some(id) => id,
        None => return (StatusCode::UNAUTHORIZED, "未登录").into_response(),
    };
    ws.on_upgrade(move |socket| handle_ws(socket, state, user_id))
}

/// WebSocket 处理：纯转发层，不主动启动 agent。
/// Agent 仅在收到第一条 prompt 时按需启动。
async fn handle_ws(socket: WebSocket, state: AppState, user_id: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (event_tx, mut event_rx) = unbounded_channel::<ServerEvent>();

    // 获取或创建活跃对话
    let default_agent_id = state.agents.default_agent().map(|a| a.id.clone());
    let conversation_id = state.db
        .get_or_create_active_conversation(&user_id, default_agent_id.as_deref())
        .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

    // 发送历史消息
    if let Ok(messages) = state.db.get_messages(&conversation_id) {
        if !messages.is_empty() {
            let _ = event_tx.send(ServerEvent::History { messages });
        }
    }

    // 通知前端已就绪（不启动 agent，只是表示 WebSocket 通道可用）
    let conv = state.db.get_conversation(&conversation_id).ok().flatten();
    let conv_agent_id = conv.and_then(|c| c.agent_id)
        .or(default_agent_id);
    let _ = event_tx.send(ServerEvent::Ready {
        agent: "idle".into(),
        session: conversation_id.clone(),
        agent_id: conv_agent_id.unwrap_or_default(),
    });

    // 任务 A: 事件 → WebSocket
    let forwarder = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let json = match serde_json::to_string(&event) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // 任务 B: WebSocket → 按需路由到 agent pool
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<ClientMessage>(text.as_str()) {
                    Ok(ClientMessage::Prompt { text, agent_id }) => {
                        // 确定使用哪个 agent
                        let resolved_agent_id = agent_id
                            .or_else(|| {
                                state.db.get_conversation(&conversation_id).ok()
                                    .flatten().and_then(|c| c.agent_id)
                            })
                            .or_else(|| state.agents.default_agent().map(|a| a.id));

                        let agent_config = resolved_agent_id.as_deref()
                            .and_then(|id| state.agents.get(id))
                            .or_else(|| state.agents.default_agent());

                        let Some(config) = agent_config else {
                            let _ = event_tx.send(ServerEvent::Error {
                                message: "没有可用的智能体".into(),
                            });
                            continue;
                        };

                        // 更新对话的 agent_id
                        let _ = state.db.set_conversation_agent(&conversation_id, &config.id);

                        // 保存用户消息
                        let _ = state.db.save_message(&conversation_id, "user", &text, None, None);

                        // 发送到 agent pool（按需启动或复用）
                        if let Err(e) = state.pool.send_prompt(
                            &conversation_id,
                            &config,
                            text,
                            event_tx.clone(),
                        ).await {
                            let _ = event_tx.send(ServerEvent::Error { message: e });
                        }
                    }
                    Ok(ClientMessage::SetConfigOption { config_id, value }) => {
                        let resolved_agent_id = state.db.get_conversation(&conversation_id).ok()
                            .flatten().and_then(|c| c.agent_id)
                            .or_else(|| state.agents.default_agent().map(|a| a.id));

                        let Some(aid) = resolved_agent_id else {
                            let _ = event_tx.send(ServerEvent::Error {
                                message: "没有活跃的智能体".into(),
                            });
                            continue;
                        };

                        // pool 内部会持久化到 DB；若 agent 未启动，下次启动时会自动应用
                        match state.pool.send_set_config_option(
                            &conversation_id,
                            &aid,
                            config_id.clone(),
                            value.clone(),
                            event_tx.clone(),
                        ).await {
                            Ok(()) => {}
                            Err(_) => {
                                // Agent 未启动 → 立即用 fallback 计算并通知前端，让 UI 反映用户选择
                                if let Some(agent_cfg) = state.agents.get(&aid) {
                                    let opts = compute_fallback_options(
                                        &agent_cfg,
                                        &state.db,
                                        &aid,
                                    );
                                    let _ = event_tx.send(ServerEvent::ConfigOptions { options: opts });
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    });

    // 等待任一任务结束
    tokio::select! {
        _ = forwarder => {}
        _ = reader => {}
    }
}

/// 计算 fallback configOptions（应用 DB 中保存的用户选择），用于 agent 未启动时
fn compute_fallback_options(
    agent: &AgentConfig,
    db: &Database,
    agent_id: &str,
) -> serde_json::Value {
    let Some(mut fb) = agent.fallback_config_options.clone() else {
        return serde_json::Value::Null;
    };
    if let Ok(saved) = db.get_agent_config_selections(agent_id) {
        for (cfg_id, val) in &saved {
            for opt in fb.iter_mut() {
                if opt.id.0.as_ref() == cfg_id {
                    if let agent_client_protocol::schema::SessionConfigKind::Select(sel) =
                        &mut opt.kind
                    {
                        sel.current_value =
                            agent_client_protocol::schema::SessionConfigValueId::new(val.as_str());
                    }
                    break;
                }
            }
        }
    }
    serde_json::to_value(&fb).unwrap_or(serde_json::Value::Null)
}

// ─── Vite Dev Server ──────────────────────────────────────────────────────────

fn should_spawn_vite() -> bool {
    if std::env::var_os("SKIP_FRONTEND_DEV").is_some() { return false; }
    if std::env::var_os("FRONTEND_DEV").is_some() { return true; }
    cfg!(debug_assertions)
}

fn spawn_vite_dev(frontend_dir: &std::path::Path) -> Option<ChildGuard> {
    if !frontend_dir.exists() { return None; }
    if !frontend_dir.join("node_modules").exists() {
        println!("📦 npm install ...");
        let status = std::process::Command::new(npm_cmd())
            .arg("install").current_dir(frontend_dir).status();
        match status {
            Ok(s) if s.success() => {}
            _ => return None,
        }
    }
    let mut cmd = Command::new(npm_cmd());
    cmd.arg("run").arg("dev").current_dir(frontend_dir)
        .stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    match cmd.spawn() {
        Ok(child) => {
            println!("🚀 vite dev: http://127.0.0.1:5300");
            Some(ChildGuard(Some(child)))
        }
        Err(_) => None,
    }
}

struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() { let _ = c.start_kill(); }
    }
}

#[cfg(windows)] fn npm_cmd() -> &'static str { "npm.cmd" }
#[cfg(not(windows))] fn npm_cmd() -> &'static str { "npm" }

async fn shutdown_signal() {
    let ctrl_c = async { let _ = tokio::signal::ctrl_c().await; };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sig) = signal(SignalKind::terminate()) { sig.recv().await; }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {} _ = terminate => {} }
    println!("\n🛑 正在退出...");
}
