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
    state: std::sync::RwLock<Option<std::sync::Arc<Graph>>>,
}

impl LazyGraph {
    pub fn new(root: std::path::PathBuf) -> Self {
        LazyGraph {
            root,
            state: std::sync::RwLock::new(None),
        }
    }

    fn build_now(&self) -> std::sync::Arc<Graph> {
        let start = std::time::Instant::now();
        let g = std::sync::Arc::new(Graph::build(&self.root));
        tracing::info!(
            "code graph built: {} symbols in {:.1}s",
            g.symbols.len(),
            start.elapsed().as_secs_f64()
        );
        g
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
