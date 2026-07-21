//! In-session task list (Vibe `todo` class).
//!
//! Durable JSON store for the coding session; survives turns within that session.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl Default for TodoStatus {
    fn default() -> Self {
        Self::Pending
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TodoFile {
    items: Vec<TodoItem>,
}

/// Session-scoped todo store (file-backed).
#[derive(Debug, Clone)]
pub struct TodoStore {
    path: PathBuf,
}

impl TodoStore {
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        if !path.exists() {
            let empty = TodoFile::default();
            std::fs::write(&path, serde_json::to_string_pretty(&empty)?)?;
        }
        Ok(Self { path })
    }

    /// Default path under session or cwd: `{base}/todos.json`.
    pub fn default_path(base: &Path) -> PathBuf {
        base.join("todos.json")
    }

    fn read(&self) -> anyhow::Result<TodoFile> {
        let text = std::fs::read_to_string(&self.path)?;
        if text.trim().is_empty() {
            return Ok(TodoFile::default());
        }
        Ok(serde_json::from_str(&text)?)
    }

    fn write(&self, f: &TodoFile) -> anyhow::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(f)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn list(&self) -> anyhow::Result<Vec<TodoItem>> {
        Ok(self.read()?.items)
    }

    pub fn add(&self, content: &str) -> anyhow::Result<TodoItem> {
        let mut f = self.read()?;
        let id = format!("t{}", f.items.len() + 1);
        let item = TodoItem {
            id: id.clone(),
            content: content.into(),
            status: TodoStatus::Pending,
        };
        f.items.push(item.clone());
        self.write(&f)?;
        Ok(item)
    }

    pub fn update(
        &self,
        id: &str,
        content: Option<&str>,
        status: Option<TodoStatus>,
    ) -> anyhow::Result<TodoItem> {
        let mut f = self.read()?;
        let item = f
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("todo not found: {id}"))?;
        if let Some(c) = content {
            item.content = c.into();
        }
        if let Some(s) = status {
            item.status = s;
        }
        let out = item.clone();
        self.write(&f)?;
        Ok(out)
    }

    pub fn format_list(items: &[TodoItem]) -> String {
        if items.is_empty() {
            return "todo list empty".into();
        }
        let mut out = String::from("todo list:\n");
        for i in items {
            let st = match i.status {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "in_progress",
                TodoStatus::Completed => "completed",
                TodoStatus::Cancelled => "cancelled",
            };
            out.push_str(&format!("- [{}] {} — {}\n", i.id, st, i.content));
        }
        out
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TodoAction {
    Add,
    Update,
    List,
}

#[derive(Deserialize, JsonSchema)]
struct TodoArgs {
    action: TodoAction,
    /// Text for add / optional update.
    #[serde(default)]
    content: Option<String>,
    /// Id for update.
    #[serde(default)]
    id: Option<String>,
    /// Status for update: pending | in_progress | completed | cancelled.
    #[serde(default)]
    status: Option<String>,
}

pub struct TodoTool {
    store: Mutex<TodoStore>,
}

impl TodoTool {
    pub fn new(store: TodoStore) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    pub fn open_at(base: &Path) -> anyhow::Result<Self> {
        Ok(Self::new(TodoStore::open(TodoStore::default_path(base))?))
    }
}

#[async_trait]
impl AgentTool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Manage a durable in-session task checklist. Actions: add (content), \
         update (id, optional content/status), list."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TodoArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("todo: session task list (add/update/list)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: TodoArgs = serde_json::from_value(ctx.args)?;
        let store = self.store.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        match args.action {
            TodoAction::List => {
                let items = store.list()?;
                Ok(ToolOutput::text(TodoStore::format_list(&items)))
            }
            TodoAction::Add => {
                let content = args
                    .content
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("todo add requires content"))?;
                let item = store.add(&content)?;
                Ok(ToolOutput::text(format!(
                    "added todo {} — {}",
                    item.id, item.content
                )))
            }
            TodoAction::Update => {
                let id = args
                    .id
                    .ok_or_else(|| anyhow::anyhow!("todo update requires id"))?;
                let status = match args.status.as_deref() {
                    None => None,
                    Some("pending") => Some(TodoStatus::Pending),
                    Some("in_progress") | Some("in-progress") => Some(TodoStatus::InProgress),
                    Some("completed") | Some("done") => Some(TodoStatus::Completed),
                    Some("cancelled") => Some(TodoStatus::Cancelled),
                    Some(other) => anyhow::bail!("unknown status {other:?}"),
                };
                let item = store.update(&id, args.content.as_deref(), status)?;
                Ok(ToolOutput::text(format!(
                    "updated todo {} status={:?} — {}",
                    item.id, item.status, item.content
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_list_update_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("todos.json");
        let store = TodoStore::open(&path).unwrap();
        let a = store.add("ship ask_user").unwrap();
        store
            .update(&a.id, None, Some(TodoStatus::InProgress))
            .unwrap();
        store.add("wire profiles").unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status, TodoStatus::InProgress);

        let store2 = TodoStore::open(&path).unwrap();
        let list2 = store2.list().unwrap();
        assert_eq!(list2.len(), 2);
        assert!(TodoStore::format_list(&list2).contains("ship ask_user"));
    }
}
