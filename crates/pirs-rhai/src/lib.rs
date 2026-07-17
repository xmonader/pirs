use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _};
use pirs_agent::{AgentTool, Hooks, ToolExecContext, ToolOutput, ToolResultPatch};
use pirs_ai::{ContentBlock, ToolResultMessage};
use rhai::{Dynamic, Engine, Scope, AST};
use serde_json::Value;

pub struct RegisteredTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    ext: usize,
}

pub struct Extension {
    pub path: PathBuf,
    ast: AST,
    scope: Scope<'static>,
    has_on_tool_call: bool,
    has_on_tool_result: bool,
}

pub struct ExtensionHost {
    engine: Arc<Mutex<Engine>>,
    extensions: Vec<Mutex<Extension>>,
    tool_registry: Vec<RegisteredTool>,
    pub load_errors: Vec<String>,
}

impl ExtensionHost {
    pub fn new() -> Self {
        let mut engine = Engine::new();
        engine.set_max_operations(200_000);
        engine.set_max_call_levels(32);
        ExtensionHost {
            engine: Arc::new(Mutex::new(engine)),
            extensions: Vec::new(),
            tool_registry: Vec::new(),
            load_errors: Vec::new(),
        }
    }

    pub fn load_default_dirs(&mut self, cwd: &Path) {
        let mut dirs = vec![cwd.join(".pirs").join("extensions")];
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(Path::new(&home).join(".pirs").join("extensions"));
        }
        for dir in dirs {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut scripts: Vec<PathBuf> = read
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rhai"))
                .collect();
            scripts.sort();
            for script in scripts {
                if let Err(e) = self.load_script(&script) {
                    self.load_errors
                        .push(format!("{}: {e}", script.display()));
                }
            }
        }
    }

    pub fn load_script(&mut self, path: &Path) -> anyhow::Result<()> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.load_source(&source, path.display().to_string())
    }

    pub fn load_source(&mut self, source: &str, name: String) -> anyhow::Result<()> {
        let ext_index = self.extensions.len();
        let registered: Arc<Mutex<Vec<(String, String, rhai::Map)>>> =
            Arc::new(Mutex::new(Vec::new()));

        let mut engine = Engine::new();
        engine.set_max_operations(200_000);
        engine.set_max_call_levels(32);

        let registrations = Arc::clone(&registered);
        engine.register_fn(
            "register_tool",
            move |name: &str, description: &str, schema: rhai::Map| {
                registrations
                    .lock()
                    .unwrap()
                    .push((name.to_string(), description.to_string(), schema));
            },
        );

        let ast = engine
            .compile(source)
            .map_err(|e| anyhow!("parse error in {name}: {e}"))?;

        let has_on_tool_call = ast.iter_functions().any(|f| f.name == "on_tool_call");
        let has_on_tool_result = ast.iter_functions().any(|f| f.name == "on_tool_result");

        let mut scope = Scope::new();
        engine
            .run_ast_with_scope(&mut scope, &ast)
            .map_err(|e| anyhow!("error evaluating {name}: {e}"))?;

        let mut ast = ast;
        ast.clear_statements();

        let declared = registered.lock().unwrap().clone();

        for (tool_name, description, schema_map) in declared {
            let fn_name = format!("tool_{tool_name}");
            if !ast.iter_functions().any(|f| f.name == fn_name) {
                return Err(anyhow!(
                    "{name}: register_tool(\"{tool_name}\") requires a function `fn {fn_name}(args)`"
                ));
            }
            let schema = rhai::serde::from_dynamic(&Dynamic::from_map(schema_map))
                .unwrap_or(Value::Object(serde_json::Map::new()));
            self.tool_registry.push(RegisteredTool {
                name: tool_name.clone(),
                description,
                schema,
                ext: ext_index,
            });
        }

        self.extensions.push(Mutex::new(Extension {
            path: PathBuf::from(name),
            ast,
            scope,
            has_on_tool_call,
            has_on_tool_result,
        }));
        Ok(())
    }

    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn AgentTool>> {
        self.tool_registry
            .iter()
            .map(|t| {
                Arc::new(RhaiTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    schema: t.schema.clone(),
                    host: Arc::clone(self),
                    ext: t.ext,
                }) as Arc<dyn AgentTool>
            })
            .collect()
    }

    pub fn hooks(self: &Arc<Self>) -> Hooks {
        let mut hooks = Hooks::default();
        let has_call = self
            .extensions
            .iter()
            .any(|e| e.lock().unwrap().has_on_tool_call);
        let has_result = self
            .extensions
            .iter()
            .any(|e| e.lock().unwrap().has_on_tool_result);

        if has_call {
            let host = Arc::clone(self);
            hooks.before_tool_call = Some(Arc::new(move |id, name, args| {
                host.run_on_tool_call(id, name, args)
            }));
        }
        if has_result {
            let host = Arc::clone(self);
            hooks.after_tool_call = Some(Arc::new(move |id, name, result| {
                host.run_on_tool_result(id, name, result)
            }));
        }
        hooks
    }

    fn run_on_tool_call(&self, id: &str, name: &str, args: &Value) -> Option<String> {
        for ext in &self.extensions {
            let mut ext = ext.lock().unwrap();
            if !ext.has_on_tool_call {
                continue;
            }
            let dynamic_args = rhai::serde::to_dynamic(args).unwrap_or(Dynamic::UNIT);
            let engine = self.engine.lock().unwrap();
            let ext = &mut *ext;
            let result: Result<Dynamic, _> = engine.call_fn(
                &mut ext.scope,
                &ext.ast,
                "on_tool_call",
                (id.to_string(), name.to_string(), dynamic_args),
            );
            match result {
                Ok(d) if d.is_unit() => continue,
                Ok(d) => {
                    if d.is::<rhai::Map>() {
                        let map = d.cast::<rhai::Map>();
                        let block = map
                            .get("block")
                            .and_then(|b| b.as_bool().ok())
                            .unwrap_or(false);
                        if block {
                            let reason = map
                                .get("reason")
                                .map(|r| r.to_string())
                                .unwrap_or_else(|| "blocked by extension".to_string());
                            return Some(reason);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("on_tool_call in {} failed: {e}", ext.path.display());
                }
            }
        }
        None
    }

    fn run_on_tool_result(
        &self,
        id: &str,
        name: &str,
        result: &ToolResultMessage,
    ) -> Option<ToolResultPatch> {
        for ext in &self.extensions {
            let mut ext = ext.lock().unwrap();
            if !ext.has_on_tool_result {
                continue;
            }
            let text: String = result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            let mut map = rhai::Map::new();
            map.insert("text".into(), text.into());
            map.insert("isError".into(), result.is_error.into());
            map.insert("terminate".into(), result.terminate.into());
            if let Some(d) = &result.details {
                map.insert(
                    "details".into(),
                    rhai::serde::to_dynamic(d).unwrap_or(Dynamic::UNIT),
                );
            }
            let engine = self.engine.lock().unwrap();
            let ext = &mut *ext;
            let call_result: Result<Dynamic, _> = engine.call_fn(
                &mut ext.scope,
                &ext.ast,
                "on_tool_result",
                (id.to_string(), name.to_string(), Dynamic::from_map(map)),
            );
            match call_result {
                Ok(d) if d.is_unit() => continue,
                Ok(d) => {
                    if !d.is::<rhai::Map>() {
                        continue;
                    }
                    let map = d.cast::<rhai::Map>();
                    let patch = ToolResultPatch {
                        content: map
                            .get("text")
                            .map(|t| vec![ContentBlock::text(t.to_string())]),
                        details: map.get("details").and_then(|d| {
                            if d.is_unit() {
                                None
                            } else {
                                rhai::serde::from_dynamic(d).ok()
                            }
                        }),
                        is_error: map.get("isError").and_then(|b| b.as_bool().ok()),
                        terminate: map.get("terminate").and_then(|b| b.as_bool().ok()),
                    };
                    return Some(patch);
                }
                Err(e) => {
                    tracing::warn!("on_tool_result in {} failed: {e}", ext.path.display());
                }
            }
        }
        None
    }

    pub fn extension_names(&self) -> Vec<String> {
        self.extensions
            .iter()
            .map(|e| e.lock().unwrap().path.display().to_string())
            .collect()
    }
}

impl Default for ExtensionHost {
    fn default() -> Self {
        Self::new()
    }
}

struct RhaiTool {
    name: String,
    description: String,
    schema: Value,
    host: Arc<ExtensionHost>,
    ext: usize,
}

#[async_trait::async_trait]
impl AgentTool for RhaiTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let host = Arc::clone(&self.host);
        let ext_index = self.ext;
        let fn_name = format!("tool_{}", self.name);
        let args = ctx.args.clone();

        let output = tokio::task::spawn_blocking(move || {
            let mut ext_guard = host.extensions[ext_index].lock().unwrap();
            let ext = &mut *ext_guard;
            let engine = host.engine.lock().unwrap();
            let dynamic_args = rhai::serde::to_dynamic(&args).unwrap_or(Dynamic::UNIT);
            let result: Result<Dynamic, _> =
                engine.call_fn(&mut ext.scope, &ext.ast, &fn_name, (dynamic_args,));
            result.map_err(|e| anyhow!("{e}"))
        })
        .await??;

        let text = if output.is_unit() {
            String::new()
        } else if output.is::<String>() {
            output.cast::<String>()
        } else if output.is::<rhai::Map>() || output.is::<rhai::Array>() {
            let json: Value = rhai::serde::from_dynamic(&output)?;
            serde_json::to_string_pretty(&json)?
        } else {
            output.to_string()
        };
        Ok(ToolOutput::text(text))
    }
}
