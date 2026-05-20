// 最简单的 ACP（Agent Client Protocol）Agent 示例
//
// 该 Agent 通过标准输入/输出（stdio）与 ACP 客户端通信：
//   - 收到 `initialize` 请求时，返回支持的协议版本与能力
//   - 收到其他任何请求/通知时，统一返回 "unhandled message" 错误
//
// 运行方式：
//   cargo run
// 然后由 ACP 客户端（例如 Zed）以子进程方式连接此 Agent。
//
// 参考：https://agentclientprotocol.com/libraries/rust
//      https://github.com/agentclientprotocol/rust-sdk

use agent_client_protocol::schema::{AgentCapabilities, InitializeRequest, InitializeResponse};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Result, Stdio};

#[tokio::main]
async fn main() -> Result<()> {
    Agent
        .builder()
        .name("acp-demo-agent") // 仅用于调试日志
        // 处理 initialize 请求：回复协议版本与（空的）能力声明
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _connection| {
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        // 兜底：其他消息一律回复内部错误
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled message"),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        // 通过 stdio 与客户端通信
        .connect_to(Stdio::new())
        .await
}
