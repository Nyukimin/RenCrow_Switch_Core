use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_core::compact::inspect_committed_task_checkpoint_window;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_rollout::decode_rollout_line;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

#[derive(Serialize)]
struct TaskCheckpointProof<'a> {
    version: u32,
    status: &'static str,
    thread_id: &'a str,
    canonical_rollout_path: &'a str,
    rollout_sha256: &'a str,
    history_tip: HistoryTip<'a>,
    checkpoint: Checkpoint<'a>,
    root_task_id: &'a str,
    closed_task_id: &'a str,
    task_revision: u64,
    closed_task_revision: u64,
}

#[derive(Serialize)]
struct HistoryTip<'a> {
    ordinal: u64,
    sha256: &'a str,
}

#[derive(Serialize)]
struct Checkpoint<'a> {
    ordinal: u64,
    sha256: &'a str,
    committed_transaction_hash: &'a str,
    task_boundary_index: usize,
    task_boundary_summary_hash: &'a str,
}

struct CanonicalRow {
    value: Value,
    item: RolloutItem,
    ordinal: u64,
    sha256: String,
}

/// Validate the latest task-boundary candidate in the selected thread's own canonical rollout.
/// This command is read-only and emits no proof until the source bytes have remained unchanged.
pub async fn run(codex_home: &Path, thread: ThreadId, root_task_id: &str) -> Result<()> {
    ensure!(!root_task_id.trim().is_empty(), "root task id is empty");
    let home = super::owning_codex_home(Some(codex_home.to_path_buf()))?;
    let thread_text = thread.to_string();
    let rollout_path = super::thread_rollout(&thread_text, Some(home)).await?;
    ensure!(
        !rollout_path.is_symlink(),
        "canonical rollout path is symlinked"
    );
    let canonical_path = rollout_path
        .canonicalize()
        .context("canonical rollout path does not resolve")?;
    let initial_bytes = std::fs::read(&canonical_path).context("cannot read canonical rollout")?;
    let initial_digest = exact_file_digest(&initial_bytes)?;
    let rows = canonical_rows(&initial_bytes)?;
    let tip = rows.last().context("canonical rollout is empty")?;
    let candidate_index = latest_raw_task_checkpoint_candidate(&rows)?;

    let items = super::thread_rollout_items(&canonical_path, &thread).await?;
    ensure!(
        std::fs::read(&canonical_path).context("cannot verify rollout after typed read")?
            == initial_bytes,
        "canonical rollout changed while its typed history was loading"
    );
    ensure!(
        items.len() == rows.len(),
        "canonical rollout row and typed item counts differ"
    );
    let committed_whole = codex_history::compaction_transaction::committed_items(&items)
        .map_err(anyhow::Error::msg)
        .context("whole rollout compaction integrity validation failed")?;
    let candidate = items
        .get(candidate_index)
        .context("latest raw task checkpoint candidate has no typed item")?;
    let following_items = task_checkpoint_following_items(candidate)?;
    let window_len = following_items
        .checked_add(2)
        .context("task checkpoint transaction window length overflow")?;
    let window_end = candidate_index
        .checked_add(window_len)
        .context("task checkpoint transaction index overflow")?;
    ensure!(
        window_end <= items.len(),
        "latest raw task checkpoint candidate has an incomplete transaction window"
    );
    let proof = inspect_committed_task_checkpoint_window(
        &items[candidate_index..window_end],
        &thread_text,
        root_task_id,
    )
    .map_err(anyhow::Error::msg)
    .context("latest raw task checkpoint candidate failed owner validation")?;
    ensure!(
        proof.root_task_id == root_task_id,
        "validated task checkpoint belongs to another root task"
    );
    ensure!(
        whole_rollout_contains_committed_transaction(
            &committed_whole,
            &proof.committed_transaction_hash
        ),
        "validated task checkpoint is absent from committed whole-rollout history"
    );

    let checkpoint_row = &rows[candidate_index];
    let final_bytes = std::fs::read(&canonical_path).context("cannot reread canonical rollout")?;
    ensure!(
        final_bytes == initial_bytes,
        "canonical rollout changed during task checkpoint validation"
    );
    let final_digest = exact_file_digest(&final_bytes)?;
    ensure!(
        final_digest == initial_digest,
        "canonical rollout digest changed during task checkpoint validation"
    );

    let canonical_path_text = canonical_path
        .to_str()
        .context("canonical rollout path is not UTF-8")?;
    let output = TaskCheckpointProof {
        version: 1,
        status: "validated",
        thread_id: &thread_text,
        canonical_rollout_path: canonical_path_text,
        rollout_sha256: &initial_digest,
        history_tip: HistoryTip {
            ordinal: tip.ordinal,
            sha256: &tip.sha256,
        },
        checkpoint: Checkpoint {
            ordinal: checkpoint_row.ordinal,
            sha256: &checkpoint_row.sha256,
            committed_transaction_hash: &proof.committed_transaction_hash,
            task_boundary_index: proof.task_boundary_index,
            task_boundary_summary_hash: &proof.task_boundary_summary_hash,
        },
        root_task_id: &proof.root_task_id,
        closed_task_id: &proof.closed_task_id,
        task_revision: proof.task_revision,
        closed_task_revision: proof.closed_task_revision,
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn exact_file_digest(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("canonical rollout is not UTF-8 JSONL")?;
    Ok(codex_history::archive_reference::content_sha256(text))
}

fn canonical_rows(bytes: &[u8]) -> Result<Vec<CanonicalRow>> {
    ensure!(
        bytes.last() == Some(&b'\n'),
        "canonical rollout has an incomplete final row"
    );
    let mut lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    ensure!(
        lines.pop() == Some(&b""[..]),
        "canonical rollout row terminator is invalid"
    );
    let mut rows = Vec::new();
    for line in lines {
        ensure!(!line.is_empty(), "canonical rollout contains an empty row");
        let value: Value = serde_json::from_slice(line).context("invalid JSONL row")?;
        let ordinal = value["ordinal"]
            .as_u64()
            .context("canonical rollout row has no unsigned ordinal")?;
        ensure!(
            ordinal == rows.len() as u64,
            "canonical rollout ordinals are not contiguous from zero"
        );
        let item = decode_rollout_line(value.clone())
            .context("canonical rollout row failed the typed decoder")?
            .item;
        let sha256 = codex_history::compaction_candidate::digest(&value)
            .map_err(anyhow::Error::msg)
            .context("cannot hash canonical rollout row")?;
        rows.push(CanonicalRow {
            value,
            item,
            ordinal,
            sha256,
        });
    }
    ensure!(!rows.is_empty(), "canonical rollout is empty");
    Ok(rows)
}

fn latest_raw_task_checkpoint_candidate(rows: &[CanonicalRow]) -> Result<usize> {
    let mut latest = None;
    for (index, row) in rows.iter().enumerate() {
        if !matches!(&row.item, RolloutItem::Compacted(_)) {
            continue;
        }
        let payload = &row.value["payload"];
        let Some(metadata_rows) = payload["replacement_history_metadata"].as_array() else {
            continue;
        };
        let has_task_boundary = metadata_rows.iter().any(|entry| {
            let Some(compaction) = entry.get("rencrow_compaction") else {
                return false;
            };
            compaction.get("version").and_then(Value::as_u64) == Some(3)
                && compaction.get("mode").and_then(Value::as_str) == Some("task_boundary")
        });
        let latest_capacity_has_task_state = metadata_rows.last().is_some_and(|entry| {
            let Some(compaction) = entry.get("rencrow_compaction") else {
                return false;
            };
            compaction.get("version").and_then(Value::as_u64) == Some(3)
                && compaction.get("mode").and_then(Value::as_str) == Some("capacity")
                && compaction.get("task_state").is_some()
        });
        if has_task_boundary || latest_capacity_has_task_state {
            latest = Some(index);
        }
    }
    latest.context("canonical rollout has no raw V3 task-boundary candidate")
}

fn task_checkpoint_following_items(item: &RolloutItem) -> Result<usize> {
    let RolloutItem::Compacted(compacted) = item else {
        anyhow::bail!("latest task checkpoint candidate is not a compacted row")
    };
    let metadata = compacted
        .replacement_history
        .as_ref()
        .and_then(|history| history.last())
        .and_then(|envelope| envelope.metadata.as_ref())
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        .context("latest task checkpoint has no owner transaction metadata")?;
    ensure!(
        metadata.get("version").and_then(Value::as_u64) == Some(3)
            && metadata.get("mode").and_then(Value::as_str) == Some("capacity"),
        "latest task checkpoint does not carry V3 capacity transaction metadata"
    );
    let following = metadata
        .get("transaction_following_items")
        .and_then(Value::as_u64)
        .context("latest task checkpoint has invalid transaction window metadata")?;
    usize::try_from(following).context("task checkpoint transaction window is too large")
}

fn whole_rollout_contains_committed_transaction(
    items: &[RolloutItem],
    expected_hash: &str,
) -> bool {
    items.iter().any(|item| {
        let RolloutItem::Compacted(compacted) = item else {
            return false;
        };
        compacted
            .replacement_history
            .as_ref()
            .and_then(|history| history.last())
            .and_then(|envelope| envelope.metadata.as_ref())
            .and_then(|metadata| metadata.rencrow_compaction.as_ref())
            .and_then(|metadata| metadata.get("committed_transaction_hash"))
            .and_then(Value::as_str)
            == Some(expected_hash)
    })
}

#[cfg(test)]
#[path = "task_checkpoint_tests.rs"]
mod tests;
