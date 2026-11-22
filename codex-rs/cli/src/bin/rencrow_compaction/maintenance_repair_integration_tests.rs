// Modified by RenCrow Switch Core, 2026-11-21: authorized resume and profile regressions.
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

fn write_env_only_base_and_profile_mcp_config(home: &Path) -> codex_utils_cli::ProfileV2Name {
    std::fs::create_dir_all(home).expect("create isolated CODEX_HOME");
    std::fs::write(
        home.join("config.toml"),
        r#"[mcp_servers.rencrow_advisor.env]
RENCROW_ADVISOR_EXECUTOR_POLICY_FILE = "/tmp/fixture-policy.json"
"#,
    )
    .expect("write base env-only MCP fragment");
    std::fs::write(
        home.join("rencrow.config.toml"),
        r#"[mcp_servers.rencrow_advisor]
command = "fixture-advisor"
args = []
"#,
    )
    .expect("write external profile MCP transport");
    "rencrow".parse().expect("valid runtime profile name")
}

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
    assert_eq!(
        source_hash, expected_source_hash,
        "source fixture hash must match its pinned copy"
    );
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

async fn write_authorized_resume_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf, String) {
    let root = tempfile::tempdir().expect("fixture root");
    let canonical_root = root.path().canonicalize().expect("canonical fixture root");
    let home = canonical_root.join(CODEX_HOME_NAME);
    let rollout = home
        .join("sessions/2026/11/11")
        .join(format!("rollout-2026-11-11T05-00-00-{THREAD}.jsonl"));
    std::fs::create_dir_all(rollout.parent().expect("rollout parent"))
        .expect("create rollout directory");
    let thread_id = ThreadId::from_string(THREAD).expect("valid thread id");
    let paused = snapshot(true, None);
    let mut file = std::fs::File::create(&rollout).expect("create rollout");
    write_metadata(&mut file, thread_id, &home);
    write_row(&mut file, 1, CURRENT_AT, &host_snapshot(paused.clone()));
    file.sync_all().expect("sync initial rollout");
    let items = super::super::thread_rollout_items(&rollout, &thread_id)
        .await
        .expect("read seeded rollout");
    let current_hash = super::completion_snapshots(&items).expect("find paused snapshot")[0]
        .1
        .clone();
    let tip = super::history_tip(&rollout).expect("history tip");
    let authorization_path = canonical_root.join("state.json");
    let authorization = serde_json::json!({
        "thread_id":THREAD, "root_task_id":ROOT_TASK,
        "recorded_by":"official Codex supervisor",
        "authorization": {
            "issuer":"user in current official Codex conversation",
            "original_text":"作業再開、ではなく作業完了まですすめて",
            "interpretation":"Existing latest explicit continuation supersedes prior pause; no new approval is needed. This is not a native Human intake receipt."
        }
    });
    let authorization_text =
        serde_json::to_string(&authorization).expect("serialize auth evidence");
    std::fs::write(&authorization_path, &authorization_text).expect("write auth evidence");
    let authorization_path = authorization_path
        .canonicalize()
        .expect("canonical auth evidence");
    let request_path = canonical_root.join("authorized-resume-request.json");
    let request = serde_json::json!({
        "version":2, "reason":"explicit_authorized_resume", "thread_id":THREAD,
        "root_task_id":ROOT_TASK, "expected_resume_generation":0,
        "expected_current_snapshot_sha256":current_hash,
        "expected_history_tip":{"ordinal":tip.ordinal,"sha256":tip.sha256},
        "authorization":{"path":authorization_path,
            "sha256":codex_history::archive_reference::content_sha256(&authorization_text)}
    });
    std::fs::write(
        &request_path,
        serde_json::to_vec(&request).expect("serialize request"),
    )
    .expect("write authorized resume request");
    (root, home, rollout, request_path, current_hash)
}

#[tokio::test]
async fn maintenance_repair_full_run_is_dry_then_applied_and_idempotent() {
    let (_root, home, rollout, request, _, _) =
        write_repair_fixture(SOURCE_COPY, HISTORICAL_SOURCE_FIXTURE).await;
    let profile = write_env_only_base_and_profile_mcp_config(&home);
    let before_dry_run = std::fs::read(&rollout).expect("read initial rollout");
    run_with_profile(
        home.clone(),
        THREAD.into(),
        request.clone(),
        Some(profile.clone()),
        false,
    )
    .await
    .expect("dry-run maintenance repair");
    assert_eq!(
        std::fs::read(&rollout).expect("read after dry-run"),
        before_dry_run
    );

    run_with_profile(
        home.clone(),
        THREAD.into(),
        request.clone(),
        Some(profile.clone()),
        true,
    )
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

    run_with_profile(
        home.clone(),
        THREAD.into(),
        request,
        Some(profile.clone()),
        true,
    )
    .await
    .expect("idempotent apply retry");
    assert_eq!(
        std::fs::read(&rollout).expect("read after retry"),
        after_apply
    );

    let config = super::offline_store_config(&home, Some(&profile))
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
async fn explicit_authorized_resume_is_dry_then_applied_and_idempotent() {
    let (_root, home, rollout, request, _) = write_authorized_resume_fixture().await;
    let profile = write_env_only_base_and_profile_mcp_config(&home);
    let before = std::fs::read(&rollout).expect("read initial rollout");
    run_with_profile(
        home.clone(),
        THREAD.into(),
        request.clone(),
        Some(profile.clone()),
        false,
    )
    .await
    .expect("dry-run explicit authorized resume");
    assert_eq!(std::fs::read(&rollout).expect("read after dry-run"), before);
    run_with_profile(
        home.clone(),
        THREAD.into(),
        request.clone(),
        Some(profile.clone()),
        true,
    )
    .await
    .expect("apply explicit authorized resume");
    let after_apply = std::fs::read(&rollout).expect("read applied rollout");
    let thread_id = ThreadId::from_string(THREAD).unwrap();
    let items = super::super::thread_rollout_items(&rollout, &thread_id)
        .await
        .expect("read applied rollout");
    let snapshots = super::completion_snapshots(&items).expect("find applied snapshot");
    let (_, _, resumed) = snapshots.last().expect("resumed host snapshot");
    assert_eq!(resumed["state"]["paused"], false);
    assert_eq!(resumed["resume_generation"], 1);
    assert_eq!(
        resumed["state"]["conditions"],
        snapshot(true, None)["state"]["conditions"]
    );
    assert_eq!(resumed["active_scope"], Value::Null);
    let receipt = &resumed["maintenance_repair_receipt"];
    assert_eq!(receipt["version"], 2);
    assert_eq!(receipt["reason"], "explicit_authorized_resume");
    assert!(receipt.get("shutdown_before_snapshot_sha256").is_none());
    assert!(
        receipt
            .get("legacy_scope_mismatch_snapshot_sha256")
            .is_none()
    );
    run_with_profile(home, THREAD.into(), request, Some(profile), true)
        .await
        .expect("idempotent resume retry");
    assert_eq!(
        std::fs::read(&rollout).expect("read after retry"),
        after_apply
    );
}

#[tokio::test]
async fn explicit_authorized_resume_rejects_missing_profile_without_modifying_history() {
    let (_root, home, rollout, request, _) = write_authorized_resume_fixture().await;
    write_env_only_base_and_profile_mcp_config(&home);
    let before = std::fs::read(&rollout).expect("read initial rollout");

    let no_profile = run_with_profile(home.clone(), THREAD.into(), request.clone(), None, false)
        .await
        .expect_err("env-only base MCP config must fail without its runtime profile");
    let no_profile_chain = format!("{no_profile:#}");
    assert!(no_profile_chain.contains("from base config"));
    assert!(no_profile_chain.contains("invalid transport"));
    assert_eq!(
        std::fs::read(&rollout).expect("read rollout after base config rejection"),
        before
    );

    let missing_profile: codex_utils_cli::ProfileV2Name =
        "missing".parse().expect("valid profile name");
    let missing_profile_error = run_with_profile(
        home.clone(),
        THREAD.into(),
        request,
        Some(missing_profile),
        false,
    )
    .await
    .expect_err("an unavailable explicit profile must fail closed");
    let missing_profile_chain = format!("{missing_profile_error:#}");
    assert!(missing_profile_chain.contains("profile `missing`"));
    assert!(missing_profile_chain.contains("missing.config.toml"));
    assert_eq!(
        std::fs::read(&rollout).expect("read rollout after profile rejection"),
        before
    );
}

#[tokio::test]
async fn maintenance_repair_without_profile_is_dry_then_applied_and_idempotent() {
    let (_root, home, rollout, request, _, _) =
        write_repair_fixture(SOURCE_COPY, HISTORICAL_SOURCE_FIXTURE).await;
    let before_dry_run = std::fs::read(&rollout).expect("read initial rollout");

    run(home.clone(), THREAD.into(), request.clone(), false)
        .await
        .expect("dry-run maintenance repair without profile");
    assert_eq!(
        std::fs::read(&rollout).expect("read after dry-run"),
        before_dry_run
    );

    run(home.clone(), THREAD.into(), request.clone(), true)
        .await
        .expect("apply maintenance repair without profile");
    let after_apply = std::fs::read(&rollout).expect("read applied rollout");
    assert_ne!(after_apply, before_dry_run);

    run(home, THREAD.into(), request, true)
        .await
        .expect("idempotent maintenance repair without profile");
    assert_eq!(
        std::fs::read(&rollout).expect("read after retry"),
        after_apply
    );
}

#[tokio::test]
async fn explicit_authorized_resume_without_profile_is_dry_then_applied_and_idempotent() {
    let (_root, home, rollout, request, _) = write_authorized_resume_fixture().await;
    let before_dry_run = std::fs::read(&rollout).expect("read initial rollout");

    run(home.clone(), THREAD.into(), request.clone(), false)
        .await
        .expect("dry-run explicit authorized resume without profile");
    assert_eq!(
        std::fs::read(&rollout).expect("read after dry-run"),
        before_dry_run
    );

    run(home.clone(), THREAD.into(), request.clone(), true)
        .await
        .expect("apply explicit authorized resume without profile");
    let after_apply = std::fs::read(&rollout).expect("read applied rollout");
    assert_ne!(after_apply, before_dry_run);

    run(home, THREAD.into(), request, true)
        .await
        .expect("idempotent explicit authorized resume without profile");
    assert_eq!(
        std::fs::read(&rollout).expect("read after retry"),
        after_apply
    );
}

#[tokio::test]
async fn explicit_authorized_resume_rejects_stale_or_unbound_requests() {
    let (_root, home, rollout, request_path, _) = write_authorized_resume_fixture().await;
    let initial_rollout = std::fs::read(&rollout).expect("read initial rollout");
    let original: Value =
        serde_json::from_slice(&std::fs::read(&request_path).expect("read request"))
            .expect("parse request");
    let mut cases = Vec::new();
    let mut value = original.clone();
    value["thread_id"] = Value::String("01a1c0b9-582b-71c0-a6d6-552f2d581a8f".into());
    cases.push(("wrong thread", value));
    let mut value = original.clone();
    value["root_task_id"] = Value::String("another-root".into());
    cases.push(("wrong root", value));
    let mut value = original.clone();
    value["expected_current_snapshot_sha256"] = Value::String("1".repeat(64));
    cases.push(("wrong snapshot", value));
    let mut value = original.clone();
    value["expected_history_tip"]["sha256"] = Value::String("2".repeat(64));
    cases.push(("wrong history tip", value));
    let mut value = original.clone();
    value["expected_resume_generation"] = Value::from(4);
    cases.push(("wrong generation", value));
    let mut value = original.clone();
    value["authorization"]["sha256"] = Value::String("3".repeat(64));
    cases.push(("wrong authorization hash", value));
    let mut value = original.clone();
    value["authorization"]["path"] = Value::String(
        home.join("missing-authorization.json")
            .to_string_lossy()
            .into_owned(),
    );
    cases.push(("missing authorization", value));
    for (label, request) in cases {
        std::fs::write(&request_path, serde_json::to_vec(&request).unwrap())
            .expect("write changed request");
        assert!(
            run(home.clone(), THREAD.into(), request_path.clone(), false)
                .await
                .is_err(),
            "{label} must be rejected"
        );
        assert_eq!(
            std::fs::read(&rollout).expect("read unchanged rollout"),
            initial_rollout,
            "{label} must not modify canonical history"
        );
    }
}

#[tokio::test]
async fn explicit_authorized_resume_rejects_an_active_live_writer() {
    let (_root, home, rollout, request, _) = write_authorized_resume_fixture().await;
    let thread_id = ThreadId::from_string(THREAD).unwrap();
    let initial_items = super::super::thread_rollout_items(&rollout, &thread_id)
        .await
        .expect("load actual canonical history before resuming writer");
    let config = codex_core::config::ConfigBuilder::default()
        .codex_home(home.clone())
        .build()
        .await
        .expect("build isolated config");
    let state_db = codex_rollout::state_db::try_init(&config)
        .await
        .expect("initialize isolated state DB");
    let store = Arc::new(LocalThreadStore::new(
        LocalThreadStoreConfig::from_config(&config),
        Some(state_db),
    ));
    let metadata = store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: false,
        })
        .await
        .expect("read thread metadata");
    let persistence: Arc<dyn ThreadStore> = store;
    let _active_writer = LiveThread::resume(
        persistence,
        metadata.history_mode,
        ResumeThreadParams {
            thread_id,
            rollout_path: Some(rollout.clone()),
            history: Some(std::sync::Arc::new(initial_items)),
            include_archived: true,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(metadata.cwd),
                model_provider: metadata.model_provider,
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await
    .expect("hold existing writer lock");
    let before_attempt = std::fs::read(&rollout).expect("read locked canonical rollout");
    let error = run(home, THREAD.into(), request, true)
        .await
        .expect_err("second writer must be rejected");
    let raw_error = format!("{error:#}");
    assert_eq!(
        raw_error,
        format!("thread-store conflict: thread {THREAD} already has an active writer"),
        "the second store must be rejected by the shared writer lock"
    );
    assert_eq!(
        std::fs::read(&rollout).expect("read unchanged locked canonical rollout"),
        before_attempt,
        "a rejected second writer must not modify canonical history"
    );
}

#[tokio::test]
async fn explicit_authorized_resume_rejects_a_stop_after_authorization_tip() {
    let (_root, home, rollout, request, _) = write_authorized_resume_fixture().await;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .expect("open rollout for newer stop");
    write_row(
        &mut file,
        2,
        "2026-11-11T05:06:33.000000+00:00",
        &RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some("stop-after-grant".into()),
                reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        )),
    );
    file.sync_all().expect("sync newer stop");
    let after_stop = std::fs::read(&rollout).expect("read rollout with newer stop");
    let error = run(home, THREAD.into(), request, false)
        .await
        .expect_err("newer stop must invalidate the authorization tip");
    assert!(error.to_string().contains("history tip changed"));
    assert_eq!(
        std::fs::read(&rollout).expect("read unchanged stopped rollout"),
        after_stop
    );
}

#[tokio::test]
async fn candidate17_deployer_source_is_exactly_pinned_and_mispair_is_rejected() {
    let (root, home, rollout, request, _, source_path) =
        write_repair_fixture(CANDIDATE17_SOURCE_COPY, CANDIDATE17_SOURCE_FIXTURE).await;
    let before = std::fs::read(&rollout).expect("read initial rollout");
    run(home.clone(), THREAD.into(), request.clone(), false)
        .await
        .expect("candidate17 source pair should pass dry-run");
    assert_eq!(
        std::fs::read(&rollout).expect("read after candidate17 dry-run"),
        before
    );

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
    assert!(
        error
            .to_string()
            .contains("reviewed deployer source path/hash pair")
    );
    assert_eq!(
        std::fs::read(&rollout).expect("read after rejected source"),
        before
    );

    let reviewed_source = Path::new(env!("CARGO_MANIFEST_DIR")).join(CANDIDATE17_SOURCE_FIXTURE);
    let unreviewed_path = root.path().join("candidate-17/deployer-copy.py");
    std::fs::copy(reviewed_source, &unreviewed_path)
        .expect("copy reviewed bytes to unreviewed path");
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
