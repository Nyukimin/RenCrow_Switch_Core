// Modified by RenCrow: a rejected selection must not block Work summary or skip future selection.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_selection_summarizes_work_and_retries_after_restart() -> Result<()> {
    let server = start_mock_server().await;
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = selections.clone();
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let responses = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().unwrap();
            captured.lock().unwrap().push(body.clone());
            let id = format!("response-{}", responses.fetch_add(1, Ordering::SeqCst));
            let reply = match stage(&body) {
                Stage::Selection(data) => {
                    if selected.fetch_add(1,Ordering::SeqCst)==0 {
                        "{\"operations\":[{\"action\":\"drop_superseded\",\"source\":\"unfinished".into()
                    } else {
                        json!({"operations":[{"action":"drop_superseded","source":passage_id(&data,"Use obsolete-label."),"correction":passage_id(&data,"Use current-label instead.")}]}).to_string()
                    }
                }
                Stage::Summary => "Work recorded; preserve Japanese and the current label.".into(),
                Stage::Ordinary => "Observed ordinary work. ".repeat(150),
            };
            assistant_reply(&id,&reply,ev_completed(&id))
        }).mount(&server).await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.rencrow_compaction = true;
    });
    let test = builder.build(&server).await?;
    submit_human_turn(&test, "old", "Keep Japanese. Use obsolete-label.").await?;
    submit_human_turn(&test, "new", "Use current-label instead.").await?;
    compact_without_error(&test.codex).await?;
    let path = test.codex.rollout_path().unwrap();
    let first = checkpoint_rows(&path)?;
    assert_eq!(
        checkpoint_metadata(&first[0])["selection_mode"],
        "conservative_selection"
    );
    let retained = first[0]["payload"]["replacement_history"].to_string();
    assert!(retained.contains("Keep Japanese. Use obsolete-label."));
    assert!(retained.contains("Use current-label instead."));
    assert!(retained.contains("Work recorded"));

    let provider = non_openai_model_provider(&server);
    let mut restarted = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.rencrow_compaction = true;
    });
    let resumed = restarted.restart(&server, &test).await?;
    // No new declared Human: the conservative checkpoint must still cause reselection.
    resumed
        .submit_text_turn("Continue the existing work.")
        .await?;
    compact_without_error(&resumed.codex).await?;
    resumed
        .submit_text_turn("Continue after accepted compaction.")
        .await?;
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    let rows = checkpoint_rows(&path)?;
    assert_eq!(
        checkpoint_metadata(&rows[1])["selection_mode"],
        "model_selection"
    );
    let retained = rows[1]["payload"]["replacement_history"].to_string();
    assert!(!retained.contains("Use obsolete-label."));
    assert!(retained.contains("Keep Japanese."));
    assert!(retained.contains("Use current-label instead."));
    let requests = seen.lock().unwrap();
    let next = requests.last().unwrap()["input"].to_string();
    assert!(!next.contains("Use obsolete-label."));
    assert!(next.contains("Keep Japanese."));
    assert!(next.contains("Use current-label instead."));
    assert!(next.contains("Work recorded"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(stage(request), Stage::Summary))
            .count(),
        2
    );
    Ok(())
}
