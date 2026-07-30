//! MCP server catalog: metadata index **without** connecting every server.
//!
//! Large fleets (hundreds–thousands of configured servers) are indexed here.
//! Live connections live in [`crate::pool::McpPool`].

use crate::client::McpToolDef;
use crate::config::{ServerSpec, ServerTransport};

/// One tool known for a catalog entry (may be empty until first connect).
#[derive(Debug, Clone)]
pub struct CatalogTool {
    pub name: String,
    pub description: String,
    /// Present after list_tools / describe; may be null object until then.
    pub input_schema: Option<serde_json::Value>,
}

/// One configured MCP server in the catalog (not necessarily live).
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport_kind: String,
    /// Short non-secret summary (command basename or host).
    pub summary: String,
    pub known_tools: Vec<CatalogTool>,
    pub tags: Vec<String>,
}

impl CatalogEntry {
    pub fn from_spec(spec: &ServerSpec) -> Self {
        let (transport_kind, summary): (String, String) = match &spec.transport {
            ServerTransport::Stdio { command, args, .. } => {
                let bas = std::path::Path::new(command)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(command);
                let arg0 = args.first().map(|s| s.as_str()).unwrap_or("");
                let summary = if arg0.is_empty() {
                    bas.to_string()
                } else {
                    // Prefer script basename for python mocks.
                    let a = std::path::Path::new(arg0)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(arg0);
                    format!("{bas} {a}")
                };
                ("stdio".to_string(), summary)
            }
            ServerTransport::Http { url, mode, .. } => {
                let host = url
                    .split("://")
                    .nth(1)
                    .unwrap_or(url)
                    .split('/')
                    .next()
                    .unwrap_or(url);
                ("http".to_string(), format!("{host} ({mode})"))
            }
        };
        let mut tags = vec![transport_kind.clone()];
        // Cheap heuristic tags from name for search.
        for part in spec.name.split(|c: char| !c.is_ascii_alphanumeric()) {
            if part.len() >= 2 {
                tags.push(part.to_ascii_lowercase());
            }
        }
        Self {
            name: spec.name.clone(),
            transport_kind,
            summary,
            known_tools: Vec::new(),
            tags,
        }
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = CatalogTool>) -> Self {
        self.known_tools = tools.into_iter().collect();
        self
    }

    pub fn matches_query(&self, query: &str) -> bool {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return true;
        }
        let name = self.name.to_ascii_lowercase();
        if name.contains(&q) {
            return true;
        }
        if self.summary.to_ascii_lowercase().contains(&q) {
            return true;
        }
        if self
            .tags
            .iter()
            .any(|t| t.contains(&q) || q.contains(t.as_str()))
        {
            return true;
        }
        self.known_tools.iter().any(|t| {
            t.name.to_ascii_lowercase().contains(&q)
                || t.description.to_ascii_lowercase().contains(&q)
        })
    }
}

/// Index of configured MCP servers (cold — no live clients).
#[derive(Debug, Clone, Default)]
pub struct McpCatalog {
    entries: Vec<CatalogEntry>,
}

impl McpCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build catalog from specs **without connecting** any server.
    pub fn from_specs(specs: &[ServerSpec]) -> Self {
        Self {
            entries: specs.iter().map(CatalogEntry::from_spec).collect(),
        }
    }

    /// Build catalog and attach pre-known tools (e.g. from a previous session cache).
    pub fn from_specs_with_known_tools(
        specs: &[ServerSpec],
        tools_by_server: &std::collections::HashMap<String, Vec<McpToolDef>>,
    ) -> Self {
        let mut cat = Self::from_specs(specs);
        for e in &mut cat.entries {
            if let Some(defs) = tools_by_server.get(&e.name) {
                e.known_tools = defs
                    .iter()
                    .map(|d| CatalogTool {
                        name: d.name.clone(),
                        description: d.description.clone(),
                        input_schema: Some(d.input_schema.clone()),
                    })
                    .collect();
            }
        }
        cat
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut CatalogEntry> {
        self.entries.iter_mut().find(|e| e.name == name)
    }

    /// Search by name / summary / tags / known tool text. Empty query = all (capped).
    pub fn search(&self, query: &str, limit: usize) -> Vec<&CatalogEntry> {
        let limit = limit.clamp(1, 500);
        self.entries
            .iter()
            .filter(|e| e.matches_query(query))
            .take(limit)
            .collect()
    }

    /// Names only (cheap status line).
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    pub fn set_known_tools(&mut self, server: &str, tools: &[McpToolDef]) {
        if let Some(e) = self.get_mut(server) {
            e.known_tools = tools
                .iter()
                .map(|d| CatalogTool {
                    name: d.name.clone(),
                    description: d.description.clone(),
                    input_schema: Some(d.input_schema.clone()),
                })
                .collect();
        }
    }

    /// Synthetic bulk insert for scale tests (no real transport).
    pub fn push_synthetic(&mut self, name: impl Into<String>, tools: &[&str]) {
        let name = name.into();
        let known_tools = tools
            .iter()
            .map(|t| CatalogTool {
                name: (*t).to_string(),
                description: format!("synthetic tool {t}"),
                input_schema: Some(serde_json::json!({"type": "object", "properties": {}})),
            })
            .collect();
        self.entries.push(CatalogEntry {
            name: name.clone(),
            transport_kind: "stdio".into(),
            summary: format!("synthetic:{name}"),
            known_tools,
            tags: vec!["synthetic".into(), name.to_ascii_lowercase()],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerTransport;
    use std::collections::HashMap;

    fn stdio_spec(name: &str, cmd: &str) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            transport: ServerTransport::Stdio {
                command: cmd.into(),
                args: vec![],
                env: HashMap::new(),
            },
            cwd: None,
        }
    }

    #[test]
    fn from_specs_does_not_require_connect() {
        let specs: Vec<_> = (0..250)
            .map(|i| stdio_spec(&format!("srv-{i:04}"), "true"))
            .collect();
        let cat = McpCatalog::from_specs(&specs);
        assert_eq!(cat.len(), 250);
        assert!(cat.get("srv-0000").is_some());
        assert!(cat.get("srv-0249").is_some());
        assert!(cat.get("missing").is_none());
    }

    #[test]
    fn search_finds_by_name_and_tool() {
        let mut cat = McpCatalog::new();
        cat.push_synthetic("email-calendar", &["email_list", "calendar_get"]);
        cat.push_synthetic("github", &["create_issue"]);
        cat.push_synthetic("stripe", &["charge"]);
        let hits = cat.search("email", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "email-calendar");
        let hits = cat.search("create_issue", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "github");
        let all = cat.search("", 5);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn large_synthetic_catalog_search_is_bounded() {
        let mut cat = McpCatalog::new();
        for i in 0..1000 {
            cat.push_synthetic(format!("server-{i}"), &["ping", "pong"]);
        }
        assert_eq!(cat.len(), 1000);
        let hits = cat.search("server-42", 20);
        assert!(hits.iter().any(|e| e.name == "server-42"));
        let capped = cat.search("", 50);
        assert_eq!(capped.len(), 50);
    }
}
