// 通过 ACP（Agent Client Protocol）调用本地 kiro-cli 的命令行最简客户端示例。
//
// 工作流程：
//   1. 以子进程方式启动 `kiro-cli acp`，通过 stdin/stdout 进行 JSON-RPC 通信
//   2. 发送 `initialize` 完成协议版本与能力协商
//   3. 发送 `session/new` 创建会话
//   4. 发送 `session/prompt` 提交用户消息
//   5. 收到 `session/notification` 中的 `AgentMessageChunk`，将文本拼接打印到 stdout
//
// 默认问题是 "Hello, who are you?"，可通过命令行参数覆盖：
//   cargo run --bin cli -- "用一句话介绍 ACP 协议"
//
// 默认会调用 PATH 中的 kiro-cli，可用环境变量 KIRO_CLI 指定完整路径，例如：
//   set KIRO_CLI=E:\Software\KiroCLI\kiro-cli.exe

use std::path::PathBuf;
use std::str::FromStr;

use agent_client_protocol::schema::{
    ContentBlock, ContentChunk, InitializeRequest, NewSessionRequest, PromptRequest,
    ProtocolVersion, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let prompt: String = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Hello, who are you? 用中文一句话回答。".to_string());

    let kiro_cmd = std::env::var("KIRO_CLI").unwrap_or_else(|_| "kiro-cli".to_string());
    let command = format!("\"{kiro_cmd}\" acp");

    eprintln!("🚀 启动 Agent: {command}");

    let agent = AcpAgent::from_str(&command)
        .map_err(|e| agent_client_protocol::util::internal_error(format!("解析命令失败: {e}")))?;

    agent_client_protocol::Client
        .builder()
        .name("acp-demo-client")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                handle_session_notification(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            eprintln!("🤝 initialize ...");
            let init = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            eprintln!("✓ Agent: {:?}", init.agent_info);

            eprintln!("📝 session/new ...");
            let new_session = connection
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                ))
                .block_task()
                .await?;
            let session_id = new_session.session_id;
            eprintln!("✓ session_id = {session_id:?}");

            eprintln!("💬 prompt: {prompt}");
            println!("------ Agent 回复 ------");
            let resp = connection
                .send_request(PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(prompt.clone()))],
                ))
                .block_task()
                .await?;
            println!();
            println!("------------------------");
            eprintln!("✅ 完成，stop_reason = {:?}", resp.stop_reason);

            Ok(())
        })
        .await?;

    Ok(())
}

fn handle_session_notification(notification: SessionNotification) {
    match notification.update {
        SessionUpdate::AgentMessageChunk(ContentChunk { content, .. }) => {
            print_content_block(&content);
        }
        SessionUpdate::AgentThoughtChunk(ContentChunk { content, .. }) => {
            if let ContentBlock::Text(t) = content {
                eprint!("[thought] {}", t.text);
            }
        }
        SessionUpdate::ToolCall(tc) => {
            eprintln!("[tool] {:?}: {}", tc.kind, tc.title);
        }
        SessionUpdate::ToolCallUpdate(_) => {}
        other => {
            eprintln!("[update] {other:?}");
        }
    }
}

fn print_content_block(block: &ContentBlock) {
    use std::io::Write;
    if let ContentBlock::Text(t) = block {
        print!("{}", t.text);
        let _ = std::io::stdout().flush();
    }
}
