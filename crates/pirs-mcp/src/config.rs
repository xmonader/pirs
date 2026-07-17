use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::client::ServerSpec;

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: HashMap<String, ServerEntry>,
    #[serde(default)]
    servers: HashMap<String, ServerEntry>,
}

#[derive(Debug, Deserialize)]
struct ServerEntry {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[allow(dead_code)]
    url: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    transport: Option<String>,
}

fn config_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![cwd.join(".mcp.json")];
    if let Ok(home) = std::env::var("HOME") {
        paths.push(Path::new(&home).join(".pirs").join("mcp.json"));
    }
    paths
}

pub fn load_server_specs(cwd: &Path) -> (Vec<ServerSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for path in config_paths(cwd) {
        let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if !seen.insert(canonical) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let config: McpConfigFile = match serde_json::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{}: invalid JSON: {e}", path.display()));
                continue;
            }
        };
        for (name, entry) in config.mcp_servers.into_iter().chain(config.servers) {
            let Some(command) = entry.command else {
                if entry.url.is_some() {
                    errors.push(format!(
                        "MCP server '{name}': HTTP transport not supported yet (stdio only)"
                    ));
                } else {
                    errors.push(format!("MCP server '{name}': no command configured"));
                }
                continue;
            };
            specs.push(ServerSpec {
                name,
                command,
                args: entry.args,
                env: entry.env,
                cwd: Some(cwd.to_string_lossy().to_string()),
            });
        }
    }
    (specs, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_claude_code_format() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path().join("no-home"));
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{
  "mcpServers": {
    "fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": {"FOO": "bar"}
    }
  }
}"#,
        )
        .unwrap();
        let (specs, errors) = load_server_specs(dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "fs");
        assert_eq!(specs[0].command, "npx");
        assert_eq!(specs[0].args[1], "@modelcontextprotocol/server-filesystem");
        assert_eq!(specs[0].env["FOO"], "bar");
    }

    #[test]
    fn http_servers_report_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path().join("no-home"));
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers": {"remote": {"url": "http://localhost:3000/sse"}}}"#,
        )
        .unwrap();
        let (specs, errors) = load_server_specs(dir.path());
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("HTTP transport not supported"));
    }
}
