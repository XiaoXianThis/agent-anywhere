# Agent Anywhere

多智能体 ACP 网页聊天平台。一个 Rust 后端 + React 前端，通过 [Agent Client Protocol (ACP)](https://github.com/zed-industries/agent-client-protocol) 同时驱动多个本地 AI 编程智能体（Kiro / OpenCode / Pi 等），在浏览器里随时切换。

```
浏览器 ⇄ WebSocket(/ws) ⇄ Rust(Axum) ⇄ 子进程 ACP Agent (stdio JSON-RPC)
```

## 特性

- **多智能体**：通过 `agents.json` 声明任意 ACP 兼容的智能体，可在前端动态增删改
- **会话与智能体解耦**：每个会话独立选择/切换智能体，历史消息保留
- **配置选项透传**：自动从 ACP agent 拉取 `configOptions`（如模型选择），前端展示并下发
- **持久化**：SQLite 存储对话与消息，掉线/刷新自动恢复
- **认证**：环境变量密码 + 签名 Cookie 会话
- **生产单文件部署**：`cargo build --release` 自动打包前端到 `static/dist`，由 Axum 直接托管

## 项目结构

```
.
├── Cargo.toml            # Rust workspace（3 个 binary）
├── build.rs              # release 构建时自动 npm run build 打包前端
├── agents.json           # 智能体声明（命令、参数、配置选项）
├── src/
│   ├── main.rs           # 默认 binary：acp-demo（Web 服务）
│   ├── agents.rs         # 智能体注册表
│   ├── db.rs             # SQLite 持久化
│   ├── pool.rs           # ACP 子进程连接池
│   └── bin/
│       ├── cli.rs        # 命令行版 ACP 客户端（一次性 prompt 演示）
│       └── agent.rs      # 最简 ACP Agent 示例（被其他客户端连接）
├── frontend/             # Vite + React 19 + TypeScript + Tailwind v4
│   ├── src/
│   │   ├── App.tsx
│   │   ├── main.tsx
│   │   ├── index.css
│   │   └── types.ts
│   └── vite.config.ts    # dev 时把 /ws、/api 代理到 :3000
└── static/dist/          # 前端构建产物（release 自动生成）
```

## 快速开始

### 前置条件

- Rust 1.85+（edition 2024）
- Node.js 20+ / npm
- 至少一个 ACP 兼容的 CLI（`kiro-cli`、`opencode`、`pi-acp` 等）在 `PATH` 中

### 配置

```bash
cp .env.example .env
# 编辑 .env 至少设置 ACP_DEMO_PASSWORD
```

环境变量：

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `ACP_DEMO_PASSWORD` | `admin` | 登录密码 |
| `ACP_DEMO_SECRET` | 随机生成 | Cookie 签名密钥（不设置则每次重启需重新登录） |
| `ACP_DEMO_ADDR` | `127.0.0.1:3000` | 监听地址 |
| `KIRO_CLI` | 从 `PATH` 查找 | `kiro-cli` 可执行文件路径 |

### 开发模式

后端和前端分别运行，前端 dev server 把 `/ws`、`/api` 代理到后端：

```bash
# 终端 1：Rust 后端 (127.0.0.1:3000)
cargo run

# 终端 2：Vite 前端 (127.0.0.1:5300)
cd frontend
npm install
npm run dev
```

打开 http://127.0.0.1:5300

### 生产构建

`build.rs` 在 release 构建时自动执行 `npm install` 与 `npm run build`，把前端产物输出到 `static/dist/`，运行时由 Axum 一并托管：

```bash
cargo build --release
./target/release/acp-demo
```

打开 http://127.0.0.1:3000

跳过前端构建：`SKIP_FRONTEND_BUILD=1 cargo build --release`

## 配置智能体

编辑根目录的 `agents.json`：

```json
[
  {
    "id": "kiro",
    "name": "Kiro",
    "command": "kiro-cli",
    "args": ["acp"],
    "description": "Kiro AI 编程助手",
    "color": "#8b5cf6",
    "fallbackConfigOptions": [
      {
        "id": "model",
        "name": "模型",
        "category": "model",
        "type": "select",
        "currentValue": "auto",
        "options": [
          { "value": "auto", "name": "Auto" },
          { "value": "claude-opus-4.7", "name": "Claude Opus 4.7" }
        ]
      }
    ]
  }
]
```

字段说明：

- `command` / `args`：启动 ACP agent 子进程的命令
- `fallbackConfigOptions`：当 agent 自身未提供 `configOptions` 时使用的兜底选项
- 智能体也可在前端 UI 直接增删改，改动写回 `agents.json`

## HTTP API 一览

```
POST   /api/login                              登录
GET    /api/me                                 当前会话信息
POST   /api/logout                             登出

GET    /api/agents                             列出智能体
POST   /api/agents                             新增智能体
PUT    /api/agents/{id}                        更新智能体
DELETE /api/agents/{id}                        删除智能体

GET    /api/conversations                      历史会话列表
POST   /api/conversations                      新建会话
GET    /api/conversations/current              当前会话
POST   /api/conversations/{id}/switch          切换会话
PUT    /api/conversations/{id}/agent           更换会话所用智能体
GET    /api/conversations/{id}/config          会话配置
DELETE /api/conversations/{id}                 删除会话

GET    /ws                                     WebSocket 流式聊天
```

## 附属 Binary

- **`cli`** — 命令行版 ACP 客户端，演示如何 spawn `kiro-cli acp` 并完成一次 prompt
  ```bash
  cargo run --bin cli -- "Hello, who are you?"
  ```
- **`agent`** — 最简 ACP Agent，可被 Zed 等客户端以子进程方式连接，用于学习协议
  ```bash
  cargo run --bin agent
  ```

## License

MIT
