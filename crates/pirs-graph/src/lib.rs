pub mod ast_edit;
pub mod code_map;
pub mod graph;

pub use graph::{Graph, Lang, SymKind, Symbol};

/// Builds the graph lazily on first use so CLI startup never blocks on it.
pub struct LazyGraph {
    root: std::path::PathBuf,
    once: std::sync::OnceLock<Graph>,
}

impl LazyGraph {
    pub fn new(root: std::path::PathBuf) -> Self {
        LazyGraph {
            root,
            once: std::sync::OnceLock::new(),
        }
    }

    pub fn get(&self) -> &Graph {
        self.once.get_or_init(|| {
            let start = std::time::Instant::now();
            let g = Graph::build(&self.root);
            tracing::info!(
                "code graph built: {} symbols in {:.1}s",
                g.symbols.len(),
                start.elapsed().as_secs_f64()
            );
            g
        })
    }

    pub fn is_built(&self) -> bool {
        self.once.get().is_some()
    }
}
