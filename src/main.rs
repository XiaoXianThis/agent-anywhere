//! 通过 ACP 调用本地 kiro-cli 的网页聊天 Demo。
//!
//! 架构：
//!   浏览器 ⇄ WebSocket(/ws) ⇄ Rust 服务 ⇄ 子进程 `kiro-cli acp` (stdio JSON-RPC)
//!
//! 每个浏览器 WebSocket 连接独占一个 kiro-cli 子进程与一个 ACP 会话。
//!
//! 启动：
//!   set KIRO_CLI=E:\Software\KiroCLI\kiro-cli.exe   (可选；PATH 中有 kiro-cli 时不需要)
//!   set ACP_DEMO_ADDR=127.0.0.1:3000                (可选；默认 127.0.0.1:3000)
//!   cargo run
//!
//! 然后浏览器打开 http://127.0.0.1:3000

use std::path::PathBuf;
use std::str::FromStr;

use agent_client_protocol::schema::{
    ContentBlock, ContentChunk, InitializeRequest, NewSessionRequest, PromptRequest,
    ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};
use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::{Html, IntoResponse},
    routing::get,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const INDEX_HTML: &str = include_str!("../static/index.html");

/// 服务器 → 浏览器的事件
#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ServerEvent {
    Ready { agent: String, session: String },
    Chunk { text: String },
    Thought { text: String },
    Tool { kind: String, title: String },
    End { stop_reason: String },
    Error { message: String },
}

/// 浏览器 → 服务器的消息
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMessage {
    Prompt { text: String },
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/ws", get(ws_handler));

    let addr = std::env::var("ACP_DEMO_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("绑定监听地址失败");
    println!("🌐 ACP Demo Web app running: http://{addr}");

    axum::serve(listener, app).await.expect("服务器异常退出");
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_ws)
}

/// 处理一条 WebSocket 连接的整个生命周期。
async fn handle_ws(socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (event_tx, mut event_rx) = unbounded_channel::<ServerEvent>();
    let (prompt_tx, prompt_rx) = unbounded_channel::<String>();

    // 任务 A: 服务端事件 → WebSocket
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

    // 任务 B: WebSocket → 用户提示词
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Text(text) = msg {
                if let Ok(ClientMessage::Prompt { text }) =
                    serde_json::from_str::<ClientMessage>(text.as_str())
                {
                    if prompt_tx.send(text).is_err() {
                        break;
                    }
                }
            }
        }
        // 结束时 prompt_tx 自动 drop，使 prompt_rx.recv() 返回 None
    });

    // 任务 C (当前栈): 启动 ACP Agent，跑会话循环
    let event_tx_for_err = event_tx.clone();
    if let Err(e) = run_agent_session(event_tx, prompt_rx).await {
        let _ = event_tx_for_err.send(ServerEvent::Error {
            message: format!("ACP 错误: {e}"),
        });
    }

    forwarder.abort();
    reader.abort();
}

/// 启动 kiro-cli 子进程，建立 ACP 连接，循环接收浏览器的 prompt 并把回复事件写回。
async fn run_agent_session(
    event_tx: UnboundedSender<ServerEvent>,
    mut prompt_rx: UnboundedReceiver<String>,
) -> agent_client_protocol::Result<()> {
    // 子进程命令：优先取环境变量 KIRO_CLI，否则依赖 PATH。
    let kiro = std::env::var("KIRO_CLI").unwrap_or_else(|_| "kiro-cli".into());
    let cmd = format!("\"{kiro}\" acp");
    let agent = AcpAgent::from_str(&cmd).map_err(|e| {
        agent_client_protocol::util::internal_error(format!("启动 kiro-cli 失败: {e}"))
    })?;

    // 在闭包里要发送事件，需要分别 clone 出来给 notification handler 和会话循环用。
    let event_tx_notif = event_tx.clone();
    let event_tx_loop = event_tx.clone();

    agent_client_protocol::Client
        .builder()
        .name("acp-web-demo")
        // Agent 推送的 session/notification（流式输出落在这里）
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                forward_notification(notif, &event_tx_notif);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // 工具/文件等权限请求：YOLO 模式自动批准第一个选项；演示用，请按需调整。
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
        // 连接建立后的会话循环
        .connect_with(agent, |conn: ConnectionTo<Agent>| async move {
            // 1) initialize
            let init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            // 2) session/new
            let session = conn
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                ))
                .block_task()
                .await?;

            // 通知前端可用了
            let agent_label = init
                .agent_info
                .as_ref()
                .map(|i| format!("{} v{}", i.name, i.version))
                .unwrap_or_else(|| "Unknown agent".into());
            let _ = event_tx_loop.send(ServerEvent::Ready {
                agent: agent_label,
                session: format!("{:?}", session.session_id),
            });

            // 3) 循环：每收到一条 prompt 就发一次 session/prompt
            while let Some(text) = prompt_rx.recv().await {
                let resp = conn
                    .send_request(PromptRequest::new(
                        session.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(text))],
                    ))
                    .block_task()
                    .await?;
                let _ = event_tx_loop.send(ServerEvent::End {
                    stop_reason: format!("{:?}", resp.stop_reason),
                });
            }
            Ok(())
        })
        .await
}

/// 将 ACP 的 SessionUpdate 转换为前端事件。
fn forward_notification(notif: SessionNotification, tx: &UnboundedSender<ServerEvent>) {
    match notif.update {
        SessionUpdate::AgentMessageChunk(ContentChunk { content, .. }) => {
            if let ContentBlock::Text(t) = content {
                let _ = tx.send(ServerEvent::Chunk { text: t.text });
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
            });
        }
        // 其余 SessionUpdate 变体暂不渲染
        _ => {}
    }
}
