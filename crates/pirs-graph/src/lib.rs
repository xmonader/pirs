pub mod ast_edit;
pub mod code_map;
pub mod graph;
pub mod store;

pub use graph::{Graph, Lang, SymKind, Symbol};
pub use store::{full_graph, GraphStore};

/// Builds the graph lazily on first use so CLI startup never blocks on it.
/// An editing agent invalidates and rebuilds as files change.
pub struct LazyGraph {
    root: std::path::PathBuf,
    /// When set, symbols are cached in this SQLite file and refreshes re-parse
    /// only changed files. When `None`, every build is a full parse (the
    /// original behavior — the toggle-off path).
    db_path: Option<std::path::PathBuf>,
    state: std::sync::RwLock<Option<std::sync::Arc<Graph>>>,
}

impl LazyGraph {
    pub fn new(root: std::path::PathBuf) -> Self {
        LazyGraph {
            root,
            db_path: None,
            state: std::sync::RwLock::new(None),
        }
    }

    /// Like [`LazyGraph::new`] but backs the graph with a persistent,
    /// incrementally-refreshed store at `db_path`.
    pub fn persistent(root: std::path::PathBuf, db_path: std::path::PathBuf) -> Self {
        LazyGraph {
            root,
            db_path: Some(db_path),
            state: std::sync::RwLock::new(None),
        }
    }

    fn build_now(&self) -> std::sync::Arc<Graph> {
        let start = std::time::Instant::now();
        let g = std::sync::Arc::new(self.build_graph());
        tracing::info!(
            "code graph built: {} symbols in {:.1}s{}",
            g.symbols.len(),
            start.elapsed().as_secs_f64(),
            if self.db_path.is_some() {
                " (persistent)"
            } else {
                ""
            }
        );
        g
    }

    /// Build via the persistent store when enabled, falling back to a full parse
    /// if the store can't be opened/refreshed — the graph is an aid, never a
    /// hard dependency, so a cache problem must never break the agent.
    fn build_graph(&self) -> Graph {
        let Some(db) = &self.db_path else {
            return Graph::build(&self.root);
        };
        match GraphStore::open(db, &self.root).and_then(|mut s| s.load_graph()) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("graph store unavailable ({e:#}); falling back to full parse");
                Graph::build(&self.root)
            }
        }
    }

    pub fn get(&self) -> std::sync::Arc<Graph> {
        if let Some(g) = self.state.read().unwrap().as_ref() {
            return std::sync::Arc::clone(g);
        }
        let g = self.build_now();
        *self.state.write().unwrap() = Some(std::sync::Arc::clone(&g));
        g
    }

    pub fn invalidate(&self) {
        *self.state.write().unwrap() = None;
    }

    pub fn is_built(&self) -> bool {
        self.state.read().unwrap().is_some()
    }
}
