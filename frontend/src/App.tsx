import { Component, useCallback, useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";
import type { AgentConfig, ChatMessage, Conversation, ServerEvent, StoredMessage } from "./types";

// ─── Error Boundary ───────────────────────────────────────────────────────────

class MarkdownErrorBoundary extends Component<
  { children: React.ReactNode; fallback: string },
  { hasError: boolean }
> {
  state = { hasError: false };
  static getDerivedStateFromError() { return { hasError: true }; }
  render() {
    if (this.state.hasError) return <span className="whitespace-pre-wrap">{this.props.fallback}</span>;
    return this.props.children;
  }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

type Status =
  | { kind: "connecting" }
  | { kind: "ready"; agent: string; session: string; agentId: string }
  | { kind: "closed" }
  | { kind: "error"; message: string };

type AuthState =
  | { kind: "checking" }
  | { kind: "unauthenticated" }
  | { kind: "authenticated"; userId: string };

const newId = () =>
  globalThis.crypto?.randomUUID?.() ??
  Math.random().toString(36).slice(2) + Date.now().toString(36);

// ─── App Root ─────────────────────────────────────────────────────────────────

export default function App() {
  const [auth, setAuth] = useState<AuthState>({ kind: "checking" });

  useEffect(() => {
    fetch("/api/me").then(r => r.json()).then(data => {
      setAuth(data.authenticated
        ? { kind: "authenticated", userId: data.user_id }
        : { kind: "unauthenticated" });
    }).catch(() => setAuth({ kind: "unauthenticated" }));
  }, []);

  if (auth.kind === "checking")
    return <div className="flex h-full items-center justify-center bg-[#0a0a0a]"><div className="text-white/40 text-sm">加载中...</div></div>;
  if (auth.kind === "unauthenticated")
    return <LoginScreen onLogin={(userId) => setAuth({ kind: "authenticated", userId })} />;
  return <ChatApp userId={auth.userId} onLogout={() => setAuth({ kind: "unauthenticated" })} />;
}

// ─── Login Screen ─────────────────────────────────────────────────────────────

function LoginScreen({ onLogin }: { onLogin: (userId: string) => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  useEffect(() => { inputRef.current?.focus(); }, []);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!password.trim()) return;
    setLoading(true); setError("");
    try {
      const res = await fetch("/api/login", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ password }),
      });
      const data = await res.json();
      if (data.ok) onLogin(data.user_id);
      else setError(data.message || "登录失败");
    } catch { setError("网络错误"); }
    finally { setLoading(false); }
  }

  return (
    <div className="flex h-full items-center justify-center bg-[#0a0a0a]">
      <form onSubmit={handleSubmit} className="w-full max-w-sm mx-4 p-6 rounded-2xl bg-[#111111] border border-white/[0.08]">
        <div className="flex items-center gap-2.5 mb-6">
          <div className="w-9 h-9 rounded-lg bg-gradient-to-br from-blue-500 to-violet-600 flex items-center justify-center">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="white" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <path d="M12 2L2 7l10 5 10-5-10-5z"/><path d="M2 17l10 5 10-5"/><path d="M2 12l10 5 10-5"/>
            </svg>
          </div>
          <h1 className="text-lg font-semibold text-white/90">Agent Anywhere</h1>
        </div>
        <label className="block text-sm text-white/50 mb-2">输入密码以继续</label>
        <input ref={inputRef} type="password" value={password} onChange={e => setPassword(e.target.value)}
          placeholder="密码" disabled={loading}
          className="w-full px-4 py-2.5 rounded-lg bg-[#1a1a1a] border border-white/[0.12] text-sm text-white/90 placeholder:text-white/25 outline-none focus:border-white/25 transition-colors disabled:opacity-50" />
        {error && <p className="mt-2 text-sm text-red-400">{error}</p>}
        <button type="submit" disabled={loading || !password.trim()}
          className="w-full mt-4 px-4 py-2.5 rounded-lg bg-white text-black text-sm font-medium hover:bg-white/90 transition-colors disabled:opacity-40 disabled:cursor-not-allowed">
          {loading ? "登录中..." : "登录"}
        </button>
      </form>
    </div>
  );
}

// ─── Chat App ─────────────────────────────────────────────────────────────────

function ChatApp({ userId, onLogout }: { userId: string; onLogout: () => void }) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [status, setStatus] = useState<Status>({ kind: "connecting" });
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(true);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [agents, setAgents] = useState<AgentConfig[]>([]);
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [currentAgentId, setCurrentAgentId] = useState<string | null>(null);
  const [configOptions, setConfigOptions] = useState<import("./types").AcpConfigOption[]>([]);

  const wsRef = useRef<WebSocket | null>(null);
  const streamingIdRef = useRef<string | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const currentAgentIdRef = useRef<string | null>(currentAgentId);
  useEffect(() => { currentAgentIdRef.current = currentAgentId; }, [currentAgentId]);
  const [queue, setQueue] = useState<string[]>([]);
  const reconnectRef = useRef<number>(0);
  const [waitingReply, setWaitingReply] = useState(false);

  // 初始化：加载 agents、当前对话、消息、configOptions
  useEffect(() => {
    (async () => {
      const agentsData = await fetch("/api/agents").then(r => r.json()).catch(() => ({ agents: [] }));
      const list: AgentConfig[] = agentsData.agents || [];
      setAgents(list);

      const convData = await fetch("/api/conversations/current").then(r => r.json()).catch(() => null);
      const convAgentId = convData?.conversation?.agent_id || (list[0]?.id ?? null);
      setCurrentAgentId(convAgentId);

      if (Array.isArray(convData?.messages) && convData.messages.length > 0) {
        setMessages(convData.messages.map((m: any) => ({
          id: m.id,
          role: m.role as ChatMessage["role"],
          text: m.text,
          meta: m.meta ? JSON.parse(m.meta) : undefined,
          agentId: m.agent_id || undefined,
        })));
      }

      if (convData?.conversation?.id) {
        const cfg = await fetch(`/api/conversations/${convData.conversation.id}/config`)
          .then(r => r.json()).catch(() => null);
        setConfigOptions(Array.isArray(cfg?.options) ? cfg.options : []);
      }
    })();
    loadConversations();
  }, []);

  function loadConversations() {
    fetch("/api/conversations").then(r => r.json()).then(d => setConversations(d.conversations || []));
  }

  // WebSocket
  const connectWs = useCallback(() => {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/ws`);
    wsRef.current = ws;
    ws.onopen = () => { setStatus({ kind: "connecting" }); reconnectRef.current = 0; };
    ws.onclose = () => {
      setStatus({ kind: "closed" }); setBusy(true);
      const delay = Math.min(1000 * 2 ** reconnectRef.current, 30000);
      reconnectRef.current++;
      setTimeout(connectWs, delay);
    };
    ws.onerror = () => setStatus({ kind: "error", message: "WebSocket 连接失败" });
    ws.onmessage = (ev) => {
      try { handleEvent(JSON.parse(ev.data)); } catch {}
    };
  }, []);

  useEffect(() => {
    connectWs();
    return () => { reconnectRef.current = 999; wsRef.current?.close(); };
  }, [connectWs]);

  useEffect(() => { bottomRef.current?.scrollIntoView({ behavior: "smooth" }); }, [messages]);
  useEffect(() => {
    if (!busy && status.kind === "ready") {
      textareaRef.current?.focus();
      if (queue.length > 0) { const [next, ...rest] = queue; setQueue(rest); sendText(next); }
    }
  }, [busy, status.kind]);

  function handleEvent(event: ServerEvent) {
    switch (event.type) {
      case "ready":
        setStatus({ kind: "ready", agent: event.agent, session: event.session, agentId: event.agentId });
        // 仅在我们还没决定 agent 时（首次加载/重连），采用后端报告的 agent
        // 用户主动切换后，不被 ready 事件覆盖
        if (event.agentId && currentAgentIdRef.current == null) setCurrentAgentId(event.agentId);
        setBusy(false);
        break;
      case "history":
        if (event.messages?.length > 0) {
          setMessages(event.messages.map(m => ({
            id: m.id, role: m.role as ChatMessage["role"], text: m.text,
            meta: m.meta ? JSON.parse(m.meta) : undefined,
            agentId: m.agent_id || undefined,
          })));
        }
        break;
      case "chunk":
        setWaitingReply(false);
        setMessages(prev => {
          const id = streamingIdRef.current;
          if (id) return prev.map(m => m.id === id ? { ...m, text: m.text + event.text } : m);
          const msg: ChatMessage = { id: newId(), role: "agent", text: event.text, streaming: true, agentId: event.agentId || currentAgentIdRef.current || undefined };
          streamingIdRef.current = msg.id;
          return [...prev, msg];
        });
        break;
      case "tool":
        setMessages(prev => [...prev, { id: newId(), role: "tool", text: event.title, meta: { kind: event.kind, input: event.input } }]);
        break;
      case "end":
        setMessages(prev => prev.map(m => m.streaming ? { ...m, streaming: false } : m));
        streamingIdRef.current = null;
        setWaitingReply(false);
        setBusy(false);
        loadConversations();
        break;
      case "error":
        setMessages(prev => [...prev, { id: newId(), role: "error", text: event.message }]);
        setWaitingReply(false);
        setBusy(false);
        break;
      case "configOptions":
        setConfigOptions(Array.isArray(event.options) ? event.options : []);
        break;
    }
  }

  function sendText(text: string) {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    setMessages(prev => [...prev, { id: newId(), role: "user", text }]);
    ws.send(JSON.stringify({ type: "prompt", text, agent_id: currentAgentId }));
    setBusy(true);
    setWaitingReply(true);
    streamingIdRef.current = null;
  }

  const send = useCallback(() => {
    const text = input.trim();
    if (!text) return;
    setInput("");
    if (status.kind !== "ready" || busy) setQueue(prev => [...prev, text]);
    else sendText(text);
    setTimeout(() => textareaRef.current?.focus(), 0);
  }, [input, busy, status.kind]);

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); }
  }

  async function switchAgent(agentId: string) {
    if (currentAgentId === agentId) return;
    setCurrentAgentId(agentId);
    setConfigOptions([]); // 切换瞬间清空，等待新 agent 的 options

    const activeConv = conversations.find(c => c.active);
    if (!activeConv) {
      // 还没有活跃对话：直接拉对应 agent 的 fallback
      // 这种情况下后端会在收到第一条 prompt 时把 agent_id 落库
      try {
        const res = await fetch("/api/conversations/current");
        const data = await res.json();
        if (data.conversation?.id) {
          // 由于此时对话仍可能没绑定 agentId，直接写入再读取
          await fetch(`/api/conversations/${data.conversation.id}/agent`, {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ agent_id: agentId }),
          });
          const cfg = await fetch(`/api/conversations/${data.conversation.id}/config`).then(r => r.json());
          setConfigOptions(Array.isArray(cfg?.options) ? cfg.options : []);
        }
      } catch {}
      return;
    }

    await fetch(`/api/conversations/${activeConv.id}/agent`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ agent_id: agentId }),
    });
    const cfg = await fetch(`/api/conversations/${activeConv.id}/config`).then(r => r.json()).catch(() => null);
    setConfigOptions(Array.isArray(cfg?.options) ? cfg.options : []);
    loadConversations();
  }

  async function newConversation(agentId?: string) {
    const aid = agentId || currentAgentId;
    await fetch("/api/conversations", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ agent_id: aid }),
    });
    setMessages([]); // 新对话，清空消息
    // 立即拉新对话的 configOptions（fallback 已应用 DB 中保存的选择）
    const res = await fetch("/api/conversations/current").then(r => r.json()).catch(() => null);
    if (res?.conversation?.id) {
      const cfg = await fetch(`/api/conversations/${res.conversation.id}/config`)
        .then(r => r.json()).catch(() => null);
      setConfigOptions(Array.isArray(cfg?.options) ? cfg.options : []);
    }
    loadConversations();
    reconnectRef.current = 0;
    wsRef.current?.close();
  }

  async function switchConversation(convId: string) {
    await fetch(`/api/conversations/${convId}/switch`, { method: "POST" });
    // 立即加载对话记录，不等 WebSocket 重连
    const res = await fetch("/api/conversations/current");
    const data = await res.json();
    if (data.messages?.length > 0) {
      setMessages(data.messages.map((m: any) => ({
        id: m.id, role: m.role as ChatMessage["role"], text: m.text,
        meta: m.meta ? JSON.parse(m.meta) : undefined,
        agentId: m.agent_id || undefined,
      })));
    } else {
      setMessages([]);
    }
    if (data.conversation?.agent_id) {
      setCurrentAgentId(data.conversation.agent_id);
    }
    // 加载该对话的 configOptions（已绑定的 agent 的 fallback / pool 缓存）
    if (data.conversation?.id) {
      const cfg = await fetch(`/api/conversations/${data.conversation.id}/config`)
        .then(r => r.json()).catch(() => null);
      setConfigOptions(Array.isArray(cfg?.options) ? cfg.options : []);
    } else {
      setConfigOptions([]);
    }
    loadConversations();
    // 后台重连 WebSocket
    reconnectRef.current = 0;
    wsRef.current?.close();
  }

  async function handleLogout() {
    await fetch("/api/logout", { method: "POST" });
    reconnectRef.current = 999;
    wsRef.current?.close();
    onLogout();
  }

  async function deleteConversation(convId: string) {
    await fetch(`/api/conversations/${convId}`, { method: "DELETE" });
    loadConversations();
    // 如果删除的是当前活跃对话，清空消息
    const conv = conversations.find(c => c.id === convId);
    if (conv?.active) {
      setMessages([]);
      reconnectRef.current = 0;
      wsRef.current?.close();
    }
  }

  const ready = status.kind === "ready";
  const currentAgent = agents.find(a => a.id === currentAgentId);

  return (
    <div className="flex h-full bg-[#0a0a0a] text-white overflow-hidden">
      {sidebarOpen && <div className="sidebar-overlay lg:hidden" onClick={() => setSidebarOpen(false)} />}

      {/* ── Sidebar ── */}
      <aside className={[
        "fixed lg:relative z-50 lg:z-auto flex flex-col w-64 h-full",
        "bg-[#111111] border-r border-white/[0.06] transition-transform duration-200 ease-out",
        sidebarOpen ? "translate-x-0" : "-translate-x-full lg:translate-x-0",
      ].join(" ")}>
        {/* Logo */}
        <div className="flex items-center gap-2.5 px-4 py-4 border-b border-white/[0.06]">
          <div className="w-7 h-7 rounded-lg bg-gradient-to-br from-blue-500 to-violet-600 flex items-center justify-center flex-shrink-0">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="white" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <path d="M12 2L2 7l10 5 10-5-10-5z"/><path d="M2 17l10 5 10-5"/><path d="M2 12l10 5 10-5"/>
            </svg>
          </div>
          <span className="font-semibold text-sm text-white/90">Agent Anywhere</span>
        </div>

        {/* New chat */}
        <div className="px-3 py-3">
          <button onClick={() => { newConversation(); setSidebarOpen(false); }}
            className="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm text-white/60 hover:text-white/90 hover:bg-white/[0.06] transition-colors">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>
            </svg>
            新对话
          </button>
        </div>

        {/* Conversation list */}
        <div className="flex-1 px-3 overflow-y-auto">
          <p className="px-3 py-1 text-xs font-medium text-white/30 uppercase tracking-wider">对话记录</p>
          <div className="mt-1 space-y-0.5">
            {conversations.map(conv => (
              <ConversationItem key={conv.id} conv={conv} agents={agents}
                onSwitch={() => { switchConversation(conv.id); setSidebarOpen(false); }}
                onDelete={() => deleteConversation(conv.id)} />
            ))}
          </div>
        </div>

        {/* Bottom */}
        <div className="px-4 py-3 border-t border-white/[0.06] space-y-2">
          <button onClick={handleLogout}
            className="w-full flex items-center gap-2 px-3 py-1.5 rounded-lg text-xs text-white/40 hover:text-white/70 hover:bg-white/[0.06] transition-colors">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/>
            </svg>
            退出登录
          </button>
        </div>
      </aside>

      {/* ── Main ── */}
      <main className="flex flex-col flex-1 min-w-0 h-full">
        {/* Mobile header */}
        <header className="flex items-center gap-3 px-4 py-3 border-b border-white/[0.06] lg:hidden flex-shrink-0">
          <button onClick={() => setSidebarOpen(true)}
            className="p-1.5 rounded-md text-white/50 hover:text-white/80 hover:bg-white/[0.06] transition-colors">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <line x1="3" y1="6" x2="21" y2="6"/><line x1="3" y1="12" x2="21" y2="12"/><line x1="3" y1="18" x2="21" y2="18"/>
            </svg>
          </button>
          <span className="font-semibold text-sm text-white/80">Agent Anywhere</span>
          <div className="ml-auto"><StatusDot status={status} compact /></div>
        </header>

        {/* Messages */}
        <div className="flex-1 overflow-y-auto">
          {messages.length === 0 ? (
            <EmptyState agentName={currentAgent?.name} agents={agents} onSelectAgent={switchAgent} />
          ) : (
            <div className="max-w-3xl mx-auto px-4 py-6 space-y-1">
              {messages.map((m, i) => (
                <MessageRow key={m.id} message={m} isLatestTool={m.role === "tool" && busy && i === messages.length - 1}
                  agentColor={agents.find(a => a.id === m.agentId)?.color}
                  agentName={agents.find(a => a.id === m.agentId)?.name} />
              ))}
              {waitingReply && <ThinkingIndicator />}
              <div ref={bottomRef} />
            </div>
          )}
        </div>

        {/* Input area */}
        <div className="flex-shrink-0 bg-[#0a0a0a] px-4 py-3">
          <div className="max-w-3xl mx-auto">
            {queue.length > 0 && (
              <div className="mb-2 space-y-1">
                {queue.map((text, i) => (
                  <div key={i} className="flex items-center gap-2 px-3 py-1.5 rounded-lg bg-white/[0.04] border border-white/[0.08]">
                    <span className="text-xs text-white/30">#{i+1}</span>
                    <span className="text-sm text-white/60 truncate flex-1">{text}</span>
                    <button onClick={() => setQueue(prev => prev.filter((_, idx) => idx !== i))} className="text-white/20 hover:text-white/50">
                      <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
                    </button>
                  </div>
                ))}
              </div>
            )}
            <div className={["rounded-2xl border bg-[#161616] transition-colors",
              "border-white/[0.12] focus-within:border-white/25"].join(" ")}>
              {/* 上面一行：输入框 */}
              <div className="px-4 pt-3 pb-2">
                <textarea ref={textareaRef} value={input} onChange={e => setInput(e.target.value)} onKeyDown={onKeyDown}
                  placeholder={!ready ? "连接中，可先输入内容…" : busy ? "输入下一条消息…" : "发消息（Enter 发送，Shift+Enter 换行）"}
                  rows={1}
                  className="w-full bg-transparent text-sm text-white/90 placeholder:text-white/25 outline-none resize-none leading-relaxed" />
              </div>
              {/* 下面一行：左侧参数选择，右侧发送按钮 */}
              <div className="flex items-center px-3 pb-3">
                <div className="flex items-center gap-2 flex-1 min-w-0">
                  <AgentPicker agents={agents} currentAgentId={currentAgentId} onSwitch={switchAgent}
                    configOptions={configOptions} onSetConfig={(id, val) => {
                      // 乐观更新：UI 立即反映用户选择，权威值由后端 configOptions 事件确认
                      setConfigOptions(prev => prev.map(o =>
                        o.id === id ? { ...o, currentValue: val } : o
                      ));
                      const ws = wsRef.current;
                      if (ws && ws.readyState === WebSocket.OPEN) {
                        ws.send(JSON.stringify({ type: "setConfigOption", config_id: id, value: val }));
                      }
                    }} />
                  <StatusDot status={status} compact />
                </div>
                <button onClick={send} disabled={!input.trim()}
                  className={["flex-shrink-0 w-8 h-8 rounded-full flex items-center justify-center transition-all",
                    input.trim() ? "bg-white text-black hover:bg-white/90 active:scale-95" : "bg-white/[0.06] text-white/20 cursor-not-allowed"].join(" ")}>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                    <line x1="12" y1="19" x2="12" y2="5"/><polyline points="5 12 12 5 19 12"/>
                  </svg>
                </button>
              </div>
            </div>
          </div>
        </div>
      </main>
    </div>
  );
}

// ─── Agent Picker (below input, like Cursor) ─────────────────────────────────

function AgentPicker({ agents, currentAgentId, onSwitch, configOptions, onSetConfig }: {
  agents: AgentConfig[];
  currentAgentId: string | null;
  onSwitch: (id: string) => void;
  configOptions: import("./types").AcpConfigOption[];
  onSetConfig: (configId: string, value: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const current = agents.find(a => a.id === currentAgentId);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handleClick(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    }
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [open]);

  return (
    <div className="flex items-center gap-2 flex-wrap" ref={ref}>
      {/* Agent selector */}
      <div className="relative">
        <button onClick={() => setOpen(!open)}
          className="flex items-center gap-1.5 px-2 py-1 rounded-md text-xs text-white/40 hover:text-white/70 hover:bg-white/[0.06] transition-colors">
          {current && <div className="w-2 h-2 rounded-full" style={{ background: current.color }} />}
          <span>{current?.name || currentAgentId || "智能体"}</span>
          <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
            <polyline points="6 9 12 15 18 9"/>
          </svg>
        </button>
        {open && (
          <div className="absolute bottom-full left-0 mb-1 w-48 py-1 rounded-lg bg-[#1a1a1a] border border-white/[0.1] shadow-xl z-50">
            {agents.map(agent => (
              <button key={agent.id}
                onClick={() => { onSwitch(agent.id); setOpen(false); }}
                className={["w-full flex items-center gap-2.5 px-3 py-2 text-xs text-left transition-colors",
                  agent.id === currentAgentId ? "text-white/90 bg-white/[0.06]" : "text-white/50 hover:text-white/80 hover:bg-white/[0.04]"
                ].join(" ")}>
                <div className="w-2.5 h-2.5 rounded-full flex-shrink-0" style={{ background: agent.color }} />
                <div className="flex-1 min-w-0">
                  <div className="truncate">{agent.name}</div>
                  {agent.description && <div className="text-white/25 truncate">{agent.description}</div>}
                </div>
                {agent.id === currentAgentId && <span className="text-white/30">✓</span>}
              </button>
            ))}
          </div>
        )}
      </div>

      {/* ACP Config Options (动态从 agent 获取) */}
      {configOptions.map(opt => (
        <select key={opt.id}
          value={opt.currentValue || ""}
          onChange={e => onSetConfig(opt.id, e.target.value)}
          title={opt.description || opt.name}
          className="px-1.5 py-0.5 rounded text-xs bg-white/[0.04] border border-white/[0.08] text-white/50 outline-none hover:border-white/[0.15] focus:border-white/[0.2] transition-colors cursor-pointer max-w-[180px]"
        >
          {(opt.options || []).map(v => (
            <option key={v.value} value={v.value}>{v.name}</option>
          ))}
        </select>
      ))}
    </div>
  );
}

// ─── Conversation Item (with delete) ──────────────────────────────────────────

function ConversationItem({ conv, agents, onSwitch, onDelete }: {
  conv: Conversation; agents: AgentConfig[]; onSwitch: () => void; onDelete: () => void;
}) {
  const [hovered, setHovered] = useState(false);

  return (
    <div className="relative group" onMouseEnter={() => setHovered(true)} onMouseLeave={() => setHovered(false)}>
      <button onClick={onSwitch}
        className={["w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors text-left pr-8",
          conv.active ? "bg-white/[0.06] text-white/80" : "text-white/40 hover:text-white/70 hover:bg-white/[0.03]"
        ].join(" ")}>
        {conv.agent_id && (
          <div className="w-2 h-2 rounded-full flex-shrink-0"
            style={{ background: agents.find(a => a.id === conv.agent_id)?.color || "#666" }} />
        )}
        <span className="truncate flex-1">{conv.title}</span>
      </button>
      {hovered && (
        <button onClick={(e) => { e.stopPropagation(); onDelete(); }}
          className="absolute right-2 top-1/2 -translate-y-1/2 p-1 rounded text-white/20 hover:text-red-400 hover:bg-white/[0.06] transition-colors"
          title="删除对话">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>
          </svg>
        </button>
      )}
    </div>
  );
}

// ─── Empty State ──────────────────────────────────────────────────────────────

function EmptyState({ agentName, agents, onSelectAgent }: { agentName?: string; agents: AgentConfig[]; onSelectAgent: (id: string) => void }) {
  return (
    <div className="flex flex-col items-center justify-center h-full px-4 text-center">
      <div className="w-14 h-14 rounded-2xl bg-gradient-to-br from-blue-500/20 to-violet-600/20 border border-white/[0.08] flex items-center justify-center mb-5">
        <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="rgba(255,255,255,0.5)" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
          <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>
        </svg>
      </div>
      <h2 className="text-lg font-semibold text-white/80 mb-1">
        {agentName ? `与 ${agentName} 对话` : "选择智能体开始对话"}
      </h2>
      <p className="text-sm text-white/35 max-w-xs mb-6">
        发送消息开始交互，支持 Markdown 和代码高亮。可随时切换智能体。
      </p>
      {agents.length > 1 && (
        <div className="flex flex-wrap gap-2 justify-center">
          {agents.map(a => (
            <button key={a.id} onClick={() => onSelectAgent(a.id)}
              className="flex items-center gap-2 px-4 py-2 rounded-lg bg-white/[0.04] border border-white/[0.08] text-sm text-white/60 hover:text-white/90 hover:bg-white/[0.08] transition-colors">
              <div className="w-2.5 h-2.5 rounded-full" style={{ background: a.color }} />
              {a.name}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

// ─── Thinking Indicator ───────────────────────────────────────────────────────

function ThinkingIndicator() {
  return (
    <div className="py-2 msg-enter">
      <div className="flex items-center gap-1.5">
        <span className="thinking-dot bg-white/40" /><span className="thinking-dot bg-white/40" /><span className="thinking-dot bg-white/40" />
      </div>
    </div>
  );
}

// ─── Message Row ──────────────────────────────────────────────────────────────

function MessageRow({ message, isLatestTool, agentColor, agentName }: { message: ChatMessage; isLatestTool?: boolean; agentColor?: string; agentName?: string }) {
  if (message.role === "user") {
    return (
      <div className="flex justify-end py-1 msg-enter">
        <div className="max-w-[80%] sm:max-w-[70%] px-4 py-2.5 rounded-2xl rounded-tr-sm bg-[#1e1e2e] border border-white/[0.08] text-sm text-white/90 whitespace-pre-wrap leading-relaxed">
          {message.text}
        </div>
      </div>
    );
  }
  if (message.role === "tool") return <ToolCallRow message={message} defaultOpen={!!isLatestTool} />;
  if (message.role === "error") return <div className="py-1 msg-enter"><div className="text-sm text-red-400">{message.text}</div></div>;

  // agent message
  return (
    <div className="py-1 msg-enter">
      {agentName && (
        <div className="flex items-center gap-1.5 mb-1">
          {agentColor && <div className="w-2 h-2 rounded-full" style={{ background: agentColor }} />}
          <span className="text-xs text-white/35">{agentName}</span>
        </div>
      )}
      <div className="text-sm text-white/85">
        <div className={`prose-chat break-words overflow-x-auto ${message.streaming ? "is-streaming" : ""}`}>
          <MarkdownErrorBoundary fallback={message.text}>
            <MarkdownContent text={message.text} streaming={message.streaming} />
          </MarkdownErrorBoundary>
          {message.streaming && <span className="streaming-cursor opacity-70" />}
        </div>
      </div>
    </div>
  );
}

// ─── Tool Call Row ────────────────────────────────────────────────────────────

function ToolCallRow({ message, defaultOpen }: { message: ChatMessage; defaultOpen: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  const hasInput = message.meta?.input != null;
  useEffect(() => { if (defaultOpen) setOpen(true); }, [defaultOpen]);
  useEffect(() => { if (!defaultOpen && open) setOpen(false); }, [defaultOpen]);

  return (
    <div className="py-1 msg-enter">
      <div className="rounded-md bg-white/[0.03] border border-white/[0.06] overflow-hidden">
        <button onClick={() => hasInput && setOpen(!open)}
          className={["w-full flex items-center gap-2 px-3 py-1.5 text-xs font-mono text-left", hasInput ? "cursor-pointer hover:bg-white/[0.03]" : "cursor-default"].join(" ")}>
          {defaultOpen ? <span className="text-blue-400/80 animate-pulse">●</span> : <span className="text-emerald-400/70">✓</span>}
          <span className="text-white/50">{message.meta?.kind ?? "tool"}</span>
          <span className="text-white/20">→</span>
          <span className="text-white/60 truncate flex-1">{message.text}</span>
          {hasInput && <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"
            className={`text-white/30 transition-transform duration-150 ${open ? "rotate-180" : ""}`}><polyline points="6 9 12 15 18 9"/></svg>}
        </button>
        {open && hasInput && (
          <div className="px-3 py-2 border-t border-white/[0.06] bg-white/[0.02]">
            <pre className="text-xs text-white/50 whitespace-pre-wrap break-all leading-relaxed overflow-x-auto max-h-48 overflow-y-auto">
              {typeof message.meta!.input === "string" ? message.meta!.input : JSON.stringify(message.meta!.input, null, 2)}
            </pre>
          </div>
        )}
      </div>
    </div>
  );
}

// ─── Markdown ─────────────────────────────────────────────────────────────────

function closeOpenCodeFences(text: string): string {
  const fenceRegex = /^(`{3,})/gm;
  let open = false;
  let match: RegExpExecArray | null;
  while ((match = fenceRegex.exec(text)) !== null) open = !open;
  return open ? text + "\n```" : text;
}

function MarkdownContent({ text, streaming }: { text: string; streaming?: boolean }) {
  const source = streaming ? closeOpenCodeFences(text) : text;
  return (
    <ReactMarkdown remarkPlugins={[remarkGfm]} components={{
      pre({ node, children }: any) {
        // 代码块：pre 内部包含 code 元素
        // 提取 code 子节点的信息来渲染 CodeBlock
        const codeChild = node?.children?.find((c: any) => c.tagName === "code");
        if (codeChild) {
          const className = codeChild.properties?.className?.[0] || "";
          const match = /language-(\w+)/.exec(className);
          const lang = match?.[1] ?? "";
          // 提取文本内容
          const codeText = codeChild.children
            ?.map((c: any) => c.value ?? "")
            .join("")
            .replace(/\n$/, "") ?? "";
          return <CodeBlock lang={lang} code={codeText} />;
        }
        // fallback: 直接渲染 children
        return <pre>{children}</pre>;
      },
      code({ className, children, ...props }: any) {
        // 这里只处理行内代码（不在 pre 内的 code）
        // 代码块已经由 pre 组件处理了
        return <code className="inline-code" {...props}>{children}</code>;
      },
    }}>{source}</ReactMarkdown>
  );
}

function CodeBlock({ lang, code }: { lang: string; code: string }) {
  const [copied, setCopied] = useState(false);
  function copy() { navigator.clipboard.writeText(code).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500); }); }
  return (
    <div className="code-block-wrapper my-2">
      <div className="code-block-header"><span>{lang || "code"}</span><button className="code-copy-btn" onClick={copy}>{copied ? "✓ 已复制" : "复制"}</button></div>
      <SyntaxHighlighter language={lang || "text"} style={oneDark}
        customStyle={{ margin: 0, padding: "12px 16px", background: "#0d1117", fontSize: "13px", lineHeight: "1.6" }}
        showLineNumbers={code.split("\n").length > 5}
        lineNumberStyle={{ color: "rgba(255,255,255,0.2)", minWidth: "2.5em" }}>{code}</SyntaxHighlighter>
    </div>
  );
}

// ─── Status Dot ───────────────────────────────────────────────────────────────

function StatusDot({ status, compact }: { status: Status; compact?: boolean }) {
  const configs = {
    connecting: { color: "bg-yellow-400", pulse: true, label: "连接中" },
    ready:      { color: "bg-emerald-400", pulse: false, label: "已连接" },
    closed:     { color: "bg-white/30", pulse: false, label: "重连中..." },
    error:      { color: "bg-red-400", pulse: false, label: "连接错误" },
  };
  const cfg = configs[status.kind];
  if (compact) return <div className={`w-2 h-2 rounded-full ${cfg.color} ${cfg.pulse ? "animate-pulse" : ""}`} />;
  return (
    <div className="flex items-center gap-2">
      <div className={`w-2 h-2 rounded-full flex-shrink-0 ${cfg.color} ${cfg.pulse ? "animate-pulse" : ""}`} />
      <span className="text-xs text-white/40">{cfg.label}</span>
    </div>
  );
}
