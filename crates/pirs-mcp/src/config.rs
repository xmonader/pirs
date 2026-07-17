use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub enum ServerTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
        mode: String,
    },
}

#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: String,
    pub transport: ServerTransport,
    pub cwd: Option<String>,
}

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
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(rename = "type")]
    transport: Option<String>,
}

pub fn interpolate(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let var = &after[..end];
                out.push_str(&std::env::var(var).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
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
            if let Some(url) = entry.url {
                let transport = entry
                    .transport
                    .as_deref()
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "auto".to_string());
                specs.push(ServerSpec {
                    name,
                    transport: ServerTransport::Http {
                        url: interpolate(&url),
                        headers: entry
                            .headers
                            .into_iter()
                            .map(|(k, v)| (k, interpolate(&v)))
                            .collect(),
                        mode: transport,
                    },
                    cwd: Some(cwd.to_string_lossy().to_string()),
                });
                continue;
            }
            let Some(command) = entry.command else {
                errors.push(format!("MCP server '{name}': no command or url configured"));
                continue;
            };
            specs.push(ServerSpec {
                name,
                transport: ServerTransport::Stdio {
                    command: interpolate(&command),
                    args: entry.args.iter().map(|a| interpolate(a)).collect(),
                    env: entry
                        .env
                        .into_iter()
                        .map(|(k, v)| (k, interpolate(&v)))
                        .collect(),
                },
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
        match &specs[0].transport {
            ServerTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args[1], "@modelcontextprotocol/server-filesystem");
                assert_eq!(env["FOO"], "bar");
            }
            _ => panic!("expected stdio transport"),
        }
    }

    #[test]
    fn http_servers_become_http_specs() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path().join("no-home"));
        std::env::set_var("PIRS_TEST_KEY", "sekrit");
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers": {"remote": {"url": "http://localhost:3000/sse", "headers": {"Authorization": "Bearer ${PIRS_TEST_KEY}"}}}}"#,
        )
        .unwrap();
        let (specs, errors) = load_server_specs(dir.path());
        assert!(errors.is_empty());
        assert_eq!(specs.len(), 1);
        match &specs[0].transport {
            crate::config::ServerTransport::Http { url, headers, .. } => {
                assert_eq!(url, "http://localhost:3000/sse");
                assert_eq!(headers["Authorization"], "Bearer sekrit");
            }
            _ => panic!("expected http transport"),
        }
    }
}
