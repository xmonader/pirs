use std::sync::{Arc, Mutex};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_fans_out_caches_and_merges() {
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let calls2 = Arc::clone(&calls);
    let mut host = pirs_rhai::ExtensionHost::new();
    host.set_subagent_runner(Arc::new(move |task: String, _| {
        calls2.lock().unwrap().push(task.clone());
        if task.contains("Merge these") {
            Ok("MERGED: two issues".to_string())
        } else if task.contains("alpha") {
            Ok("bug in alpha".to_string())
        } else {
            Ok("CLEAN".to_string())
        }
    }));
    let path = format!(
        "{}/../../examples/extensions/workflow.rhai",
        env!("CARGO_MANIFEST_DIR")
    );
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    let host = Arc::new(host);

    let tmp = std::env::temp_dir().join(format!("pirs-wf-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let f1 = tmp.join("alpha.rs");
    let f2 = tmp.join("beta.rs");
    std::fs::write(&f1, "fn alpha() {}").unwrap();
    std::fs::write(&f2, "fn beta() {}").unwrap();
    let home = tmp.join("home");
    std::env::set_var("HOME", &home);

    let tool = host.tools().into_iter().find(|t| t.name() == "workflow").unwrap();
    let args = serde_json::json!({"files": [f1.to_string_lossy(), f2.to_string_lossy()], "model": ""});
    let out1 = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: args.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    let text1 = out1.content[0].as_text().unwrap();
    assert!(text1.contains("MERGED: two issues"), "{text1}");

    let first_call_count = calls.lock().unwrap().len();
    assert_eq!(first_call_count, 3, "2 file reviews + 1 merge: {:?}", calls.lock().unwrap());

    let out2 = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t2".into(),
            args,
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert!(out2.content[0].as_text().unwrap().contains("MERGED"));
    assert_eq!(
        calls.lock().unwrap().len(),
        first_call_count + 1,
        "rerun must skip cached file reviews (only merge runs)"
    );
}
