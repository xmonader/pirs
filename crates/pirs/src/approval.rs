use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pirs_agent::events::BeforeToolCallHook;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// No core prompts; rhai policy hooks still apply (default).
    Auto,
    /// Prompt inline for sensitive operations; remembers "always" choices.
    Ask,
    /// No prompts, no rhai policy hooks. DANGEROUS.
    Yolo,
}

impl ApprovalMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "ask" => Some(Self::Ask),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ask => "ask",
            Self::Yolo => "yolo",
        }
    }
}

const BASH_PATTERNS: &[&str] = &[
    "rm ", "rm -", "mv ", "dd ", "mkfs", "chmod", "chown", "git push", "git commit",
    "git reset", "git clean", "pip install", "pip uninstall", "npm install", "npm uninstall",
    "apt ", "apt-get", "doas", "sudo", ">", "curl ", "wget ",
];

pub fn is_sensitive(tool: &str, args: &Value, cwd: &Path) -> bool {
    match tool {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            BASH_PATTERNS.iter().any(|p| cmd.contains(p))
        }
        "edit" | "write" => {
            let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let path = if raw.starts_with('/') {
                PathBuf::from(raw)
            } else {
                cwd.join(raw)
            };
            !path.starts_with(cwd)
        }
        _ => tool.starts_with("mcp_"),
    }
}

fn bucket(tool: &str, args: &Value) -> String {
    if tool == "bash" {
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let first = cmd.split_whitespace().next().unwrap_or("");
        format!("bash:{first}")
    } else {
        tool.to_string()
    }
}

fn summarize(tool: &str, args: &Value) -> String {
    let key = match tool {
        "bash" => "command",
        "edit" | "write" | "read" => "path",
        _ => "",
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(100).collect::<String>())
        .unwrap_or_else(|| args.to_string().chars().take(100).collect())
}

pub struct ApprovalGate {
    mode: Arc<Mutex<ApprovalMode>>,
    remembered: Arc<Mutex<HashSet<String>>>,
    cwd: PathBuf,
    prompter: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl ApprovalGate {
    pub fn new(mode: ApprovalMode, cwd: PathBuf) -> Self {
        ApprovalGate {
            mode: Arc::new(Mutex::new(mode)),
            remembered: Arc::new(Mutex::new(HashSet::new())),
            cwd,
            prompter: Arc::new(|question| {
                eprint!("{question}");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                line.trim().to_string()
            }),
        }
    }

    pub fn with_prompter(mut self, f: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.prompter = Arc::new(f);
        self
    }

    pub fn shared_mode(&self) -> Arc<Mutex<ApprovalMode>> {
        Arc::clone(&self.mode)
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.lock().unwrap()
    }

    pub fn hook(&self) -> BeforeToolCallHook {
        let mode = Arc::clone(&self.mode);
        let remembered = Arc::clone(&self.remembered);
        let cwd = self.cwd.clone();
        let prompter = Arc::clone(&self.prompter);
        Arc::new(move |_id, tool, args| {
            if *mode.lock().unwrap() != ApprovalMode::Ask {
                return None;
            }
            if !is_sensitive(tool, args, &cwd) {
                return None;
            }
            let bucket = bucket(tool, args);
            if remembered.lock().unwrap().contains(&bucket) {
                return None;
            }
            let question = format!(
                "\n[approval] {tool}: {}\nallow? [y]es / [n]o / [a]lways {bucket}: ",
                summarize(tool, args)
            );
            match prompter(&question).as_str() {
                "y" | "yes" => None,
                "a" | "always" => {
                    remembered.lock().unwrap().insert(bucket);
                    None
                }
                _ => Some("denied by user".to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sensitive_classification() {
        let cwd = Path::new("/work");
        assert!(is_sensitive("bash", &json!({"command": "rm -rf x"}), cwd));
        assert!(!is_sensitive("bash", &json!({"command": "ls -la"}), cwd));
        assert!(is_sensitive("edit", &json!({"path": "/etc/passwd"}), cwd));
        assert!(!is_sensitive("edit", &json!({"path": "src/main.rs"}), cwd));
        assert!(is_sensitive("mcp_fs_delete", &json!({}), cwd));
        assert!(!is_sensitive("read", &json!({"path": "x"}), cwd));
    }

    #[test]
    fn ask_prompts_and_remembers() {
        let answers = Arc::new(Mutex::new(vec!["a".to_string()]));
        let answers2 = Arc::clone(&answers);
        let gate = ApprovalGate::new(ApprovalMode::Ask, PathBuf::from("/work"))
            .with_prompter(move |_| answers2.lock().unwrap().remove(0));
        let hook = gate.hook();
        assert!(hook("1", "bash", &json!({"command": "rm -rf x"})).is_none());
        assert!(hook("2", "bash", &json!({"command": "rm -rf y"})).is_none(), "remembered always");
        assert_eq!(answers.lock().unwrap().len(), 0);
    }

    #[test]
    fn no_denies() {
        let gate = ApprovalGate::new(ApprovalMode::Ask, PathBuf::from("/work"))
            .with_prompter(|_| "n".to_string());
        let hook = gate.hook();
        assert_eq!(
            hook("1", "bash", &json!({"command": "git push"})).as_deref(),
            Some("denied by user")
        );
    }

    #[test]
    fn auto_and_yolo_never_prompt() {
        for mode in [ApprovalMode::Auto, ApprovalMode::Yolo] {
            let gate = ApprovalGate::new(mode, PathBuf::from("/work"))
                .with_prompter(|_| panic!("should not prompt"));
            let hook = gate.hook();
            assert!(hook("1", "bash", &json!({"command": "rm -rf /"})).is_none());
        }
    }
}
