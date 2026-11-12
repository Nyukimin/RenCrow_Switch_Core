use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use chrono::DateTime;
use chrono::NaiveDateTime;
use codex_core::MaintenanceRepairEvidenceDigest;
use codex_core::MaintenanceRepairReceipt;
use codex_core::build_maintenance_repair_snapshot;
use codex_core::inspect_maintenance_repair_snapshot;
use codex_core::maintenance_snapshot_digest;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TurnAbortReason;
use codex_thread_store::LiveThread;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
#[path = "maintenance_repair_integration_tests.rs"]
mod integration_tests;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const EVIDENCE_KINDS: [&str; 6] = [
    "deployment_result",
    "deployment_preflight",
    "maintenance_ticket",
    "pre_deploy_snapshot",
    "pre_deploy_facts",
    "deployer_source",
];
const REVIEWED_DEPLOYER_SOURCES: [(&str, &str); 2] = [
    (
        "supervisor-activation-tests/deploy-native-candidate.before-stop-after-install.py",
        "4167110a6cd6d875475e6946c6b1572bd936a375d5e65ccff1bab467c92b0a3c",
    ),
    (
        "candidate-17/deploy-native-candidate.reviewed.py",
        "6c7fb349b0e30ec25ed55d2669762c154f0ccb5a4919e9627d63f4d1e04a1889",
    ),
];

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    thread_id: String,
    root_task_id: String,
    expected_resume_generation: u64,
    expected_current_snapshot_sha256: String,
    expected_history_tip: HistoryTip,
    shutdown_before_snapshot_sha256: String,
    shutdown_paused_snapshot_sha256: String,
    legacy_scope_mismatch_snapshot_sha256: Vec<String>,
    evidence: Vec<EvidenceFile>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoryTip {
    ordinal: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceFile {
    kind: String,
    path: PathBuf,
    sha256: String,
}

#[derive(Serialize)]
struct ResultRecord<'a> {
    status: &'a str,
    thread_id: &'a str,
    root_task_id: &'a str,
    request_sha256: &'a str,
    snapshot_sha256: &'a str,
    history_tip: &'a HistoryTip,
    live_session_modified: bool,
}

struct EvidenceSummary {
    receipt: Vec<MaintenanceRepairEvidenceDigest>,
    predeploy_at: chrono::DateTime<chrono::FixedOffset>,
    deployment_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
struct RepairFailure {
    phase: &'static str,
    modified: Option<bool>,
    message: String,
}

impl std::fmt::Display for RepairFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RepairFailure {}

pub(super) fn failure_details(error: &anyhow::Error) -> Option<(Option<bool>, &'static str)> {
    error
        .downcast_ref::<RepairFailure>()
        .map(|failure| (failure.modified, failure.phase))
}

pub(super) fn attach_failure_details(output: &mut Value, error: &anyhow::Error) {
    if let Some((modified, phase)) = failure_details(error) {
        output["write_phase"] = Value::String(phase.to_string());
        match modified {
            Some(true) => output["live_session_modified"] = Value::Bool(true),
            Some(false) => {}
            None => output["live_session_modified_unknown"] = Value::Bool(true),
        }
    }
}

fn repair_failure(
    error: anyhow::Error,
    modified: Option<bool>,
    phase: &'static str,
) -> anyhow::Error {
    RepairFailure {
        phase,
        modified,
        message: format!("{error:#}"),
    }
    .into()
}

fn secure_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect file {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    ensure!(
        metadata.len() <= max_bytes,
        "{} exceeds its size bound",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "{} has a different owner",
            path.display()
        );
        ensure!(
            metadata.mode() & 0o022 == 0,
            "{} is group/world writable",
            path.display()
        );
    }
    std::fs::read(path).with_context(|| format!("cannot read file {}", path.display()))
}

fn evidence_root(request: &Request) -> Result<PathBuf> {
    let mut paths = BTreeMap::new();
    for evidence in &request.evidence {
        ensure!(
            EVIDENCE_KINDS.contains(&evidence.kind.as_str()),
            "unexpected evidence kind {}",
            evidence.kind
        );
        ensure!(
            paths
                .insert(evidence.kind.as_str(), &evidence.path)
                .is_none(),
            "duplicate evidence kind"
        );
    }
    ensure!(
        paths.len() == EVIDENCE_KINDS.len(),
        "all six maintenance evidence files are required"
    );
    let ticket = paths["maintenance_ticket"].canonicalize()?;
    let candidate = ticket
        .parent()
        .context("maintenance ticket has no parent")?;
    let root = candidate
        .parent()
        .context("candidate directory has no parent")?;
    for (kind, name) in [
        ("deployment_result", "deploy-result.json"),
        ("deployment_preflight", "deploy-preflight-ready.json"),
        ("maintenance_ticket", "maintenance-ticket.json"),
    ] {
        let path = paths[kind].canonicalize()?;
        ensure!(
            path == candidate.join(name),
            "{kind} path does not match the recorded deployment evidence layout"
        );
    }
    let snapshot = paths["pre_deploy_snapshot"].canonicalize()?;
    let facts = paths["pre_deploy_facts"].canonicalize()?;
    ensure!(
        snapshot
            .file_name()
            .is_some_and(|name| name == "pre-deploy-snapshot.json"),
        "pre-deploy snapshot path is unexpected"
    );
    ensure!(
        facts
            .file_name()
            .is_some_and(|name| name == "pre-deploy-facts.json"),
        "pre-deploy facts path is unexpected"
    );
    ensure!(
        snapshot.parent() == facts.parent(),
        "pre-deploy evidence does not share one deployment directory"
    );
    let deployment_dir = snapshot
        .parent()
        .context("pre-deploy evidence has no parent")?;
    ensure!(
        deployment_dir
            .parent()
            .is_some_and(|parent| parent == root.join("deployments")),
        "pre-deploy evidence is outside the recorded deployments directory"
    );
    let source = paths["deployer_source"].canonicalize()?;
    ensure!(
        REVIEWED_DEPLOYER_SOURCES
            .iter()
            .any(|(relative_path, _)| source == root.join(relative_path)),
        "deployer source is not an exact reviewed source copy"
    );
    Ok(root.to_path_buf())
}

fn load_request(
    path: &Path,
    thread_id: &str,
    codex_home: &Path,
    rollout_path: &Path,
) -> Result<(Request, String, EvidenceSummary)> {
    ensure!(path.is_absolute(), "request path must be absolute");
    let canonical = path
        .canonicalize()
        .context("request path does not resolve")?;
    let bytes = secure_regular_file(&canonical, MAX_REQUEST_BYTES as u64)?;
    let value: Value =
        serde_json::from_slice(&bytes).context("maintenance request is invalid JSON")?;
    let request: Request =
        serde_json::from_value(value.clone()).context("maintenance request schema is invalid")?;
    ensure!(
        request.version == 1,
        "unsupported maintenance request version"
    );
    ensure!(
        request.expected_resume_generation.checked_add(1).is_some(),
        "resume generation is exhausted"
    );
    let root = evidence_root(&request)?;
    let request_sha256 = maintenance_snapshot_digest(&value).map_err(anyhow::Error::msg)?;
    let mut evidence_receipt = Vec::with_capacity(request.evidence.len());
    let mut evidence_values = BTreeMap::new();
    let mut evidence_paths = BTreeMap::new();
    for evidence in &request.evidence {
        ensure!(
            evidence.path.is_absolute(),
            "evidence path must be absolute"
        );
        let canonical_evidence = evidence
            .path
            .canonicalize()
            .context("evidence path does not resolve")?;
        ensure!(
            canonical_evidence == evidence.path,
            "evidence path must already be canonical"
        );
        let content = secure_regular_file(&canonical_evidence, 2 * 1024 * 1024)?;
        let text = std::str::from_utf8(&content).context("evidence file must be UTF-8")?;
        let actual_sha256 = codex_history::archive_reference::content_sha256(text);
        ensure!(
            actual_sha256 == evidence.sha256,
            "evidence hash mismatch for {}",
            evidence.kind
        );
        validate_evidence_content(&evidence.kind, text, &request.thread_id)?;
        if evidence.kind == "deployer_source" {
            validate_deployer_source_pair(&root, &canonical_evidence, &actual_sha256)?;
        }
        if evidence.kind != "deployer_source" {
            evidence_values.insert(evidence.kind.clone(), serde_json::from_str::<Value>(text)?);
        }
        evidence_paths.insert(evidence.kind.clone(), canonical_evidence.clone());
        evidence_receipt.push(MaintenanceRepairEvidenceDigest {
            kind: evidence.kind.clone(),
            path: canonical_evidence.to_string_lossy().into_owned(),
            sha256: actual_sha256,
        });
    }
    validate_evidence_cross_binding(
        &evidence_values,
        &evidence_paths,
        thread_id,
        codex_home,
        rollout_path,
        &root,
    )?;
    let predeploy_at = DateTime::parse_from_rfc3339(
        evidence_values["pre_deploy_snapshot"]["time"]
            .as_str()
            .context("pre-deploy timestamp is missing")?,
    )?;
    let backup_path = PathBuf::from(
        evidence_values["deployment_result"]["deployment"]["backup"]
            .as_str()
            .context("deployment backup path is missing")?,
    );
    let backup_stamp = backup_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("deployment backup timestamp is missing")?;
    let deployment_at = NaiveDateTime::parse_from_str(backup_stamp, "%Y%m%dT%H%M%SZ")?.and_utc();
    evidence_receipt.sort_by(|left, right| left.kind.cmp(&right.kind));
    Ok((
        request,
        request_sha256,
        EvidenceSummary {
            receipt: evidence_receipt,
            predeploy_at,
            deployment_at,
        },
    ))
}

fn validate_evidence_content(kind: &str, text: &str, thread_id: &str) -> Result<()> {
    if kind == "deployer_source" {
        ensure!(
            text.contains("def apply("),
            "deployer source does not contain the reviewed apply entrypoint"
        );
        return Ok(());
    }
    let value: Value =
        serde_json::from_str(text).context("maintenance evidence JSON is invalid")?;
    match kind {
        "deployment_result" => {
            ensure!(
                value["mode"] == "apply" && value["deployment_route"] == "native_aware_second_pair",
                "deployment result is not an applied native pair update"
            );
            ensure!(
                value["thread"] == thread_id
                    && value["blockers"].as_array().is_some_and(Vec::is_empty),
                "deployment result thread/blockers are invalid"
            );
            ensure!(
                value["deployment"]["resumed"].is_object(),
                "deployment result did not resume the same thread"
            );
        }
        "deployment_preflight" => {
            ensure!(
                value["mode"] == "dry_run" && value["thread"] == thread_id,
                "deployment preflight does not match the target thread"
            );
            ensure!(
                value["blockers"].as_array().is_some_and(Vec::is_empty),
                "deployment preflight has blockers"
            );
            ensure!(
                value["config_changes"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                    && value["supervisor_settings_changes"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                "deployment preflight reports config or supervisor settings changes"
            );
            ensure!(
                value["terminal_child_background_sessions"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().all(|row| matches!(
                        row["terminal_status"].as_str(),
                        Some("completed" | "failed")
                    ))),
                "deployment preflight has unresolved child/background sessions"
            );
        }
        "maintenance_ticket" => {
            ensure!(
                value["owner"] == "root supervisor" && value["user_stop"] == false,
                "maintenance ticket is not an active root maintenance ticket"
            );
            ensure!(
                value["preserved_exact_marker_for_existing_deployer"] == true,
                "maintenance ticket does not preserve the existing marker"
            );
        }
        "pre_deploy_snapshot" => {
            ensure!(
                value["thread"] == thread_id
                    && value.get("rollout_error").is_some_and(Value::is_null),
                "pre-deploy snapshot does not match the thread or has rollout errors"
            );
            ensure!(
                value["pending_children"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
                "pre-deploy snapshot has pending children"
            );
            ensure!(
                value["turn_observation"]["active"] == false,
                "pre-deploy snapshot has an active turn"
            );
            ensure!(
                value["process_observation"]["monitored_cli_processes"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
                "pre-deploy snapshot has active CLI processes"
            );
            ensure!(
                value["turn_observation"]["tool_running"] == false,
                "pre-deploy snapshot has a running tool"
            );
            ensure!(
                value["turn_observation"]["compacting"] == false
                    && value["turn_observation"]["exec_background_sessions"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                "pre-deploy snapshot has compaction or background sessions in progress"
            );
            ensure!(
                value["child_receipts"]["pending"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                    && value["child_receipts"]["unknown"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                "pre-deploy snapshot has unresolved child receipts"
            );
        }
        "pre_deploy_facts" => {
            ensure!(
                value["native_second_pair_update"] == true,
                "pre-deploy facts are not for a native pair update"
            );
            ensure!(
                value["activation_identity"]["thread"] == thread_id,
                "pre-deploy activation belongs to another thread"
            );
            ensure!(
                value["maintenance_ticket"] == "exact-root-maintenance-marker",
                "pre-deploy facts lack the exact maintenance marker"
            );
            ensure!(
                value["config_changes"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                    && value["settings_changes"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                "pre-deploy facts report config or supervisor settings changes"
            );
            ensure!(
                value["terminal_child_background_sessions"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().all(|row| matches!(
                        row["terminal_status"].as_str(),
                        Some("completed" | "failed")
                    ))),
                "pre-deploy facts contain unresolved child/background sessions"
            );
        }
        _ => anyhow::bail!("unexpected evidence kind"),
    }
    Ok(())
}

fn validate_deployer_source_pair(root: &Path, source: &Path, sha256: &str) -> Result<()> {
    ensure!(
        REVIEWED_DEPLOYER_SOURCES.iter().any(|(relative_path, expected_sha256)| {
            source == root.join(relative_path) && sha256 == *expected_sha256
        }),
        "deployer source does not match a reviewed deployer source path/hash pair"
    );
    Ok(())
}

fn validate_evidence_cross_binding(
    evidence: &BTreeMap<String, Value>,
    paths: &BTreeMap<String, PathBuf>,
    thread_id: &str,
    codex_home: &Path,
    rollout_path: &Path,
    evidence_root: &Path,
) -> Result<()> {
    let result = &evidence["deployment_result"];
    let preflight = &evidence["deployment_preflight"];
    let snapshot = &evidence["pre_deploy_snapshot"];
    let facts = &evidence["pre_deploy_facts"];
    let resumed = &result["deployment"]["resumed"];
    let pid = facts["activation_identity"]["pid"]
        .as_u64()
        .context("activation PID is missing")?;
    ensure!(
        facts["pid"].as_u64() == Some(pid)
            && result["target_pid"].as_u64() == Some(pid)
            && result["deployment"]["old_pid"].as_u64() == Some(pid),
        "deployment PID evidence does not agree"
    );
    ensure!(
        result["thread"] == thread_id
            && preflight["thread"] == thread_id
            && snapshot["thread"] == thread_id
            && facts["activation_identity"]["thread"] == thread_id,
        "maintenance evidence mixes thread identities"
    );
    ensure!(
        resumed["codex_home"].as_str() == codex_home.to_str(),
        "deployed process was resumed from another CODEX_HOME"
    );
    ensure!(
        resumed["pid"]
            .as_u64()
            .is_some_and(|resumed_pid| resumed_pid != pid)
            && resumed["model_effort_worker_preserved"] == true,
        "deployment did not resume a distinct process with its recorded worker settings"
    );
    ensure!(
        snapshot["rollout"].as_str().map(Path::new) == Some(rollout_path),
        "pre-deploy snapshot references another rollout"
    );
    ensure!(
        result["candidate_hashes"] == preflight["candidate_hashes"]
            && result["candidate_hashes"] == facts["candidate_hashes"],
        "deployment candidate hashes do not agree"
    );
    let backup = PathBuf::from(
        result["deployment"]["backup"]
            .as_str()
            .context("deployment backup path is missing")?,
    )
    .canonicalize()?;
    let snapshot_path = paths
        .get("pre_deploy_snapshot")
        .context("pre-deploy snapshot path is missing")?;
    let facts_path = paths
        .get("pre_deploy_facts")
        .context("pre-deploy facts path is missing")?;
    ensure!(
        snapshot_path.parent() == Some(backup.as_path())
            && facts_path.parent() == Some(backup.as_path()),
        "deployment backup path {} does not match pre-deploy evidence directories {} and {}",
        backup.display(),
        snapshot_path.display(),
        facts_path.display()
    );
    let deployments_dir = evidence_root.join("deployments");
    ensure!(
        backup.parent() == Some(deployments_dir.as_path()),
        "deployment backup is outside the evidence root"
    );
    let target = snapshot["target_processes"]
        .as_array()
        .context("pre-deploy target process evidence is missing")?;
    ensure!(
        target.len() == 1
            && target[0]["pid"].as_u64() == Some(pid)
            && target[0]["codex_home"].as_str() == codex_home.to_str()
            && target[0]["observation_known"] == true,
        "pre-deploy target process does not match the recorded old activation"
    );
    let installed = result["deployment"]["installed"]
        .as_array()
        .context("installed pair hashes are missing")?;
    let core_hash = installed
        .iter()
        .find(|entry| {
            entry["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("rencrow-switch-core"))
        })
        .and_then(|entry| entry["sha256"].as_str())
        .context("installed core hash is missing")?;
    let helper_hash = installed
        .iter()
        .find(|entry| {
            entry["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("rencrow-compaction"))
        })
        .and_then(|entry| entry["sha256"].as_str())
        .context("installed helper hash is missing")?;
    ensure!(
        resumed["binary_sha256"].as_str() == Some(core_hash)
            && result["candidate_hashes"]["rencrow-switch-core"].as_str() == Some(core_hash)
            && result["candidate_hashes"]["rencrow-compaction"].as_str() == Some(helper_hash),
        "new candidate, installed pair, and resumed core hashes do not agree"
    );
    ensure!(
        preflight["target_pid"].as_u64() == Some(pid),
        "preflight target PID does not match deployment evidence"
    );
    ensure!(
        facts["native_second_pair_update"] == true,
        "pre-deploy facts do not confirm a native pair update"
    );
    ensure!(
        !process_is_alive(pid)?,
        "the old activated PID is still running"
    );
    let resumed_pid = resumed["pid"].as_u64().context("resumed PID is missing")?;
    ensure!(
        !process_is_alive(resumed_pid)?,
        "the deployed thread PID is still running"
    );
    Ok(())
}

fn process_is_alive(pid: u64) -> Result<bool> {
    let pid = libc::pid_t::try_from(pid).context("PID is outside the platform range")?;
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    Ok(std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

fn completion_snapshots(items: &[RolloutItem]) -> Result<Vec<(usize, String, Value)>> {
    let mut snapshots = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let RolloutItem::ResponseItem(envelope) = item else {
            continue;
        };
        let Some(metadata) = envelope.metadata.as_ref() else {
            continue;
        };
        let Some(value) = metadata.rencrow_completion.as_ref() else {
            continue;
        };
        let is_host = matches!(&envelope.item, ResponseItem::Message {
            role, internal_chat_message_metadata_passthrough: Some(kind), ..
        } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
            kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
        })) && !metadata.client_authored;
        if is_host {
            snapshots.push((
                index,
                maintenance_snapshot_digest(value).map_err(anyhow::Error::msg)?,
                value.clone(),
            ));
        }
    }
    Ok(snapshots)
}

fn history_tip(rollout_path: &Path) -> Result<HistoryTip> {
    let rows = canonical_rollout_rows(rollout_path)?;
    let value = rows.last().context("canonical history is empty")?;
    let ordinal = value["ordinal"]
        .as_u64()
        .context("last canonical rollout record has no ordinal")?;
    Ok(HistoryTip {
        ordinal,
        sha256: maintenance_snapshot_digest(value).map_err(anyhow::Error::msg)?,
    })
}

async fn verify_projection_checkpoint(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> Result<()> {
    let (projected_offset, projected_next_ordinal) = store
        .thread_history_projection_state(thread_id)
        .await?
        .context("paginated thread projection checkpoint is unavailable")?;
    let appended_tip = history_tip(rollout_path)?;
    let canonical_size = std::fs::metadata(rollout_path)?.len();
    ensure!(
        projected_offset >= canonical_size,
        "SQLite projection has not reached canonical rollout EOF"
    );
    ensure!(
        projected_next_ordinal >= appended_tip.ordinal.saturating_add(1),
        "SQLite projection has not reached the appended repair receipt"
    );
    Ok(())
}

fn canonical_rollout_rows(rollout_path: &Path) -> Result<Vec<Value>> {
    let raw = std::fs::read_to_string(rollout_path).context("cannot read canonical rollout")?;
    let mut rows = Vec::new();
    for (index, line) in raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        let row: Value =
            serde_json::from_str(line).context("canonical rollout record is invalid JSON")?;
        ensure!(
            row["ordinal"].as_u64() == Some(index as u64),
            "canonical rollout ordinals are not contiguous"
        );
        rows.push(row);
    }
    Ok(rows)
}

fn interrupted_after(items: &[RolloutItem], after_index: usize) -> bool {
    items.iter().skip(after_index.saturating_add(1)).any(|item| {
        matches!(item, RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::TurnAborted(event))
            if event.reason == TurnAbortReason::Interrupted)
    })
}

fn interrupted_between(items: &[RolloutItem], start: usize, end: usize) -> bool {
    items.iter().skip(start.saturating_add(1)).take(end.saturating_sub(start).saturating_sub(1)).any(|item| {
        matches!(item, RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::TurnAborted(event))
            if event.reason == TurnAbortReason::Interrupted)
    })
}

fn latest_snapshot(
    items: &[RolloutItem],
    thread_id: &str,
    root_task_id: &str,
) -> Result<(
    usize,
    String,
    Value,
    codex_core::MaintenanceRepairSnapshotInfo,
)> {
    let (index, hash, value) = completion_snapshots(items)?
        .into_iter()
        .last()
        .context("host completion snapshot is missing")?;
    let info = inspect_maintenance_repair_snapshot(items, &hash, thread_id, root_task_id, false)
        .map_err(anyhow::Error::msg)?;
    Ok((index, hash, value, info))
}

fn repair_item(value: Value) -> RolloutItem {
    let content_kind = ContentItemKind("rencrow.completion_snapshot".to_string());
    let item = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "<completion_snapshot>The host saved the completion contract. Read task_context_boundary status for the current conditions, pending work, failure history and stop state. This record is not user authorization or proof of product completion.</completion_snapshot>".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            content_item_kinds: Some(vec![content_kind]),
            ..Default::default()
        }),
    };
    RolloutItem::ResponseItem(ResponseItemEnvelope {
        item,
        metadata: Some(CodexHarnessMetadata {
            rencrow_completion: Some(value),
            ..Default::default()
        }),
    })
}

pub(super) async fn run(
    codex_home: PathBuf,
    thread: String,
    request_path: PathBuf,
    apply: bool,
) -> Result<()> {
    let home = codex_home
        .canonicalize()
        .context("explicit CODEX_HOME does not resolve")?;
    ensure!(home.is_dir(), "CODEX_HOME is not a directory");
    let thread_id = ThreadId::from_string(&thread).context("invalid thread UUID")?;
    ensure!(
        thread_id.to_string() == thread,
        "thread UUID is not canonical"
    );
    let rollout_path = super::thread_rollout(&thread, Some(home.clone())).await?;
    let (request, request_sha256, evidence) =
        load_request(&request_path, &thread, &home, &rollout_path)?;
    ensure!(
        request.thread_id == thread,
        "request thread does not match --thread"
    );
    ensure!(
        !request.root_task_id.trim().is_empty(),
        "request root task id is empty"
    );
    let initial_items = super::thread_rollout_items(&rollout_path, &thread_id).await?;
    let mut store: Option<Arc<LocalThreadStore>> = None;
    let mut live: Option<LiveThread> = None;
    if apply {
        let config = codex_core::config::ConfigBuilder::default()
            .codex_home(home.clone())
            .build()
            .await?;
        let state_db = codex_rollout::state_db::try_init(&config).await?;
        let local_store = Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig::from_config(&config),
            Some(state_db),
        ));
        let metadata = local_store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        ensure!(
            metadata.parent_thread_id.is_none() && metadata.forked_from_id.is_none(),
            "repair is limited to the owning root thread, not a child or fork"
        );
        ensure!(
            metadata.rollout_path.as_deref() == Some(rollout_path.as_path()),
            "thread store resolved a different canonical rollout"
        );
        let persistence: Arc<dyn ThreadStore> = local_store.clone();
        let writer = LiveThread::resume(
            persistence,
            metadata.history_mode,
            ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path.clone()),
                history: Some(Arc::new(initial_items.clone())),
                include_archived: true,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(metadata.cwd.clone()),
                    model_provider: metadata.model_provider.clone(),
                    memory_mode: ThreadMemoryMode::Disabled,
                },
            },
        )
        .await?;
        store = Some(local_store);
        live = Some(writer);
    }

    let mut write_started = false;
    let mut write_confirmed = false;
    let mut existing_receipt_retry = false;
    let result = async {
        let items = if apply {
            super::thread_rollout_items(&rollout_path, &thread_id).await?
        } else {
            initial_items
        };
        let rows = canonical_rollout_rows(&rollout_path)?;
        ensure!(
            rows.len() == items.len(),
            "canonical row metadata and rollout items do not align"
        );
        let tip = history_tip(&rollout_path)?;
        let (current_index, current_hash, current_value, current_info) =
            latest_snapshot(&items, &thread, &request.root_task_id)?;

        if current_value
            .get("maintenance_repair_receipt")
            .and_then(|receipt| receipt.get("request_sha256"))
            .and_then(Value::as_str)
            == Some(request_sha256.as_str())
        {
            let receipt: MaintenanceRepairReceipt =
                serde_json::from_value(current_value["maintenance_repair_receipt"].clone())
                    .context("latest host repair receipt is malformed")?;
            ensure!(
                receipt.thread_id == thread && receipt.root_task_id == request.root_task_id,
                "latest repair receipt is bound to another thread or root"
            );
            ensure!(
                receipt.resume_generation_before == request.expected_resume_generation
                    && receipt.resume_generation_after == current_info.resume_generation
                    && current_info.resume_generation
                        == request.expected_resume_generation.saturating_add(1),
                "latest repair receipt generation does not match this request"
            );
            ensure!(
                !current_info.state.paused,
                "latest repaired snapshot is paused again"
            );
            ensure!(
                receipt.legacy_scope_mismatch_snapshot_sha256
                    == request.legacy_scope_mismatch_snapshot_sha256,
                "latest repair receipt mismatch set differs from this request"
            );
            ensure!(
                !interrupted_after(&items, current_index),
                "an explicit interruption followed the prior repair receipt"
            );
            if apply {
                let writer = live.as_ref().context("apply writer is missing")?;
                write_confirmed = true;
                existing_receipt_retry = true;
                writer.flush().await?;
                verify_projection_checkpoint(
                    store
                        .as_ref()
                        .context("apply projection store is missing")?,
                    thread_id,
                    &rollout_path,
                )
                .await?;
                return Ok(("already_applied", current_hash, tip));
            }
            return Ok(("canonical_receipt_present", current_hash, tip));
        }

        ensure!(
            current_hash == request.expected_current_snapshot_sha256,
            "latest completion snapshot changed since request creation"
        );
        ensure!(
            current_info.resume_generation == request.expected_resume_generation,
            "completion resume generation changed since request creation"
        );
        ensure!(
            current_info.state.paused,
            "latest completion snapshot is not paused"
        );
        ensure!(
            current_info.state.blocker.is_none(),
            "a blocker prevents maintenance pause repair"
        );
        ensure!(
            tip.ordinal == request.expected_history_tip.ordinal
                && tip.sha256 == request.expected_history_tip.sha256,
            "canonical history tip changed since request creation"
        );
        ensure!(
            !interrupted_after(&items, current_index),
            "an explicit interruption followed the current paused snapshot"
        );

        let snapshots = completion_snapshots(&items)?;
        let find_snapshot = |hash: &str| snapshots.iter().find(|(_, digest, _)| digest == hash);
        let before = find_snapshot(&request.shutdown_before_snapshot_sha256)
            .context("shutdown-before snapshot was not found in canonical host history")?;
        let paused = find_snapshot(&request.shutdown_paused_snapshot_sha256)
            .context("shutdown-paused snapshot was not found in canonical host history")?;
        ensure!(
            before.0 < paused.0 && paused.0 <= current_index,
            "shutdown pause pair order is invalid"
        );
        ensure!(
            paused.0 > 0 && rows[paused.0 - 1]["payload"]["type"] == "task_complete",
            "shutdown pause snapshot was not immediately preceded by a task_complete record"
        );
        let before_at = DateTime::parse_from_rfc3339(
            rows[before.0]["timestamp"]
                .as_str()
                .context("shutdown-before timestamp is missing")?,
        )?;
        let paused_at = DateTime::parse_from_rfc3339(
            rows[paused.0]["timestamp"]
                .as_str()
                .context("shutdown-paused timestamp is missing")?,
        )?;
        ensure!(
            before_at <= evidence.predeploy_at && evidence.predeploy_at < paused_at,
            "deployment preflight time is outside the shutdown pause transition"
        );
        ensure!(
            paused_at.timestamp() == evidence.deployment_at.timestamp(),
            "shutdown pause time does not match the deployment backup timestamp"
        );
        ensure!(
            !snapshots
                .iter()
                .any(|(index, _, value)| *index < paused.0 && value["state"]["paused"] == true),
            "shutdown paused snapshot was not the first paused host snapshot"
        );
        ensure!(
            !interrupted_between(&items, before.0, paused.0),
            "canonical history contains Interrupted between the shutdown pause pair"
        );
        ensure!(
            before.2["state"]["paused"] == false && paused.2["state"]["paused"] == true,
            "shutdown pause pair does not transition false to true"
        );
        let mut expected_paused = before.2.clone();
        expected_paused["state"]["paused"] = Value::Bool(true);
        ensure!(
            expected_paused == paused.2,
            "shutdown pause pair differs outside state.paused"
        );
        ensure!(
            before.2["resume_generation"] == paused.2["resume_generation"],
            "shutdown pause pair generations differ"
        );
        ensure!(
            before.2["root_task_id"] == request.root_task_id
                && paused.2["root_task_id"] == request.root_task_id,
            "shutdown pause pair root does not match request"
        );
        ensure!(
            !interrupted_after(&items, before.0),
            "canonical history contains an Interrupted event after the shutdown-before snapshot"
        );

        let mut legacy_mismatches = BTreeSet::new();
        for (_, hash, value) in &snapshots {
            let is_mismatch = value["active_scope"]["root_task_id"]
                .as_str()
                .is_some_and(|scope_root| scope_root != request.root_task_id);
            if is_mismatch {
                inspect_maintenance_repair_snapshot(
                    &items,
                    hash,
                    &thread,
                    &request.root_task_id,
                    true,
                )
                .map_err(anyhow::Error::msg)?;
                legacy_mismatches.insert(hash.clone());
            } else {
                inspect_maintenance_repair_snapshot(
                    &items,
                    hash,
                    &thread,
                    &request.root_task_id,
                    false,
                )
                .map_err(anyhow::Error::msg)?;
            }
        }
        let requested_mismatches = request
            .legacy_scope_mismatch_snapshot_sha256
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        ensure!(
            requested_mismatches.len() == request.legacy_scope_mismatch_snapshot_sha256.len(),
            "legacy scope mismatch list contains duplicates"
        );
        ensure!(
            requested_mismatches == legacy_mismatches,
            "legacy scope mismatch list does not exactly match canonical history"
        );
        ensure!(
            current_info.uncollected_children.is_empty(),
            "current completion snapshot has uncollected children"
        );
        ensure!(
            current_value["task_checks"]
                .as_array()
                .is_some_and(|checks| checks
                    .iter()
                    .all(|entry| entry["attempt"]["status"] != "Pending")),
            "current snapshot has pending task-check obligations"
        );

        let receipt = MaintenanceRepairReceipt {
            version: 1,
            thread_id: thread.clone(),
            root_task_id: request.root_task_id.clone(),
            request_sha256: request_sha256.clone(),
            snapshot_before_sha256: current_hash.clone(),
            shutdown_before_snapshot_sha256: request.shutdown_before_snapshot_sha256.clone(),
            shutdown_paused_snapshot_sha256: request.shutdown_paused_snapshot_sha256.clone(),
            legacy_scope_mismatch_snapshot_sha256: request
                .legacy_scope_mismatch_snapshot_sha256
                .clone(),
            evidence: evidence.receipt.clone(),
            resume_generation_before: current_info.resume_generation,
            resume_generation_after: current_info
                .resume_generation
                .checked_add(1)
                .context("resume generation overflow")?,
        };
        let repaired = build_maintenance_repair_snapshot(&current_value, receipt)
            .map_err(anyhow::Error::msg)?;
        if !apply {
            return Ok(("dry_run_ready", current_hash, tip));
        }

        let writer = live.as_ref().context("apply writer is missing")?;
        write_started = true;
        writer
            .append_items(&[repair_item(repaired.clone())])
            .await?;
        writer.flush().await?;
        write_confirmed = true;
        let canonical = super::thread_rollout_items(&rollout_path, &thread_id).await?;
        let canonical_snapshots = completion_snapshots(&canonical)?;
        ensure!(
            canonical_snapshots
                .last()
                .is_some_and(|(_, _, value)| value == &repaired),
            "canonical rollout readback does not match repaired snapshot"
        );
        let local_store = store
            .as_ref()
            .context("apply projection store is missing")?;
        verify_projection_checkpoint(local_store, thread_id, &rollout_path).await?;
        Ok((
            "applied",
            maintenance_snapshot_digest(&repaired).map_err(anyhow::Error::msg)?,
            tip,
        ))
    }
    .await;

    match result {
        Ok((status, snapshot_sha256, tip)) => {
            if let Some(writer) = live {
                if apply && status == "applied" {
                    writer.shutdown().await.map_err(|error| {
                        repair_failure(
                            error.into(),
                            Some(true),
                            "canonical-receipt-and-projection-verified-writer-shutdown-failed",
                        )
                    })?;
                } else {
                    writer.discard().await?;
                }
            }
            let output = ResultRecord {
                status,
                thread_id: &thread,
                root_task_id: &request.root_task_id,
                request_sha256: &request_sha256,
                snapshot_sha256: &snapshot_sha256,
                history_tip: &tip,
                live_session_modified: status == "applied",
            };
            println!("{}", serde_json::to_string(&output)?);
            Ok(())
        }
        Err(error) => {
            if let Some(writer) = live {
                let _ = writer.discard().await;
            }
            if write_confirmed {
                let phase = if existing_receipt_retry {
                    "existing-canonical-receipt-projection-retry-or-verification-failed"
                } else {
                    "canonical-append-flushed-but-readback-or-projection-verification-failed"
                };
                Err(repair_failure(error, Some(true), phase))
            } else if write_started {
                Err(repair_failure(
                    error,
                    None,
                    "append-started-outcome-unknown-discard-is-not-rollback",
                ))
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed_evidence_fixture() -> (
        BTreeMap<String, Value>,
        BTreeMap<String, PathBuf>,
        tempfile::TempDir,
        PathBuf,
        String,
    ) {
        let root = tempfile::tempdir().expect("evidence root");
        let canonical_root = root.path().canonicalize().expect("canonical evidence root");
        let thread_id = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e".to_string();
        let codex_home = PathBuf::from("/tmp/codex-switch-fixture");
        let rollout = PathBuf::from("/tmp/rollouts/fixture.jsonl");
        let candidate = canonical_root.join("candidate");
        let deployment = canonical_root.join("deployments/20261111T050631Z");
        std::fs::create_dir_all(&candidate).unwrap();
        std::fs::create_dir_all(&deployment).unwrap();

        let core_hash = "e9247da50ba03e0ea910174d607acf03a90ab1ba7f2f474d2916a96dc2ca872c";
        let helper_hash = "d4b95f28f0e4f5670711730d4ca5f9adea0185783f1fcdf47a43d3d36ed53c3d";
        let old_pid = i32::MAX as u64;
        let resumed_pid = (i32::MAX - 1) as u64;
        let result = serde_json::json!({
            "mode":"apply", "deployment_route":"native_aware_second_pair", "thread":thread_id,
            "target_pid":old_pid, "blockers":[],
            "candidate_hashes":{"rencrow-switch-core":core_hash,"rencrow-compaction":helper_hash},
            "deployment":{"backup":deployment,
                "old_pid":old_pid,
                "installed":[
                    {"path":"/usr/local/bin/rencrow-switch-core","sha256":core_hash,"inode":1},
                    {"path":"/usr/local/bin/rencrow-compaction","sha256":helper_hash,"inode":2}],
                "resumed":{"pid":resumed_pid,"turn_id":"01a1c900-4490-7990-8c54-992b72f51320",
                    "binary_inode":1,"binary_sha256":core_hash,"codex_home":codex_home,
                    "model_effort_worker_preserved":true}}
        });
        let preflight = serde_json::json!({
            "mode":"dry_run", "deployment_route":"native_aware_second_pair", "thread":thread_id,
            "target_pid":old_pid, "blockers":[], "config_changes":[], "supervisor_settings_changes":[],
            "candidate_hashes":{"rencrow-switch-core":core_hash,"rencrow-compaction":helper_hash},
            "terminal_child_background_sessions":[{"terminal_status":"failed"},{"terminal_status":"completed"}]
        });
        let ticket = serde_json::json!({"owner":"root supervisor","user_stop":false,
            "preserved_exact_marker_for_existing_deployer":true});
        let snapshot = serde_json::json!({
            "time":"2026-11-11T05:06:30.815682+00:00", "thread":thread_id,
            "rollout":rollout, "rollout_error":null, "pending_children":[],
            "target_processes":[{"pid":old_pid,"codex_home":codex_home,"observation_known":true}],
            "turn_observation":{"active":false,"tool_running":false,"compacting":false,"exec_background_sessions":[]},
            "process_observation":{"monitored_cli_processes":[]},
            "child_receipts":{"pending":[],"unknown":[]}
        });
        let facts = serde_json::json!({
            "pid":old_pid, "native_second_pair_update":true, "maintenance_ticket":"exact-root-maintenance-marker",
            "activation_identity":{"pid":old_pid,"thread":thread_id,
                "installed_core_sha256":"37fa3d8c9f23c7dc74db890e0369242556ca14f2b27eaf33874800aa15dc1ea2"},
            "candidate_hashes":{"rencrow-switch-core":core_hash,"rencrow-compaction":helper_hash},
            "config_changes":[], "settings_changes":[],
            "terminal_child_background_sessions":[{"terminal_status":"failed"},{"terminal_status":"completed"}]
        });
        let values = BTreeMap::from([
            ("deployment_result".to_string(), result),
            ("deployment_preflight".to_string(), preflight),
            ("maintenance_ticket".to_string(), ticket),
            ("pre_deploy_snapshot".to_string(), snapshot),
            ("pre_deploy_facts".to_string(), facts),
        ]);
        let paths = BTreeMap::from([
            (
                "pre_deploy_snapshot".to_string(),
                deployment.join("pre-deploy-snapshot.json"),
            ),
            (
                "pre_deploy_facts".to_string(),
                deployment.join("pre-deploy-facts.json"),
            ),
        ]);
        (values, paths, root, rollout, thread_id)
    }

    #[test]
    fn repair_item_is_host_marked_and_roundtrips_its_snapshot() {
        let expected = serde_json::json!({"version":1,"thread_id":"thread","root_task_id":"root"});
        let items = vec![repair_item(expected.clone())];
        let snapshots = completion_snapshots(&items).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].2, expected);
    }

    #[test]
    fn request_schema_rejects_unrecognized_fields() {
        let request = serde_json::json!({"version":1,"thread_id":"thread","root_task_id":"root",
            "expected_resume_generation":0,"expected_current_snapshot_sha256":"0".repeat(64),
            "expected_history_tip":{"ordinal":1,"sha256":"1".repeat(64)},
            "shutdown_before_snapshot_sha256":"2".repeat(64),
            "shutdown_paused_snapshot_sha256":"3".repeat(64),
            "legacy_scope_mismatch_snapshot_sha256":[],"evidence":[],"extra":true});
        assert!(serde_json::from_value::<Request>(request).is_err());
    }

    #[test]
    fn post_write_failure_outputs_distinguish_false_unknown_and_true() {
        for (modified, expected_modified, expected_unknown) in [
            (Some(false), false, false),
            (None, false, true),
            (Some(true), true, false),
        ] {
            let error = repair_failure(
                anyhow::anyhow!("simulated operation failure"),
                modified,
                "test-write-phase",
            );
            let mut output = serde_json::json!({
                "live_session_modified":false,
                "live_session_modified_unknown":false
            });
            attach_failure_details(&mut output, &error);
            assert_eq!(output["live_session_modified"], expected_modified);
            assert_eq!(output["live_session_modified_unknown"], expected_unknown);
            assert_eq!(output["write_phase"], "test-write-phase");
        }
    }

    #[test]
    fn observed_evidence_shapes_bind_successfully_and_reject_one_field_mutation() {
        let (mut evidence, paths, root, rollout, thread_id) = observed_evidence_fixture();
        let canonical_root = root.path().canonicalize().unwrap();
        for kind in [
            "deployment_result",
            "deployment_preflight",
            "maintenance_ticket",
            "pre_deploy_snapshot",
            "pre_deploy_facts",
        ] {
            validate_evidence_content(kind, evidence[kind].to_string().as_str(), &thread_id)
                .unwrap_or_else(|error| panic!("observed {kind} payload rejected: {error:#}"));
        }
        validate_evidence_cross_binding(
            &evidence,
            &paths,
            &thread_id,
            Path::new("/tmp/codex-switch-fixture"),
            &rollout,
            &canonical_root,
        )
        .expect("observed evidence payloads should cross-bind");

        evidence.get_mut("deployment_result").unwrap()["deployment"]["resumed"]["codex_home"] =
            Value::String("/tmp/other-codex-home".to_string());
        assert!(
            validate_evidence_cross_binding(
                &evidence,
                &paths,
                &thread_id,
                Path::new("/tmp/codex-switch-fixture"),
                &rollout,
                &canonical_root,
            )
            .is_err()
        );

        let mut missing_rollout_result = evidence["pre_deploy_snapshot"].clone();
        missing_rollout_result
            .as_object_mut()
            .unwrap()
            .remove("rollout_error");
        assert!(
            validate_evidence_content(
                "pre_deploy_snapshot",
                missing_rollout_result.to_string().as_str(),
                &thread_id,
            )
            .is_err()
        );

        let mut compacting_snapshot = evidence["pre_deploy_snapshot"].clone();
        compacting_snapshot["turn_observation"]["compacting"] = Value::Bool(true);
        assert!(
            validate_evidence_content(
                "pre_deploy_snapshot",
                compacting_snapshot.to_string().as_str(),
                &thread_id,
            )
            .is_err()
        );

        let mut background_snapshot = evidence["pre_deploy_snapshot"].clone();
        background_snapshot["turn_observation"]["exec_background_sessions"] =
            serde_json::json!([{"id":"unknown"}]);
        assert!(
            validate_evidence_content(
                "pre_deploy_snapshot",
                background_snapshot.to_string().as_str(),
                &thread_id,
            )
            .is_err()
        );
    }
}
