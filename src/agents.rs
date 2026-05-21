//! 智能体配置管理：从 agents.json 加载、保存智能体定义。
//!
//! `fallback_config_options` 使用 ACP 标准 `SessionConfigOption` 结构（camelCase）。
//! 在 agent 进程不返回 configOptions 也不返回 models 时使用。

use agent_client_protocol::schema::SessionConfigOption;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单个智能体的配置
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    /// 启动命令（可执行文件路径）
    pub command: String,
    /// 命令参数
    #[serde(default)]
    pub args: Vec<String>,
    /// 描述
    #[serde(default)]
    pub description: String,
    /// 标识颜色（hex）
    #[serde(default = "default_color")]
    pub color: String,
    /// 当 agent 不返回 configOptions / models 时使用的 fallback 配置
    /// 使用 ACP 标准 SessionConfigOption 结构（camelCase JSON）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_config_options: Option<Vec<SessionConfigOption>>,
}

fn default_color() -> String {
    "#6366f1".to_string()
}

/// 智能体配置管理器
pub struct AgentRegistry {
    config_path: PathBuf,
    agents: Mutex<Vec<AgentConfig>>,
}

impl AgentRegistry {
    /// 从配置文件加载，如果文件不存在则创建默认配置
    pub fn load(config_path: &Path) -> Self {
        let agents = if config_path.exists() {
            match std::fs::read_to_string(config_path) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                    eprintln!("⚠ 解析 agents.json 失败: {e}，使用默认配置");
                    default_agents()
                }),
                Err(e) => {
                    eprintln!("⚠ 读取 agents.json 失败: {e}，使用默认配置");
                    default_agents()
                }
            }
        } else {
            let agents = default_agents();
            // 写入默认配置
            if let Ok(json) = serde_json::to_string_pretty(&agents) {
                let _ = std::fs::write(config_path, json);
            }
            agents
        };

        Self {
            config_path: config_path.to_path_buf(),
            agents: Mutex::new(agents),
        }
    }

    /// 获取所有智能体
    pub fn list(&self) -> Vec<AgentConfig> {
        self.agents.lock().unwrap().clone()
    }

    /// 根据 ID 获取智能体
    pub fn get(&self, id: &str) -> Option<AgentConfig> {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.id == id)
            .cloned()
    }

    /// 获取第一个智能体作为默认
    pub fn default_agent(&self) -> Option<AgentConfig> {
        self.agents.lock().unwrap().first().cloned()
    }

    /// 添加智能体
    pub fn add(&self, agent: AgentConfig) -> Result<(), String> {
        let mut agents = self.agents.lock().unwrap();
        if agents.iter().any(|a| a.id == agent.id) {
            return Err(format!("智能体 ID '{}' 已存在", agent.id));
        }
        agents.push(agent);
        self.save_locked(&agents)
    }

    /// 更新智能体
    pub fn update(&self, agent: AgentConfig) -> Result<(), String> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.iter_mut().find(|a| a.id == agent.id) {
            *existing = agent;
            self.save_locked(&agents)
        } else {
            Err(format!("智能体 '{}' 不存在", agent.id))
        }
    }

    /// 删除智能体
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut agents = self.agents.lock().unwrap();
        let len_before = agents.len();
        agents.retain(|a| a.id != id);
        if agents.len() == len_before {
            return Err(format!("智能体 '{}' 不存在", id));
        }
        self.save_locked(&agents)
    }

    fn save_locked(&self, agents: &[AgentConfig]) -> Result<(), String> {
        let json = serde_json::to_string_pretty(agents).map_err(|e| format!("序列化失败: {e}"))?;
        std::fs::write(&self.config_path, json)
            .map_err(|e| format!("写入 agents.json 失败: {e}"))
    }
}

fn default_agents() -> Vec<AgentConfig> {
    vec![AgentConfig {
        id: "kiro".to_string(),
        name: "Kiro".to_string(),
        command: std::env::var("KIRO_CLI").unwrap_or_else(|_| "kiro-cli".to_string()),
        args: vec!["acp".to_string()],
        description: "Kiro AI 编程助手".to_string(),
        color: "#8b5cf6".to_string(),
        fallback_config_options: None,
    }]
}
