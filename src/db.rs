//! SQLite 数据库模块：管理用户、对话、消息的持久化。
//! 对话与智能体解耦：每条消息记录产生它的 agent_id，对话可随时切换智能体。

use rusqlite::{Connection, params};
use serde::Serialize;
use std::sync::Mutex;

pub struct Database {
    conn: Mutex<Connection>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Conversation {
    pub id: String,
    pub user_id: String,
    pub title: String,
    pub active: bool,
    /// 当前选中的智能体 ID（可为空，表示未选择）
    pub agent_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct StoredMessage {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub text: String,
    pub meta: Option<String>,
    /// 产生此消息的智能体 ID（user 消息为 None）
    pub agent_id: Option<String>,
    pub created_at: String,
}

impl Database {
    pub fn new(path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '新对话',
                active INTEGER NOT NULL DEFAULT 1,
                agent_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (user_id) REFERENCES users(id)
            );

            CREATE INDEX IF NOT EXISTS idx_conversations_user
                ON conversations(user_id, active, updated_at DESC);

            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                role TEXT NOT NULL,
                text TEXT NOT NULL,
                meta TEXT,
                agent_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_conversation
                ON messages(conversation_id, created_at ASC);
            ",
        )?;

        // 迁移：给旧表加 agent_id 列（如果不存在）
        let _ = conn.execute_batch(
            "ALTER TABLE conversations ADD COLUMN agent_id TEXT;
             ALTER TABLE messages ADD COLUMN agent_id TEXT;"
        );

        // 智能体配置选项持久化表
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS agent_config_selections (
                agent_id TEXT NOT NULL,
                config_id TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (agent_id, config_id)
            );
            ",
        )?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn get_or_create_user(&self) -> Result<String, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row("SELECT id FROM users LIMIT 1", [], |row| row.get(0))
            .ok();
        if let Some(id) = existing {
            return Ok(id);
        }
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute("INSERT INTO users (id) VALUES (?1)", params![id])?;
        Ok(id)
    }

    /// 获取用户的活跃对话，如果没有则创建一个
    pub fn get_or_create_active_conversation(
        &self,
        user_id: &str,
        default_agent_id: Option<&str>,
    ) -> Result<String, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM conversations WHERE user_id = ?1 AND active = 1 ORDER BY updated_at DESC LIMIT 1",
                params![user_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = existing {
            return Ok(id);
        }
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO conversations (id, user_id, agent_id) VALUES (?1, ?2, ?3)",
            params![id, user_id, default_agent_id],
        )?;
        Ok(id)
    }

    /// 获取活跃对话信息
    pub fn get_active_conversation(&self, user_id: &str) -> Result<Option<Conversation>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, user_id, title, active, agent_id, created_at, updated_at
             FROM conversations
             WHERE user_id = ?1 AND active = 1
             ORDER BY updated_at DESC LIMIT 1",
            params![user_id],
            |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    user_id: row.get(1)?,
                    title: row.get(2)?,
                    active: row.get::<_, i32>(3)? != 0,
                    agent_id: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        );
        match result {
            Ok(conv) => Ok(Some(conv)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 获取指定对话
    pub fn get_conversation(&self, conversation_id: &str) -> Result<Option<Conversation>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, user_id, title, active, agent_id, created_at, updated_at
             FROM conversations WHERE id = ?1",
            params![conversation_id],
            |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    user_id: row.get(1)?,
                    title: row.get(2)?,
                    active: row.get::<_, i32>(3)? != 0,
                    agent_id: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        );
        match result {
            Ok(conv) => Ok(Some(conv)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 列出用户的所有对话
    pub fn list_conversations(&self, user_id: &str) -> Result<Vec<Conversation>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, title, active, agent_id, created_at, updated_at
             FROM conversations WHERE user_id = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![user_id], |row| {
            Ok(Conversation {
                id: row.get(0)?,
                user_id: row.get(1)?,
                title: row.get(2)?,
                active: row.get::<_, i32>(3)? != 0,
                agent_id: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// 切换对话的智能体
    pub fn set_conversation_agent(
        &self,
        conversation_id: &str,
        agent_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET agent_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![agent_id, conversation_id],
        )?;
        Ok(())
    }

    /// 保存消息（带 agent_id）
    pub fn save_message(
        &self,
        conversation_id: &str,
        role: &str,
        text: &str,
        meta: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<String, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, text, meta, agent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, conversation_id, role, text, meta, agent_id],
        )?;
        conn.execute(
            "UPDATE conversations SET updated_at = datetime('now') WHERE id = ?1",
            params![conversation_id],
        )?;
        // 第一条用户消息作为标题
        let msg_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
            params![conversation_id],
            |row| row.get(0),
        )?;
        if msg_count == 1 && role == "user" {
            let title: String = if text.chars().count() > 50 {
                text.chars().take(50).collect::<String>() + "..."
            } else {
                text.to_string()
            };
            conn.execute(
                "UPDATE conversations SET title = ?1 WHERE id = ?2",
                params![title, conversation_id],
            )?;
        }
        Ok(id)
    }

    /// 获取对话的所有消息
    pub fn get_messages(&self, conversation_id: &str) -> Result<Vec<StoredMessage>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, role, text, meta, agent_id, created_at
             FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| {
            Ok(StoredMessage {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                role: row.get(2)?,
                text: row.get(3)?,
                meta: row.get(4)?,
                agent_id: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// 创建新对话（将旧活跃对话归档）
    pub fn new_conversation(&self, user_id: &str, agent_id: Option<&str>) -> Result<String, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET active = 0 WHERE user_id = ?1 AND active = 1",
            params![user_id],
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO conversations (id, user_id, agent_id) VALUES (?1, ?2, ?3)",
            params![id, user_id, agent_id],
        )?;
        Ok(id)
    }

    /// 切换到指定对话（设为活跃，其他归档）
    pub fn switch_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET active = 0 WHERE user_id = ?1 AND active = 1",
            params![user_id],
        )?;
        conn.execute(
            "UPDATE conversations SET active = 1, updated_at = datetime('now') WHERE id = ?1 AND user_id = ?2",
            params![conversation_id, user_id],
        )?;
        Ok(())
    }

    /// 删除对话及其所有消息
    pub fn delete_conversation(&self, conversation_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM messages WHERE conversation_id = ?1", params![conversation_id])?;
        conn.execute("DELETE FROM conversations WHERE id = ?1", params![conversation_id])?;
        Ok(())
    }

    // ─── Agent Config Selections ──────────────────────────────────────────────

    /// 保存智能体的配置选项值
    pub fn save_agent_config_selection(
        &self,
        agent_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_config_selections (agent_id, config_id, value, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(agent_id, config_id) DO UPDATE SET value = ?3, updated_at = datetime('now')",
            params![agent_id, config_id, value],
        )?;
        Ok(())
    }

    /// 获取智能体的所有已保存配置选项值
    pub fn get_agent_config_selections(
        &self,
        agent_id: &str,
    ) -> Result<Vec<(String, String)>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT config_id, value FROM agent_config_selections WHERE agent_id = ?1",
        )?;
        let rows = stmt.query_map(params![agent_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect()
    }
}
