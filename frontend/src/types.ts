// 服务器 → 浏览器事件
export type ServerEvent =
  | { type: "ready"; agent: string; session: string; agentId: string }
  | { type: "chunk"; text: string; agentId?: string }
  | { type: "thought"; text: string }
  | { type: "tool"; kind: string; title: string; input?: unknown }
  | { type: "end"; stopReason: string }
  | { type: "error"; message: string }
  | { type: "history"; messages: StoredMessage[] }
  | { type: "configOptions"; options: AcpConfigOption[] | null };

export type Role = "user" | "agent" | "tool" | "error" | "thought";

export interface ChatMessage {
  id: string;
  role: Role;
  text: string;
  streaming?: boolean;
  meta?: { kind?: string; input?: unknown };
  agentId?: string;
}

export interface StoredMessage {
  id: string;
  conversation_id: string;
  role: string;
  text: string;
  meta?: string;
  agent_id?: string;
  created_at: string;
}

export interface Conversation {
  id: string;
  user_id: string;
  title: string;
  active: boolean;
  agent_id?: string;
  created_at: string;
  updated_at: string;
}

export interface AgentConfig {
  id: string;
  name: string;
  command: string;
  args: string[];
  description: string;
  color: string;
}

// ─── ACP Session Config Option (ACP wire format, camelCase) ──────────────────
//
// 与后端使用的 agent-client-protocol-schema crate 中的 `SessionConfigOption`
// 结构完全一致。后端始终以 ACP 标准 camelCase JSON 序列化。

export interface AcpConfigOption {
  id: string;
  name: string;
  description?: string;
  /** 语义类别：mode | model | thought_level | 其它字符串 */
  category?: string;
  /** 选项类型，目前仅 "select" 是稳定的 */
  type: "select" | string;
  /** 仅 type === "select" 时存在 */
  currentValue?: string;
  /** 仅 type === "select" 时存在 */
  options?: AcpConfigSelectOption[];
}

export interface AcpConfigSelectOption {
  value: string;
  name: string;
  description?: string;
}
