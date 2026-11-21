// Modified by RenCrow Switch Core, 2026-09-22: handle durable compaction markers.
// Modified by RenCrow Switch Core, 2026-11-21: recover canonical blocked-call proofs.
use super::*;
use crate::context::GuardianContextMode;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::is_user_turn_boundary;
use codex_history::BlockedToolCallReference;
use codex_history::CodexHarnessMetadata;
use codex_history::ObservationReference;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::content_sha256;
use codex_history::compaction_candidate::digest;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionContextWindow;
use serde::Serialize;
use uuid::Uuid;

const MAX_LEGACY_BLOCKED_ERROR_BYTES: usize = 16 * 1024;
const RESERVED_READ_BUDGET_FATAL: &str = "Fatal error: RENCROW_READ_BUDGET_EXCEEDED:";

#[derive(Serialize)]
struct LegacyBlockedCallProof<'a> {
    kind: &'static str,
    error: &'a str,
    source_call_id: &'a str,
    source_payload_sha256: &'a str,
    source_rollout_ordinal: usize,
}

/// Reconstruct only an exact old fatal whose single direct exec call was blocked before
/// execution. The returned false output closes the call/result pair and retains source proof.
pub(super) fn legacy_blocked_tool_call_responses(
    items: &[RolloutItem],
    thread_id: &str,
) -> Vec<ResponseItemEnvelope> {
    const MAX_RECOVERIES_PER_RESUME: usize = 8;
    let mut fatal_turns = Vec::new();
    for (ordinal, item) in items.iter().enumerate() {
        let RolloutItem::EventMsg(EventMsg::TurnComplete(event)) = item else {
            continue;
        };
        let Some(error) = event.error.as_ref() else {
            continue;
        };
        if error.codex_error_info != Some(CodexErrorInfo::Other)
            || !error.message.starts_with(RESERVED_READ_BUDGET_FATAL)
            || !error.message.contains("command not executed")
            || error.message.len() > MAX_LEGACY_BLOCKED_ERROR_BYTES
        {
            continue;
        }
        fatal_turns.push((ordinal, event.turn_id.as_str(), error.message.as_str()));
    }
    if fatal_turns.is_empty() || fatal_turns.len() > MAX_RECOVERIES_PER_RESUME {
        return Vec::new();
    }
    fatal_turns
        .into_iter()
        .filter_map(|(terminal_ordinal, turn_id, error)| {
            legacy_blocked_tool_call_response_for_turn(
                items,
                thread_id,
                terminal_ordinal,
                turn_id,
                error,
            )
        })
        .collect()
}

fn legacy_blocked_tool_call_response_for_turn(
    items: &[RolloutItem],
    thread_id: &str,
    terminal_ordinal: usize,
    turn_id: &str,
    error: &str,
) -> Option<ResponseItemEnvelope> {
    let mut turn_completions = 0usize;
    let mut same_turn_calls = Vec::new();
    let mut output_call_ids = std::collections::HashSet::new();
    let mut completed_exec_call_ids = std::collections::HashSet::new();
    let mut started_exec_call_ids = std::collections::HashSet::new();
    let mut duplicate_call_ids = std::collections::HashSet::new();
    let mut seen_call_ids = std::collections::HashSet::new();
    let expected_blocked_output = error.strip_prefix("Fatal error: ").unwrap_or(error);
    let mut exact_fatal_output_call_ids = std::collections::HashSet::new();
    for (ordinal, item) in items.iter().enumerate() {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if event.turn_id == turn_id => {
                turn_completions += 1;
            }
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::FunctionCall {
                    name,
                    call_id,
                    internal_chat_message_metadata_passthrough,
                    ..
                } => {
                    if !seen_call_ids.insert(call_id.clone()) {
                        duplicate_call_ids.insert(call_id.clone());
                    }
                    if internal_chat_message_metadata_passthrough
                        .as_ref()
                        .and_then(|metadata| metadata.turn_id.as_deref())
                        == Some(turn_id)
                    {
                        same_turn_calls.push((
                            ordinal,
                            envelope,
                            name.as_str(),
                            call_id.as_str(),
                            true,
                        ));
                    }
                }
                ResponseItem::CustomToolCall {
                    name,
                    call_id,
                    internal_chat_message_metadata_passthrough,
                    ..
                } => {
                    if !seen_call_ids.insert(call_id.clone()) {
                        duplicate_call_ids.insert(call_id.clone());
                    }
                    if internal_chat_message_metadata_passthrough
                        .as_ref()
                        .and_then(|metadata| metadata.turn_id.as_deref())
                        == Some(turn_id)
                    {
                        same_turn_calls.push((
                            ordinal,
                            envelope,
                            name.as_str(),
                            call_id.as_str(),
                            false,
                        ));
                    }
                }
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    output,
                    ..
                } => {
                    output_call_ids.insert(call_id.clone());
                    let blocked_reference = envelope
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.rencrow_blocked_tool_call.as_deref());
                    let observation = output.text_content().map(|body| {
                        ObservationReference::new(thread_id, call_id, content_sha256(body))
                    });
                    // The durable proof already attributes this Fatal to its call. The
                    // ephemeral success hint is omitted from canonical output; even a
                    // contradictory hint cannot justify inferring a closure for another call.
                    if output.text_content() == Some(expected_blocked_output)
                        && blocked_reference.is_some_and(|blocked| {
                            observation
                                .as_ref()
                                .is_some_and(|observation| blocked.validates(observation))
                        })
                    {
                        exact_fatal_output_call_ids.insert(call_id.clone());
                    }
                }
                ResponseItem::CustomToolCallOutput { call_id, .. } => {
                    output_call_ids.insert(call_id.clone());
                }
                _ => {}
            },
            RolloutItem::EventMsg(EventMsg::ItemStarted(event))
                if event.thread_id.to_string() == thread_id && event.turn_id == turn_id =>
            {
                if let TurnItem::CommandExecution(command) = &event.item {
                    started_exec_call_ids.insert(command.id.clone());
                }
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if event.thread_id.to_string() == thread_id =>
            {
                if let TurnItem::CommandExecution(command) = &event.item {
                    completed_exec_call_ids.insert(command.id.clone());
                }
            }
            _ => {}
        }
    }
    if turn_completions != 1
        || same_turn_calls.is_empty()
        || same_turn_calls
            .iter()
            .any(|(_, _, _, call_id, _)| exact_fatal_output_call_ids.contains(*call_id))
        || same_turn_calls
            .iter()
            .any(|(_, _, _, call_id, _)| duplicate_call_ids.contains(*call_id))
    {
        return None;
    }

    let mut unmatched = same_turn_calls
        .iter()
        .filter(|(_, _, _, call_id, _)| {
            !output_call_ids.contains(*call_id) && !duplicate_call_ids.contains(*call_id)
        })
        .collect::<Vec<_>>();
    if unmatched.len() != 1 {
        return None;
    }
    let (source_ordinal, source_envelope, name, call_id, direct) = unmatched.pop()?;
    let source_ordinal = *source_ordinal;
    let source_envelope = *source_envelope;
    let name = *name;
    let call_id = *call_id;
    let direct = *direct;
    if !direct
        || name != "exec_command"
        || source_ordinal >= terminal_ordinal
        || source_ordinal != same_turn_calls.last()?.0
        || output_call_ids.contains(call_id)
        || duplicate_call_ids.contains(call_id)
        || started_exec_call_ids.contains(call_id)
        || completed_exec_call_ids.contains(call_id)
    {
        return None;
    }
    let source_payload_sha256 = digest(&(&source_envelope.item, &source_envelope.metadata)).ok()?;
    let proof = LegacyBlockedCallProof {
        kind: "blocked_before_handler",
        error,
        source_call_id: call_id,
        source_payload_sha256: &source_payload_sha256,
        source_rollout_ordinal: source_ordinal,
    };
    let body = serde_json::to_string(&proof).ok()?;
    let reference = ObservationReference::new(thread_id, call_id, content_sha256(&body));
    Some(ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_owned()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(body),
                success: Some(false),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            rencrow_blocked_tool_call: Some(Box::new(BlockedToolCallReference::new(reference))),
            ..Default::default()
        }),
    })
}

// Return value of `Session::reconstruct_history_from_rollout`, bundling the rebuilt history with
// the resume/fork hydration metadata derived from the same replay.
#[derive(Debug)]
pub(super) struct RolloutReconstruction {
    pub(super) history: Vec<ResponseItemEnvelope>,
    pub(super) retained_context: codex_history::RetainedContext,
    pub(super) guardian_history: Option<codex_history::GuardianHistoryCheckpoint>,
    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) reference_context_item: Option<TurnContextItem>,
    pub(super) world_state_baseline: Option<WorldStateSnapshot>,
    pub(super) window_number: u64,
    pub(super) first_window_id: Option<Uuid>,
    pub(super) previous_window_id: Option<Uuid>,
    pub(super) window_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy)]
struct ReconstructedWindow {
    number: u64,
    first_id: Option<Uuid>,
    previous_id: Option<Uuid>,
    id: Option<Uuid>,
}

#[derive(Debug, Default)]
enum TurnReferenceContextItem {
    /// No `TurnContextItem` has been seen for this replay span yet.
    ///
    /// This differs from `Cleared`: `NeverSet` means there is no evidence this turn ever
    /// established a baseline, while `Cleared` means a baseline existed and a later compaction
    /// invalidated it. Only the latter must emit an explicit clearing segment for resume/fork
    /// hydration.
    #[default]
    NeverSet,
    /// A previously established baseline was invalidated by later compaction.
    Cleared,
    /// The latest baseline established by this replay span.
    Latest(Box<TurnContextItem>),
}

#[derive(Debug, Clone, Copy)]
// The selected checkpoint and its replay tail must belong to the same surviving segment.
struct ReplayCheckpoint<'a> {
    compacted: &'a CompactedItem,
    suffix: &'a [RolloutItem],
}

#[derive(Debug, Default)]
struct ActiveReplaySegment<'a> {
    turn_id: Option<String>,
    counts_as_user_turn: bool,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay: Vec<&'a RolloutItem>,
    base_compaction: Option<ReplayCheckpoint<'a>>,
    window: Option<ReconstructedWindow>,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn finalize_active_segment<'a>(
    active_segment: ActiveReplaySegment<'a>,
    base_compaction: &mut Option<ReplayCheckpoint<'a>>,
    previous_turn_settings: &mut Option<PreviousTurnSettings>,
    reference_context_item: &mut TurnReferenceContextItem,
    world_state_replay: &mut Vec<&'a RolloutItem>,
    window: &mut Option<ReconstructedWindow>,
    pending_rollback_turns: &mut usize,
) {
    // Thread rollback drops the newest surviving real user-message boundaries. In replay, that
    // means skipping the next finalized segments that contain a non-contextual
    // `EventMsg::UserMessage`.
    if *pending_rollback_turns > 0 {
        if active_segment.counts_as_user_turn {
            *pending_rollback_turns -= 1;
        }
        return;
    }

    // Full world-state snapshots are persisted after installing initial context. They still
    // establish a baseline when a child fork removes the parent turn's agent message. Do not
    // count these context-only segments as user turns for rollback, or use a snapshot from
    // before the segment's latest compaction.
    let has_context_baseline = active_segment.counts_as_user_turn
        || active_segment
            .world_state_replay
            .iter()
            .take_while(|item| !matches!(item, RolloutItem::Compacted(_)))
            .any(|item| matches!(item, RolloutItem::WorldState(state) if state.full));
    world_state_replay.extend(active_segment.world_state_replay);

    // A surviving replacement-history checkpoint is a complete history base. Once we
    // know the newest surviving one, older rollout items do not affect rebuilt history.
    if base_compaction.is_none()
        && let Some(segment_base_compaction) = active_segment.base_compaction
    {
        *base_compaction = Some(segment_base_compaction);
    }

    if window.is_none() {
        *window = active_segment.window;
    }

    // Restore settings from the newest surviving context baseline.
    if previous_turn_settings.is_none() && has_context_baseline {
        *previous_turn_settings = active_segment.previous_turn_settings;
    }

    // `reference_context_item` comes from the newest surviving context baseline, or
    // from a surviving compaction that explicitly cleared that baseline.
    if matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        && (has_context_baseline
            || matches!(
                active_segment.reference_context_item,
                TurnReferenceContextItem::Cleared
            ))
    {
        *reference_context_item = active_segment.reference_context_item;
    }
}

impl Session {
    pub(super) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> RolloutReconstruction {
        // Replay metadata should already match the shape of the future lazy reverse loader, even
        // while history materialization still uses an eager bridge. Scan newest-to-oldest,
        // stopping once a surviving replacement-history checkpoint and the required resume metadata
        // are both known; then replay only the buffered surviving tail forward to preserve exact
        // history semantics.
        let has_legacy_compaction_without_window_number =
            rollout_items.iter().any(|item| {
                matches!(item, RolloutItem::Compacted(compacted) if compacted.window_number.is_none())
            });
        let initial_window = if has_legacy_compaction_without_window_number {
            None
        } else {
            rollout_items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(session_meta) => session_meta
                    .meta
                    .context_window
                    .as_ref()
                    .and_then(reconstructed_window_from_session_context_window),
                _ => None,
            })
        };
        let mut base_compaction = None;
        let mut previous_turn_settings = None;
        let mut reference_context_item = TurnReferenceContextItem::NeverSet;
        let mut world_state_replay = Vec::new();
        let mut window = None;
        // Rollback is "drop the newest N user turns". While scanning in reverse, that becomes
        // "skip the next N user-turn segments we finalize".
        let mut pending_rollback_turns = 0usize;
        // Reverse replay accumulates rollout items into the newest in-progress turn segment until
        // we hit its matching `TurnStarted`, at which point the segment can be finalized.
        let mut active_segment: Option<ActiveReplaySegment<'_>> = None;

        for (index, item) in rollout_items.iter().enumerate().rev() {
            match item {
                RolloutItem::Compacted(compacted) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                    if active_segment.window.is_none()
                        && let Some(window_number) = compacted.window_number
                    {
                        active_segment.window = Some(ReconstructedWindow {
                            number: window_number,
                            first_id: compacted.first_window_id.as_deref().and_then(parse_uuid_v7),
                            previous_id: compacted
                                .previous_window_id
                                .as_deref()
                                .and_then(parse_uuid_v7),
                            id: compacted.window_id.as_deref().and_then(parse_uuid_v7),
                        });
                    }
                    // Looking backward, compaction clears any older baseline unless a newer
                    // `TurnContextItem` in this same segment has already re-established it.
                    if matches!(
                        active_segment.reference_context_item,
                        TurnReferenceContextItem::NeverSet
                    ) {
                        active_segment.reference_context_item = TurnReferenceContextItem::Cleared;
                    }
                    if active_segment.base_compaction.is_none()
                        && compacted.replacement_history.is_some()
                    {
                        active_segment.base_compaction = Some(ReplayCheckpoint {
                            compacted,
                            suffix: &rollout_items[index + 1..],
                        });
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    pending_rollback_turns = pending_rollback_turns
                        .saturating_add(usize::try_from(rollback.num_turns).unwrap_or(usize::MAX));
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // Reverse replay often sees `TurnComplete` before any turn-scoped metadata.
                    // Capture the turn id early so later `TurnContext` / abort items can match it.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = Some(event.turn_id.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                    if let Some(active_segment) = active_segment.as_mut() {
                        if active_segment.turn_id.is_none()
                            && let Some(turn_id) = &event.turn_id
                        {
                            active_segment.turn_id = Some(turn_id.clone());
                        }
                    } else if let Some(turn_id) = &event.turn_id {
                        active_segment = Some(ActiveReplaySegment {
                            turn_id: Some(turn_id.clone()),
                            ..Default::default()
                        });
                    }
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::TurnContext(ctx) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // `TurnContextItem` can attach metadata to an existing segment, but only a
                    // real `UserMessage` event should make the segment count as a user turn.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = ctx.turn_id.clone();
                    }
                    if turn_ids_are_compatible(
                        active_segment.turn_id.as_deref(),
                        ctx.turn_id.as_deref(),
                    ) {
                        active_segment.previous_turn_settings = Some(PreviousTurnSettings {
                            model: ctx.model.clone(),
                            comp_hash: ctx.comp_hash.clone(),
                            realtime_active: ctx.realtime_active,
                        });
                        if matches!(
                            active_segment.reference_context_item,
                            TurnReferenceContextItem::NeverSet
                        ) {
                            active_segment.reference_context_item =
                                TurnReferenceContextItem::Latest(Box::new(ctx.clone()));
                        }
                    }
                }
                RolloutItem::WorldState(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                }
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                    // `TurnStarted` is the oldest boundary of the active reverse segment.
                    if active_segment.as_ref().is_some_and(|active_segment| {
                        turn_ids_are_compatible(
                            active_segment.turn_id.as_deref(),
                            Some(event.turn_id.as_str()),
                        )
                    }) && let Some(active_segment) = active_segment.take()
                    {
                        finalize_active_segment(
                            active_segment,
                            &mut base_compaction,
                            &mut previous_turn_settings,
                            &mut reference_context_item,
                            &mut world_state_replay,
                            &mut window,
                            &mut pending_rollback_turns,
                        );
                    }
                }
                RolloutItem::ResponseItem(response_item) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn |=
                        is_user_turn_boundary(&response_item.item);
                }
                RolloutItem::InterAgentCommunication(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::RetainedContext(_)
                | RolloutItem::RenCrowCompactionCommit { .. }
                | RolloutItem::SecurityRiskScore(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
            }

            if base_compaction.is_some()
                && previous_turn_settings.is_some()
                && !matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
            {
                // At this point we have both eager resume metadata values and the replacement-
                // history base for the surviving tail, so older rollout items cannot affect this
                // result.
                break;
            }
        }

        if let Some(active_segment) = active_segment.take() {
            finalize_active_segment(
                active_segment,
                &mut base_compaction,
                &mut previous_turn_settings,
                &mut reference_context_item,
                &mut world_state_replay,
                &mut window,
                &mut pending_rollback_turns,
            );
        }

        let fallback_window_number = u64::try_from(
            rollout_items
                .iter()
                .filter(|item| matches!(item, RolloutItem::Compacted(_)))
                .count(),
        )
        .unwrap_or(u64::MAX);

        let mut history = ContextManager::with_guardian_context_mode(
            self.guardian_context_mode,
            &turn_context.session_source,
        );
        let mut saw_legacy_compaction_without_replacement_history = false;
        if let Some(checkpoint) = base_compaction
            && let Some(items) = &checkpoint.compacted.replacement_history
        {
            history.replace_annotated(items.clone());
            history.restore_review_context(
                checkpoint.compacted.retained_context.as_ref(),
                checkpoint.compacted.guardian_history.as_ref(),
                // Keep the backup during replay; the installing session resolves its reviewer.
                /*reviewer_compaction_hash*/
                None,
            );
        }
        // Materialize exact history semantics from the replay-derived suffix. The eventual lazy
        // design should keep this same replay shape, but drive it from a resumable reverse source
        // instead of an eagerly loaded `&[RolloutItem]`.
        let rollout_suffix = base_compaction.map_or(rollout_items, |checkpoint| checkpoint.suffix);
        for item in rollout_suffix {
            match item {
                RolloutItem::RetainedContext(event) => {
                    history.record_retained_context(event);
                }
                RolloutItem::ResponseItem(response_item) => {
                    history.record_annotated_items(
                        std::slice::from_ref(response_item),
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    let response_item = communication.to_model_input_item();
                    history.record_items(
                        std::iter::once(&response_item),
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunicationMetadata { .. } => {}
                RolloutItem::Compacted(compacted) => {
                    // Reverse replay already chose the newest surviving checkpoint. Any newer
                    // replacement checkpoint belongs to a rolled-back turn; replay its original
                    // items so the rollback can still find the removed user boundary.
                    if compacted.replacement_history.is_none() {
                        saw_legacy_compaction_without_replacement_history = true;
                        // Legacy rollouts without `replacement_history` should rebuild the
                        // historical TurnContext at the correct insertion point from persisted
                        // `TurnContextItem`s. These are rare enough that we currently just clear
                        // `reference_context_item`, reinject canonical context at the end of the
                        // resumed conversation, and accept the temporary out-of-distribution
                        // prompt shape.
                        // TODO(ccunningham): if we drop support for None replacement_history compaction items,
                        // we can get rid of this second loop entirely and just build `history` directly in the first loop.
                        let identity =
                            if self.guardian_context_mode == GuardianContextMode::ThreadOwned {
                                compact::CompactedMessageIdentity::Preserve
                            } else {
                                compact::CompactedMessageIdentity::Regenerate
                            };
                        let user_messages = compact::collect_annotated_user_messages(
                            history.annotated_items(),
                            identity,
                        );
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        let retained_context = history.retained_context().clone();
                        history.replace_annotated(rebuilt);
                        history.restore_retained_context(Some(&retained_context));
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::RenCrowCompactionCommit { .. }
                | RolloutItem::SecurityRiskScore(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::SessionMeta(_) => {}
            }
        }

        let reference_context_item = match reference_context_item {
            TurnReferenceContextItem::NeverSet | TurnReferenceContextItem::Cleared => None,
            TurnReferenceContextItem::Latest(turn_reference_context_item) => {
                Some(*turn_reference_context_item)
            }
        };
        let reference_context_item = if saw_legacy_compaction_without_replacement_history {
            None
        } else {
            reference_context_item
        };

        // Segments and their contents were collected newest-first; replay the surviving records
        // chronologically so compaction resets and merge patches have their original meaning.
        world_state_replay.reverse();
        let mut world_state_baseline: Option<WorldStateSnapshot> = None;
        for item in world_state_replay {
            match item {
                RolloutItem::Compacted(_) => world_state_baseline = None,
                RolloutItem::WorldState(world_state) if world_state.full => {
                    world_state_baseline = Some(WorldStateSnapshot::from(&world_state.state));
                }
                RolloutItem::WorldState(world_state) => {
                    let Some(baseline) = world_state_baseline.as_mut() else {
                        tracing::warn!("ignored world-state patch without a full snapshot");
                        continue;
                    };
                    baseline.apply_merge_patch(&world_state.state);
                }
                RolloutItem::SessionMeta(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::TurnContext(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::RetainedContext(_)
                | RolloutItem::RenCrowCompactionCommit { .. }
                | RolloutItem::SecurityRiskScore(_)
                | RolloutItem::EventMsg(_) => {
                    unreachable!("only world-state replay items are collected")
                }
            }
        }

        let window = window.or(initial_window).unwrap_or(ReconstructedWindow {
            number: fallback_window_number,
            first_id: None,
            previous_id: None,
            id: None,
        });
        RolloutReconstruction {
            retained_context: history.retained_context().clone(),
            guardian_history: history.guardian_history_checkpoint(),
            history: history.into_annotated_items(),
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number: window.number,
            first_window_id: window.first_id,
            previous_window_id: window.previous_id,
            window_id: window.id,
        }
    }
}

fn parse_uuid_v7(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|uuid| uuid.get_version_num() == 7)
}

fn reconstructed_window_from_session_context_window(
    context_window: &SessionContextWindow,
) -> Option<ReconstructedWindow> {
    let id = parse_uuid_v7(&context_window.window_id)?;
    Some(ReconstructedWindow {
        number: 0,
        first_id: Some(id),
        previous_id: None,
        id: Some(id),
    })
}
