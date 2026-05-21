//! Agent 进程池：按需启动，空闲超时回收。
//!
//! 每个 (conversation_id, agent_id) 独立一个进程，完全隔离。
//! 进程在首次收到 prompt 时启动，空闲 5 分钟后自动回收。
//!
//! 配置选项（模型、模式等）的来源（OptionSource）决定如何应用用户的选择：
//!   - AcpConfig：通过 `session/set_config_option` 推送
//!   - AcpModel ：通过 `session/set_model` 推送（用于从 SessionModelState 合成的模型选择器）
//!   - Local    ：本地 fallback，agent 不感知，仅记录用于 UI 展示

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, Instant, sleep};

use agent_client_protocol::schema::{
    ContentBlock, ContentChunk, InitializeRequest, ModelId, NewSessionRequest,
    NewSessionResponse, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionConfigValueId, SessionModelState,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionModelRequest, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};

use crate::ServerEvent;
use crate::agents::AgentConfig;
use crate::db::Database;

const IDLE_TIMEOUT_SECS: u64 = 300;

// ─── Public API types ─────────────────────────────────────────────────────────

/// Agent → 浏览器：发给前端的命令
pub enum AgentCommand {
    Prompt {
        text: String,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    },
    SetConfigOption {
        config_id: String,
        value: String,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OptionSource {
    /// 通过 ACP `session/set_config_option` 推送
    AcpConfig,
    /// 通过 ACP `session/set_model` 推送（从 SessionModelState 合成的模型选择器）
    AcpModel,
    /// 本地 fallback，agent 不感知
    Local,
}

/// 配置选项的统一状态：选项列表 + 每个选项的来源
#[derive(Default, Clone)]
pub struct OptionsState {
    pub options: Vec<SessionConfigOption>,
    sources: HashMap<String, OptionSource>,
}

impl OptionsState {
    /// 从 NewSessionResponse 构建初始状态。优先级：
    /// 1. agent 显式返回的 config_options
    /// 2. agent 返回的 models 状态（合成 id="model" 的选择器）
    /// 3. agents.json 配置的 fallback
    fn from_new_session(
        resp: &NewSessionResponse,
        fallback: Option<&Vec<SessionConfigOption>>,
    ) -> Self {
        let mut state = Self::default();

        // 1. 显式 config_options
        if let Some(opts) = &resp.config_options {
            for opt in opts {
                state.sources.insert(opt.id.0.to_string(), OptionSource::AcpConfig);
                state.options.push(opt.clone());
            }
        }

        // 2. SessionModelState → 合成 "model" 选择器（如果没有同名 config_option）
        if let Some(model_state) = &resp.models {
            if !state.sources.contains_key("model") {
                let opt = synthesize_model_option(model_state);
                state.sources.insert("model".into(), OptionSource::AcpModel);
                state.options.push(opt);
            }
        }

        // 3. fallback：仅当 agent 既无 config_options 又无 models 时使用
        if state.options.is_empty() {
            if let Some(fb) = fallback {
                for opt in fb {
                    state.sources.insert(opt.id.0.to_string(), OptionSource::Local);
                    state.options.push(opt.clone());
                }
            }
        }

        state
    }

    fn get_source(&self, config_id: &str) -> Option<OptionSource> {
        self.sources.get(config_id).copied()
    }

    /// 把 DB 中保存的用户选择应用到 currentValue
    fn apply_saved_selections(&mut self, saved: &[(String, String)]) {
        for (cfg_id, val) in saved {
            self.set_current_value(cfg_id, val);
        }
    }

    /// 更新某个选项的 currentValue（仅本地状态，不发请求）
    fn set_current_value(&mut self, config_id: &str, new_value: &str) {
        for opt in &mut self.options {
            if opt.id.0.as_ref() == config_id {
                if let SessionConfigKind::Select(select) = &mut opt.kind {
                    select.current_value = SessionConfigValueId::new(new_value);
                }
                break;
            }
        }
    }

    /// 用 agent 返回的最新 config_options 替换状态（保留合成的 model 选择器）
    fn replace_acp_config(&mut self, new_options: Vec<SessionConfigOption>) {
        // 保留合成的 model 选择器（set_config_option 响应不含它）
        let synth_model = if matches!(self.sources.get("model"), Some(OptionSource::AcpModel)) {
            self.options.iter().find(|o| o.id.0.as_ref() == "model").cloned()
        } else {
            None
        };

        self.options.clear();
        self.sources.clear();

        for opt in new_options {
            self.sources.insert(opt.id.0.to_string(), OptionSource::AcpConfig);
            self.options.push(opt);
        }

        if let Some(m) = synth_model {
            if !self.options.iter().any(|o| o.id.0.as_ref() == "model") {
                self.sources.insert("model".into(), OptionSource::AcpModel);
                self.options.push(m);
            }
        }
    }

    /// 当前选项 currentValue
    #[allow(dead_code)]
    fn current_value(&self, config_id: &str) -> Option<String> {
        self.options
            .iter()
            .find(|o| o.id.0.as_ref() == config_id)
            .and_then(|o| match &o.kind {
                SessionConfigKind::Select(s) => Some(s.current_value.0.to_string()),
                _ => None,
            })
    }

    /// 序列化为前端可消费的 JSON（ACP wire 格式：扁平 camelCase）
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.options).unwrap_or(serde_json::Value::Null)
    }
}

// ─── Pool ─────────────────────────────────────────────────────────────────────

struct Entry {
    cmd_tx: mpsc::UnboundedSender<AgentCommand>,
    last_active: Arc<Mutex<Instant>>,
    options_state: Arc<Mutex<OptionsState>>,
}

pub struct AgentPool {
    entries: Mutex<HashMap<String, Entry>>,
    db: Arc<Database>,
}

impl AgentPool {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            db,
        }
    }

    /// 获取池中已有 agent 的 options 状态（如果进程在运行）
    pub async fn get_options_state(
        &self,
        conversation_id: &str,
        agent_id: &str,
    ) -> Option<OptionsState> {
        let key = format!("{conversation_id}:{agent_id}");
        let entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
            return Some(entry.options_state.lock().await.clone());
        }
        None
    }

    /// 发送 set_config_option 到指定的 agent 进程。
    /// 持久化保存用户选择；若 agent 进程在运行则立即推送，否则下次启动时自动应用。
    pub async fn send_set_config_option(
        self: &Arc<Self>,
        conversation_id: &str,
        agent_id: &str,
        config_id: String,
        value: String,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<(), String> {
        // 始终持久化（即便 agent 未启动）
        let _ = self.db.save_agent_config_selection(agent_id, &config_id, &value);

        let key = format!("{conversation_id}:{agent_id}");
        let entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
            *entry.last_active.lock().await = Instant::now();
            let _ = entry
                .cmd_tx
                .send(AgentCommand::SetConfigOption { config_id, value, event_tx });
            Ok(())
        } else {
            Err("智能体进程未启动".into())
        }
    }

    /// 发送 prompt 到指定的 (conversation_id, agent_id) 进程。
    /// 如果进程不存在则启动。事件通过 event_tx 回传。
    pub async fn send_prompt(
        self: &Arc<Self>,
        conversation_id: &str,
        agent_config: &AgentConfig,
        text: String,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<(), String> {
        let key = format!("{}:{}", conversation_id, agent_config.id);
        let mut entries = self.entries.lock().await;

        if let Some(entry) = entries.get(&key) {
            *entry.last_active.lock().await = Instant::now();
            let _ = entry.cmd_tx.send(AgentCommand::Prompt { text, event_tx });
            return Ok(());
        }

        // 创建新 entry
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let last_active = Arc::new(Mutex::new(Instant::now()));
        let options_state = Arc::new(Mutex::new(OptionsState::default()));

        entries.insert(
            key.clone(),
            Entry {
                cmd_tx: cmd_tx.clone(),
                last_active: last_active.clone(),
                options_state: options_state.clone(),
            },
        );

        // 入队第一条 prompt
        let _ = cmd_tx.send(AgentCommand::Prompt { text, event_tx });

        // 启动 agent 任务
        let pool = self.clone();
        let config = agent_config.clone();
        let conv_id = conversation_id.to_string();
        let db = self.db.clone();
        let opts_state_for_loop = options_state.clone();
        tokio::spawn(async move {
            run_agent_loop(&config, &conv_id, db, cmd_rx, opts_state_for_loop).await;
            // 退出后从池中移除
            pool.entries.lock().await.remove(&key);
        });

        // 启动 idle 监控
        let pool2 = self.clone();
        let key2 = format!("{}:{}", conversation_id, agent_config.id);
        let last_active2 = last_active.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(60)).await;
                let elapsed = last_active2.lock().await.elapsed();
                if elapsed >= Duration::from_secs(IDLE_TIMEOUT_SECS) {
                    pool2.entries.lock().await.remove(&key2);
                    break;
                }
            }
        });

        Ok(())
    }
}

// ─── Agent loop ───────────────────────────────────────────────────────────────

async fn run_agent_loop(
    agent_config: &AgentConfig,
    conversation_id: &str,
    db: Arc<Database>,
    mut cmd_rx: mpsc::UnboundedReceiver<AgentCommand>,
    options_state: Arc<Mutex<OptionsState>>,
) {
    // 构建启动命令
    let cmd_str = if agent_config.args.is_empty() {
        format!("\"{}\"", agent_config.command)
    } else {
        format!("\"{}\" {}", agent_config.command, agent_config.args.join(" "))
    };

    let agent = match AcpAgent::from_str(&cmd_str) {
        Ok(a) => a,
        Err(e) => {
            // 把错误发给等待中的第一个命令
            if let Some(cmd) = cmd_rx.recv().await {
                let event_tx = match cmd {
                    AgentCommand::Prompt { event_tx, .. } => event_tx,
                    AgentCommand::SetConfigOption { event_tx, .. } => event_tx,
                };
                let _ = event_tx.send(ServerEvent::Error {
                    message: format!("启动智能体 '{}' 失败: {e}", agent_config.name),
                });
            }
            return;
        }
    };

    let agent_id = agent_config.id.clone();
    let agent_name = agent_config.name.clone();
    let conv_id = conversation_id.to_string();
    let fallback_config = agent_config.fallback_config_options.clone();

    // 当前活跃 prompt 的 event_tx
    let current_event_tx: Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>> =
        Arc::new(Mutex::new(None));
    let current_event_tx_notif = current_event_tx.clone();

    let agent_buffer = Arc::new(tokio::sync::Mutex::new(String::new()));
    let agent_buffer_notif = agent_buffer.clone();
    let agent_id_notif = agent_id.clone();
    let options_state_notif = options_state.clone();

    let _ = agent_client_protocol::Client
        .builder()
        .name("agent-anywhere")
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                let tx_guard = current_event_tx_notif.lock().await;
                if let Some(tx) = tx_guard.as_ref() {
                    forward_notification(
                        notif,
                        tx,
                        &agent_buffer_notif,
                        &agent_id_notif,
                        &options_state_notif,
                    )
                    .await;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let id = request.options.first().map(|o| o.option_id.clone());
                if let Some(id) = id {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    ))
                } else {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |conn: ConnectionTo<Agent>| async move {
            // ── Initialize + NewSession ────────────────────────────────────
            let init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let session = conn
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                ))
                .block_task()
                .await?;

            let agent_label = init
                .agent_info
                .as_ref()
                .map(|i| format!("{} v{}", i.name, i.version))
                .unwrap_or_else(|| agent_name.clone());

            // ── 构建初始 options state ────────────────────────────────────
            let mut state = OptionsState::from_new_session(&session, fallback_config.as_ref());

            // 加载并应用 DB 中保存的用户选择
            let saved = db
                .get_agent_config_selections(&agent_id)
                .unwrap_or_default();
            state.apply_saved_selections(&saved);

            // 把保存的选择推送回 agent（如果与 agent 当前值不一致）
            for (cfg_id, val) in &saved {
                let source = state.get_source(cfg_id);
                match source {
                    Some(OptionSource::AcpConfig) => {
                        let req = SetSessionConfigOptionRequest::new(
                            session.session_id.clone(),
                            SessionConfigId::new(cfg_id.as_str()),
                            SessionConfigValueId::new(val.as_str()),
                        );
                        if let Ok(resp) = conn.send_request(req).block_task().await {
                            state.replace_acp_config(resp.config_options);
                            // 重新应用保存的选择（合成的 model 选择器不在响应里，需要保留）
                            state.apply_saved_selections(&saved);
                        }
                    }
                    Some(OptionSource::AcpModel) => {
                        let req = SetSessionModelRequest::new(
                            session.session_id.clone(),
                            ModelId::new(val.as_str()),
                        );
                        let _ = conn.send_request(req).block_task().await;
                        // model 响应没有数据，只更新本地状态
                        state.set_current_value(cfg_id, val);
                    }
                    Some(OptionSource::Local) | None => {
                        // 本地 fallback：agent 不感知，已应用到 state
                    }
                }
            }

            *options_state.lock().await = state;

            // ── 命令循环 ────────────────────────────────────────────────
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    AgentCommand::Prompt { text, event_tx } => {
                        *current_event_tx.lock().await = Some(event_tx.clone());
                        agent_buffer.lock().await.clear();

                        // 通知前端 ready + 当前 configOptions
                        let _ = event_tx.send(ServerEvent::Ready {
                            agent: agent_label.clone(),
                            session: format!("{:?}", session.session_id),
                            agent_id: agent_id.clone(),
                        });
                        let _ = event_tx.send(ServerEvent::ConfigOptions {
                            options: options_state.lock().await.to_json(),
                        });

                        // 发送 prompt
                        let resp = conn
                            .send_request(PromptRequest::new(
                                session.session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(text))],
                            ))
                            .block_task()
                            .await?;

                        // 保存回复
                        let full_reply = agent_buffer.lock().await.clone();
                        if !full_reply.is_empty() {
                            let _ = db.save_message(
                                &conv_id,
                                "agent",
                                &full_reply,
                                None,
                                Some(&agent_id),
                            );
                        }

                        let _ = event_tx.send(ServerEvent::End {
                            stop_reason: format!("{:?}", resp.stop_reason),
                        });

                        *current_event_tx.lock().await = None;
                    }
                    AgentCommand::SetConfigOption {
                        config_id,
                        value,
                        event_tx,
                    } => {
                        let source = options_state
                            .lock()
                            .await
                            .get_source(&config_id)
                            .unwrap_or(OptionSource::Local);

                        match source {
                            OptionSource::AcpConfig => {
                                let req = SetSessionConfigOptionRequest::new(
                                    session.session_id.clone(),
                                    SessionConfigId::new(config_id.as_str()),
                                    SessionConfigValueId::new(value.as_str()),
                                );
                                match conn.send_request(req).block_task().await {
                                    Ok(resp) => {
                                        let mut s = options_state.lock().await;
                                        s.replace_acp_config(resp.config_options);
                                        // 重新应用其他保存的选择
                                        let saved = db
                                            .get_agent_config_selections(&agent_id)
                                            .unwrap_or_default();
                                        s.apply_saved_selections(&saved);
                                        let _ = event_tx.send(ServerEvent::ConfigOptions {
                                            options: s.to_json(),
                                        });
                                    }
                                    Err(e) => {
                                        let _ = event_tx.send(ServerEvent::Error {
                                            message: format!("设置配置 '{config_id}' 失败: {e}"),
                                        });
                                    }
                                }
                            }
                            OptionSource::AcpModel => {
                                let req = SetSessionModelRequest::new(
                                    session.session_id.clone(),
                                    ModelId::new(value.as_str()),
                                );
                                match conn.send_request(req).block_task().await {
                                    Ok(_) => {
                                        let mut s = options_state.lock().await;
                                        s.set_current_value(&config_id, &value);
                                        let _ = event_tx.send(ServerEvent::ConfigOptions {
                                            options: s.to_json(),
                                        });
                                    }
                                    Err(e) => {
                                        let _ = event_tx.send(ServerEvent::Error {
                                            message: format!("切换模型失败: {e}"),
                                        });
                                    }
                                }
                            }
                            OptionSource::Local => {
                                // 本地 fallback：仅更新缓存（agent 不感知）
                                let mut s = options_state.lock().await;
                                s.set_current_value(&config_id, &value);
                                let _ = event_tx.send(ServerEvent::ConfigOptions {
                                    options: s.to_json(),
                                });
                            }
                        }
                    }
                }
            }

            Ok(())
        })
        .await;
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// 从 SessionModelState 合成一个 id="model" 的选择器
fn synthesize_model_option(state: &SessionModelState) -> SessionConfigOption {
    let opts: Vec<SessionConfigSelectOption> = state
        .available_models
        .iter()
        .map(|m| {
            let mut o = SessionConfigSelectOption::new(m.model_id.0.to_string(), m.name.clone());
            if let Some(d) = &m.description {
                o = o.description(d.clone());
            }
            o
        })
        .collect();

    SessionConfigOption::select(
        SessionConfigId::new("model"),
        "模型",
        SessionConfigValueId::new(state.current_model_id.0.to_string()),
        opts,
    )
    .category(SessionConfigOptionCategory::Model)
}

async fn forward_notification(
    notif: SessionNotification,
    tx: &mpsc::UnboundedSender<ServerEvent>,
    buffer: &tokio::sync::Mutex<String>,
    agent_id: &str,
    options_state: &Mutex<OptionsState>,
) {
    match notif.update {
        SessionUpdate::AgentMessageChunk(ContentChunk { content, .. }) => {
            if let ContentBlock::Text(t) = content {
                buffer.lock().await.push_str(&t.text);
                let _ = tx.send(ServerEvent::Chunk {
                    text: t.text,
                    agent_id: Some(agent_id.to_string()),
                });
            }
        }
        SessionUpdate::AgentThoughtChunk(ContentChunk { content, .. }) => {
            if let ContentBlock::Text(t) = content {
                let _ = tx.send(ServerEvent::Thought { text: t.text });
            }
        }
        SessionUpdate::ToolCall(tc) => {
            let _ = tx.send(ServerEvent::Tool {
                kind: format!("{:?}", tc.kind),
                title: tc.title,
                input: tc.raw_input,
            });
        }
        SessionUpdate::ConfigOptionUpdate(update) => {
            let mut s = options_state.lock().await;
            s.replace_acp_config(update.config_options);
            let _ = tx.send(ServerEvent::ConfigOptions {
                options: s.to_json(),
            });
        }
        _ => {}
    }
}
