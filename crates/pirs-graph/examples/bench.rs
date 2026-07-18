fn main() {
    let root = std::env::args().nth(1).unwrap();
    let start = std::time::Instant::now();
    let g = pirs_graph::Graph::build(std::path::Path::new(&root));
    let el = start.elapsed();
    println!(
        "{}: {} symbols in {:?} ({:.2}s)",
        root,
        g.symbols.len(),
        el,
        el.as_secs_f64()
    );
}
