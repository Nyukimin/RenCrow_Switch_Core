use super::*;
use codex_history::RolloutItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_thread_store::ReadThreadParams;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;

const THREAD: &str = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e";
const ROOT_TASK: &str = "maintenance-root";
const CODEX_HOME_NAME: &str = "home";
const SOURCE_COPY: &str =
    "supervisor-activation-tests/deploy-native-candidate.before-stop-after-install.py";
const CANDIDATE17_SOURCE_COPY: &str = "candidate-17/deploy-native-candidate.reviewed.py";
const HISTORICAL_SOURCE_FIXTURE: &str =
    "tests/fixtures/maintenance_repair/deploy-native-candidate.before-stop-after-install.py.txt";
const CANDIDATE17_SOURCE_FIXTURE: &str =
    "tests/fixtures/maintenance_repair/deploy-native-candidate.candidate17.py.txt";
const DEPLOYMENT_STAMP: &str = "20261111T050631Z";
const PREDEPLOY_AT: &str = "2026-11-11T05:06:30.815682+00:00";
const BEFORE_AT: &str = "2026-11-11T05:02:26.703000+00:00";
const PAUSED_AT: &str = "2026-11-11T05:06:31.299000+00:00";
const CURRENT_AT: &str = "2026-11-11T05:06:32.000000+00:00";

fn completion_state(paused: bool) -> Value {
    serde_json::json!({
        "contract_revision": 1,
        "units_completed_since_review": 0,
        "plan_review_at": null,
        "plan_reviews": [],
        "paused": paused,
        "consumed_host_events": [],
        "conditions": [{
            "id": "AC-1", "revision": 1, "result": "Result", "scope": "Scope",
            "verification": "Check", "acceptance_criteria": "Pass", "status": "open",
            "evidence": [], "unmet_reason": null
        }],
        "units": [{
            "id": "unit-1", "purpose": "Complete the result", "status": "ready",
            "kind": "execution", "condition_refs": ["AC-1"], "depends_on": [],
            "evidence": [], "task_refs": [], "review_target": null,
            "review_evidence": null, "review_launch": null, "review_decision_reason": null
        }],
        "current_unit": "unit-1",
        "failed_approaches": [],
        "blocker": null,
        "next_action": "Start unit-1",
        "product_progress_at": null,
        "runtime_recovery_at": null,
        "activity_at": null
    })
}

fn active_scope() -> Value {
    serde_json::json!({
        "root_task_id": "older-root", "task_id": "unit-1", "task_revision": 1,
        "begin_call_id": "begin-1", "start_history_cursor": 0
    })
}

fn snapshot(paused: bool, active_scope: Option<Value>) -> Value {
    serde_json::json!({
        "version": 1,
        "thread_id": THREAD,
        "root_task_id": ROOT_TASK,
        "resume_generation": 0,
        "state": completion_state(paused),
        "task_checks": [],
        "active_scope": active_scope,
        "patch_attempts": [],
        "uncollected_children": []
    })
}

fn host_snapshot(value: Value) -> RolloutItem {
    super::repair_item(value)
}

fn write_row(file: &mut std::fs::File, ordinal: u64, timestamp: &str, item: &RolloutItem) {
    let mut row = serde_json::to_value(item).expect("serialize rollout item");
    let object = row.as_object_mut().expect("rollout item is an object");
    object.insert("timestamp".into(), Value::String(timestamp.into()));
    object.insert("ordinal".into(), Value::from(ordinal));
    use std::io::Write;
    writeln!(file, "{row}").expect("write rollout row");
}

fn write_metadata(file: &mut std::fs::File, thread: ThreadId, home: &Path) {
    let metadata = SessionMetaLine {
        meta: SessionMeta {
            session_id: thread.into(),
            id: thread,
            timestamp: "2026-11-11T05:00:00Z".into(),
            cwd: home.to_path_buf(),
            originator: "test".into(),
            cli_version: "test".into(),
            model_provider: Some("test-provider".into()),
            history_mode: codex_protocol::protocol::ThreadHistoryMode::Paginated,
            ..SessionMeta::default()
        },
        git: None,
    };
    write_row(
        file,
        0,
        "2026-11-11T05:00:00Z",
        &RolloutItem::SessionMeta(metadata),
    );
}

fn evidence_json(root: &Path, rollout: &Path, home: &Path) -> Vec<(String, Value, PathBuf)> {
    let candidate = root.join("candidate");
    let deployment = root.join("deployments").join(DEPLOYMENT_STAMP);
    let old_pid = i32::MAX as u64;
    let resumed_pid = (i32::MAX - 1) as u64;
    let core_hash = "e9247da50ba03e0ea910174d607acf03a90ab1ba7f2f474d2916a96dc2ca872c";
    let helper_hash = "d4b95f28f0e4f5670711730d4ca5f9adea0185783f1fcdf47a43d3d36ed53c3d";
    let result = serde_json::json!({
        "mode":"apply", "deployment_route":"native_aware_second_pair", "thread":THREAD,
        "target_pid":old_pid, "blockers":[],
        "candidate_hashes":{"rencrow-switch-core":core_hash,"rencrow-compaction":helper_hash},
        "deployment":{"backup":deployment, "old_pid":old_pid,
            "installed":[
                {"path":"/usr/local/bin/rencrow-switch-core","sha256":core_hash,"inode":1},
                {"path":"/usr/local/bin/rencrow-compaction","sha256":helper_hash,"inode":2}],
            "resumed":{"pid":resumed_pid,"turn_id":"01a1c900-4490-7990-8c54-992b72f51320",
                "binary_inode":1,"binary_sha256":core_hash,"codex_home":home,
                "model_effort_worker_preserved":true}}
    });
    let preflight = serde_json::json!({
        "mode":"dry_run", "deployment_route":"native_aware_second_pair", "thread":THREAD,
        "target_pid":old_pid, "blockers":[], "config_changes":[], "supervisor_settings_changes":[],
        "candidate_hashes":{"rencrow-switch-core":core_hash,"rencrow-compaction":helper_hash},
        "terminal_child_background_sessions":[{"terminal_status":"failed"},{"terminal_status":"completed"}]
    });
    let ticket = serde_json::json!({"owner":"root supervisor", "user_stop":false,
        "preserved_exact_marker_for_existing_deployer":true});
    let snapshot = serde_json::json!({
        "time":PREDEPLOY_AT, "thread":THREAD, "rollout":rollout, "rollout_error":null,
        "pending_children":[],
        "target_processes":[{"pid":old_pid,"codex_home":home,"observation_known":true}],
        "turn_observation":{"active":false,"tool_running":false,"compacting":false,"exec_background_sessions":[],
            "child_log_unknown":false,"child_failures":0,"child_failure_receipts":[]},
        "process_observation":{"monitored_cli_processes":[]},
        "child_receipts":{"pending":[],"unknown":[]}
    });
    let facts = serde_json::json!({
        "pid":old_pid, "native_second_pair_update":true,
        "maintenance_ticket":"exact-root-maintenance-marker",
        "activation_identity":{"pid":old_pid,"thread":THREAD,
            "supervisor_sha256":"f3c93f9112fe1e6ac5558132480ae0d195c5afbae3dfaaf03a49149b1b0c53f9",
            "installed_core_sha256":"37fa3d8c9f23c7dc74db890e0369242556ca14f2b27eaf33874800aa15dc1ea2"},
        "candidate_hashes":{"rencrow-switch-core":core_hash,"rencrow-compaction":helper_hash},
        "config_changes":[], "settings_changes":[],
        "terminal_child_background_sessions":[{"terminal_status":"failed"},{"terminal_status":"completed"}]
    });
    vec![
        (
            "deployment_result".into(),
            result,
            candidate.join("deploy-result.json"),
        ),
        (
            "deployment_preflight".into(),
            preflight,
            candidate.join("deploy-preflight-ready.json"),
        ),
        (
            "maintenance_ticket".into(),
            ticket,
            candidate.join("maintenance-ticket.json"),
        ),
        (
            "pre_deploy_snapshot".into(),
            snapshot,
            deployment.join("pre-deploy-snapshot.json"),
        ),
        (
            "pre_deploy_facts".into(),
            facts,
            deployment.join("pre-deploy-facts.json"),
        ),
    ]
}

async fn write_repair_fixture(
    source_copy: &str,
    source_fixture: &str,
) -> (TempDir, PathBuf, PathBuf, PathBuf, String, PathBuf) {
    let root = tempfile::tempdir().expect("fixture root");
    let root_path = root.path().canonicalize().expect("canonical fixture root");
    let home = root_path.join(CODEX_HOME_NAME);
    let rollout = home
        .join("sessions/2026/11/11")
        .join(format!("rollout-2026-11-11T05-00-00-{THREAD}.jsonl"));
    std::fs::create_dir_all(rollout.parent().expect("rollout parent"))
        .expect("create rollout directory");
    std::fs::create_dir_all(root_path.join("candidate")).expect("create candidate directory");
    std::fs::create_dir_all(root_path.join("deployments").join(DEPLOYMENT_STAMP))
        .expect("create deployment directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(source_fixture);
    let source_path = root_path.join(source_copy);
    std::fs::create_dir_all(source_path.parent().expect("source evidence parent"))
        .expect("create source evidence directory");
    std::fs::copy(source, &source_path).expect("copy historical deployer source evidence");

    let thread_id = ThreadId::from_string(THREAD).expect("valid thread id");
    let before = snapshot(false, Some(active_scope()));
    let paused = snapshot(true, Some(active_scope()));
    let current = snapshot(true, None);
    let mut file = std::fs::File::create(&rollout).expect("create rollout");
    write_metadata(&mut file, thread_id, &home);
    write_row(&mut file, 1, BEFORE_AT, &host_snapshot(before));
    write_row(
        &mut file,
        2,
        "2026-11-11T05:04:16.879000+00:00",
        &RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "maintenance-complete".into(),
            last_agent_message: Some("completed".into()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    );
    write_row(&mut file, 3, PAUSED_AT, &host_snapshot(paused));
    write_row(&mut file, 4, CURRENT_AT, &host_snapshot(current));
    file.sync_all().expect("sync initial rollout");

    let mut evidence_files = evidence_json(&root_path, &rollout, &home);
    for entry in &mut evidence_files {
        let bytes = serde_json::to_vec(&entry.1).expect("serialize evidence");
        std::fs::write(&entry.2, bytes).expect("write evidence file");
        entry.2 = entry.2.canonicalize().expect("canonical evidence path");
    }
    let source_text = std::fs::read_to_string(&source_path).expect("read source evidence");
    let source_hash = codex_history::archive_reference::content_sha256(&source_text);
    let expected_source_hash = if source_copy == SOURCE_COPY {
        "4167110a6cd6d875475e6946c6b1572bd936a375d5e65ccff1bab467c92b0a3c"
    } else {
        "6c7fb349b0e30ec25ed55d2669762c154f0ccb5a4919e9627d63f4d1e04a1889"
    };
    assert_eq!(source_hash, expected_source_hash, "source fixture hash must match its pinned copy");
    evidence_files.push((
        "deployer_source".into(),
        Value::Null,
        source_path.canonicalize().unwrap(),
    ));
    let mut evidence = Vec::new();
    for (kind, _, path) in &evidence_files {
        let text = std::fs::read_to_string(path).expect("read evidence content");
        evidence.push(serde_json::json!({
            "kind":kind, "path":path, "sha256":if kind == "deployer_source" {
                source_hash.clone()
            } else {
                codex_history::archive_reference::content_sha256(&text)
            }
        }));
    }
    let initial_items = super::super::thread_rollout_items(&rollout, &thread_id)
        .await
        .expect("read seeded rollout");
    let rows = super::canonical_rollout_rows(&rollout).expect("read canonical rows");
    let snapshots = super::completion_snapshots(&initial_items).expect("find seeded snapshots");
    let before_hash = snapshots[0].1.clone();
    let paused_hash = snapshots[1].1.clone();
    let current_hash = snapshots[2].1.clone();
    let mismatch_hashes = vec![before_hash.clone(), paused_hash.clone()];
    let tip = super::history_tip(&rollout).expect("history tip");
    let request_path = root_path.join("maintenance-request.json");
    let request = serde_json::json!({
        "version":1, "thread_id":THREAD, "root_task_id":ROOT_TASK,
        "expected_resume_generation":0,
        "expected_current_snapshot_sha256":current_hash,
        "expected_history_tip":{"ordinal":tip.ordinal,"sha256":tip.sha256},
        "shutdown_before_snapshot_sha256":before_hash,
        "shutdown_paused_snapshot_sha256":paused_hash,
        "legacy_scope_mismatch_snapshot_sha256":mismatch_hashes,
        "evidence":evidence
    });
    std::fs::write(&request_path, serde_json::to_vec(&request).unwrap())
        .expect("write maintenance request");
    assert_eq!(rows.len(), initial_items.len());
    (root, home, rollout, request_path, current_hash, source_path)
}

#[tokio::test]
async fn maintenance_repair_full_run_is_dry_then_applied_and_idempotent() {
    let (_root, home, rollout, request, _, _) =
        write_repair_fixture(SOURCE_COPY, HISTORICAL_SOURCE_FIXTURE).await;
    let before_dry_run = std::fs::read(&rollout).expect("read initial rollout");
    run(home.clone(), THREAD.into(), request.clone(), false)
        .await
        .expect("dry-run maintenance repair");
    assert_eq!(
        std::fs::read(&rollout).expect("read after dry-run"),
        before_dry_run
    );

    run(home.clone(), THREAD.into(), request.clone(), true)
        .await
        .expect("apply maintenance repair");
    let after_apply = std::fs::read(&rollout).expect("read applied rollout");
    let thread_id = ThreadId::from_string(THREAD).unwrap();
    let items = super::super::thread_rollout_items(&rollout, &thread_id)
        .await
        .expect("read applied rollout");
    let snapshots = super::completion_snapshots(&items).expect("find applied snapshot");
    let (_, repaired_hash, repaired) = snapshots.last().expect("repaired host snapshot");
    let repaired_info = codex_core::inspect_maintenance_repair_snapshot(
        &items,
        repaired_hash,
        THREAD,
        ROOT_TASK,
        false,
    )
    .expect("inspect repaired snapshot");
    assert!(!repaired_info.state.paused);
    assert_eq!(repaired["resume_generation"], 1);
    assert!(repaired["maintenance_repair_receipt"].is_object());

    run(home.clone(), THREAD.into(), request, true)
        .await
        .expect("idempotent apply retry");
    assert_eq!(
        std::fs::read(&rollout).expect("read after retry"),
        after_apply
    );

    let config = codex_core::config::ConfigBuilder::default()
        .codex_home(home.clone())
        .build()
        .await
        .expect("build isolated config");
    let state_db = codex_rollout::state_db::try_init(&config)
        .await
        .expect("initialize isolated state DB");
    let store = LocalThreadStore::new(LocalThreadStoreConfig::from_config(&config), Some(state_db));
    let metadata = store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: false,
        })
        .await
        .expect("read repaired thread metadata");
    assert_eq!(metadata.rollout_path.as_deref(), Some(rollout.as_path()));
    let checkpoint = store
        .thread_history_projection_state(thread_id)
        .await
        .expect("read paginated projection state")
        .expect("projection checkpoint exists");
    assert!(checkpoint.0 >= std::fs::metadata(&rollout).unwrap().len());
    let tip = super::history_tip(&rollout).expect("read repaired rollout tip");
    assert!(checkpoint.1 >= tip.ordinal.saturating_add(1));
}

#[tokio::test]
async fn candidate17_deployer_source_is_exactly_pinned_and_mispair_is_rejected() {
    let (root, home, rollout, request, _, source_path) =
        write_repair_fixture(CANDIDATE17_SOURCE_COPY, CANDIDATE17_SOURCE_FIXTURE).await;
    let before = std::fs::read(&rollout).expect("read initial rollout");
    run(home.clone(), THREAD.into(), request.clone(), false)
        .await
        .expect("candidate17 source pair should pass dry-run");
    assert_eq!(std::fs::read(&rollout).expect("read after candidate17 dry-run"), before);

    let mut request_value: Value =
        serde_json::from_slice(&std::fs::read(&request).expect("read request"))
            .expect("parse request");
    let historical_source = Path::new(env!("CARGO_MANIFEST_DIR")).join(HISTORICAL_SOURCE_FIXTURE);
    std::fs::copy(historical_source, &source_path).expect("replace with wrong source contents");
    let wrong_hash = codex_history::archive_reference::content_sha256(
        &std::fs::read_to_string(&source_path).expect("read wrong source contents"),
    );
    let source_entry = request_value["evidence"]
        .as_array_mut()
        .expect("evidence array")
        .iter_mut()
        .find(|entry| entry["kind"] == "deployer_source")
        .expect("deployer source entry");
    source_entry["sha256"] = Value::String(wrong_hash);
    std::fs::write(&request, serde_json::to_vec(&request_value).unwrap())
        .expect("write mispaired request");

    let error = run(home.clone(), THREAD.into(), request.clone(), false)
        .await
        .expect_err("candidate17 path must reject historical source hash");
    assert!(error.to_string().contains("reviewed deployer source path/hash pair"));
    assert_eq!(std::fs::read(&rollout).expect("read after rejected source"), before);

    let reviewed_source = Path::new(env!("CARGO_MANIFEST_DIR")).join(CANDIDATE17_SOURCE_FIXTURE);
    let unreviewed_path = root.path().join("candidate-17/deployer-copy.py");
    std::fs::copy(reviewed_source, &unreviewed_path).expect("copy reviewed bytes to unreviewed path");
    let reviewed_hash = codex_history::archive_reference::content_sha256(
        &std::fs::read_to_string(&unreviewed_path).expect("read reviewed bytes"),
    );
    let source_entry = request_value["evidence"]
        .as_array_mut()
        .expect("evidence array")
        .iter_mut()
        .find(|entry| entry["kind"] == "deployer_source")
        .expect("deployer source entry");
    source_entry["path"] = Value::String(unreviewed_path.to_string_lossy().into_owned());
    source_entry["sha256"] = Value::String(reviewed_hash);
    std::fs::write(&request, serde_json::to_vec(&request_value).unwrap())
        .expect("write unreviewed path request");

    let error = run(home, THREAD.into(), request, false)
        .await
        .expect_err("reviewed source bytes must reject an unreviewed path");
    assert!(error.to_string().contains("exact reviewed source copy"));
    assert_eq!(
        std::fs::read(&rollout).expect("read after rejected unreviewed path"),
        before
    );
}
