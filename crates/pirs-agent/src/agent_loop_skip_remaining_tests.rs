use super::*;
use async_trait::async_trait;
use crate::tool::{AgentTool, ToolExecContext, ToolOutput};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountTool {
    name: String,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentTool for CountTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "count"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("ok"))
    }
}

#[tokio::test]
async fn sequential_skips_remaining_when_predicate_true() {
    let hits = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(CountTool { name: "a".into(), hits: Arc::clone(&hits) }),
        Arc::new(CountTool { name: "b".into(), hits: Arc::clone(&hits) }),
        Arc::new(CountTool { name: "c".into(), hits: Arc::clone(&hits) }),
    ];
    let hits_for_pred = Arc::clone(&hits);
    let pred = Arc::new(move || hits_for_pred.load(Ordering::SeqCst) >= 1);
    let calls = vec![
        ToolCallData { id: "1".into(), name: "a".into(), arguments: json!({}) },
        ToolCallData { id: "2".into(), name: "b".into(), arguments: json!({}) },
        ToolCallData { id: "3".into(), name: "c".into(), arguments: json!({}) },
    ];
    let emit: Emit = Arc::new(|_| {});
    let results = execute_tool_calls_for_test(
        calls,
        &tools,
        &Hooks::default(),
        CancellationToken::new(),
        &emit,
        true,
        None,
        Some(pred.as_ref()),
    )
    .await;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "only first tool should run");
    assert_eq!(results.len(), 3);
    assert!(!results[0].is_error);
    assert!(results[1].is_error);
    assert!(results[1].model_text().contains("Skipped"));
    assert!(results[2].model_text().contains("Skipped"));
}

#[tokio::test]
async fn thrash_blocks_identical_sequential_tools() {
    let hits = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(CountTool {
        name: "bash".into(),
        hits: Arc::clone(&hits),
    })];
    let thrash = crate::thrash::ThrashGuard::with_limits(3, 10);
    let calls: Vec<_> = (0..4)
        .map(|i| ToolCallData {
            id: format!("{i}"),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        })
        .collect();
    let emit: Emit = Arc::new(|_| {});
    let results = execute_tool_calls_for_test(
        calls,
        &tools,
        &Hooks::default(),
        CancellationToken::new(),
        &emit,
        true,
        Some(&thrash),
        None,
    )
    .await;
    // First two run, third trips loop (max_repeats=3 means trip on 3rd observe)
    assert!(results.iter().any(|r| r.model_text().contains("loop detection") || r.model_text().contains("Skipped")));
    assert!(hits.load(Ordering::SeqCst) <= 3);
}

#[tokio::test]
async fn thrash_blocks_identical_parallel_tools() {
    // Default Agent tool_execution is Parallel — thrash must still arm.
    let hits = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(CountTool {
        name: "bash".into(),
        hits: Arc::clone(&hits),
    })];
    let thrash = crate::thrash::ThrashGuard::with_limits(3, 10);
    let calls: Vec<_> = (0..4)
        .map(|i| ToolCallData {
            id: format!("p{i}"),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        })
        .collect();
    let emit: Emit = Arc::new(|_| {});
    let results = execute_tool_calls_for_test(
        calls,
        &tools,
        &Hooks::default(),
        CancellationToken::new(),
        &emit,
        false, // parallel
        Some(&thrash),
        None,
    )
    .await;
    assert!(
        results
            .iter()
            .any(|r| r.model_text().contains("loop detection")
                || r.model_text().contains("thrash")),
        "parallel path must surface loop detection: {:?}",
        results.iter().map(|r| r.model_text()).collect::<Vec<_>>()
    );
    assert!(
        thrash.peek_stop().is_some()
            || results.iter().any(|r| r.model_text().contains("loop")),
        "thrash stop should be set after parallel identical signatures"
    );
}

#[tokio::test]
async fn parallel_batch_honors_steer_skip_remaining() {
    let hits = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(CountTool {
        name: "bash".into(),
        hits: Arc::clone(&hits),
    })];
    // First pred check is false (allow first tool); later checks true (skip).
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    let pred = move || {
        let i = n2.fetch_add(1, Ordering::SeqCst);
        i >= 1
    };
    let calls: Vec<_> = (0..3)
        .map(|i| ToolCallData {
            id: format!("s{i}"),
            name: "bash".into(),
            arguments: json!({"command": "x"}),
        })
        .collect();
    let emit: Emit = Arc::new(|_| {});
    let results = execute_tool_calls_for_test(
        calls,
        &tools,
        &Hooks::default(),
        CancellationToken::new(),
        &emit,
        false, // parallel
        None,
        Some(&pred),
    )
    .await;
    assert_eq!(results.len(), 3);
    assert!(
        results[1].model_text().contains("Skipped")
            || results[2].model_text().contains("Skipped"),
        "parallel path must skip remaining on steer: {:?}",
        results.iter().map(|r| r.model_text()).collect::<Vec<_>>()
    );
    assert!(hits.load(Ordering::SeqCst) <= 1, "at most first tool runs");
}
