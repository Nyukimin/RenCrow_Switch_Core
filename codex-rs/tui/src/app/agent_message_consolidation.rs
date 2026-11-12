//! Transcript consolidation for finalized streaming agent messages.
//!
//! During streaming, the chat widget emits transient `AgentMessageCell`s so it
//! can animate stable lines into scrollback while keeping the active mutable
//! tail in the bottom pane. Once the answer finishes, the app replaces all
//! cells tagged with that answer's identity with one source-backed
//! `AgentMarkdownCell`, even when reasoning cells split the stream fragments.
//! Untagged legacy cells use the trailing-run fallback. This makes the transcript
//! the canonical owner of raw markdown used for future resize re-renders. The
//! retained view preserves its displayed revision before replacement, then
//! repaints without replaying terminal scrollback.

use std::path::PathBuf;
use std::sync::Arc;

use color_eyre::eyre::Result;

use super::App;
use super::resize_reflow::trailing_run_start;
use crate::app_event::ConsolidationScrollbackReflow;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::inline_visualization::InlineVisualizationContext;
use crate::pager_overlay::Overlay;
use crate::tui;

impl App {
    pub(super) fn handle_consolidate_agent_message(
        &mut self,
        tui: &mut tui::Tui,
        source: String,
        source_item: Option<(String, String)>,
        cwd: PathBuf,
        inline_visualization_context: Option<InlineVisualizationContext>,
        scrollback_reflow: ConsolidationScrollbackReflow,
        deferred_history_cell: Option<Box<dyn HistoryCell>>,
    ) -> Result<()> {
        // Some finalize paths must preserve a last provisional stream cell long
        // enough for queue ordering, then fold it into the canonical
        // source-backed cell during consolidation.
        if let Some(mut cell) = deferred_history_cell {
            if let Some((turn_id, item_id)) = &source_item
                && let Some(agent_message) = cell
                    .as_any_mut()
                    .downcast_mut::<history_cell::AgentMessageCell>()
                && agent_message.source_item().is_none()
            {
                agent_message.set_source_item(turn_id.clone(), item_id.clone());
            }
            let cell: Arc<dyn HistoryCell> = cell.into();
            if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                t.insert_cell(cell.clone());
            }
            if !tui.is_owned_screen() {
                self.native_history.defer(&cell);
            }
            self.transcript_cells.push(cell);
        }

        // Walk backward to find the contiguous run of streaming AgentMessageCells that
        // belong to the just-finalized stream.
        let end = self.transcript_cells.len();
        tracing::debug!(
            "ConsolidateAgentMessage: transcript_cells.len()={end}, source_len={}",
            source.len()
        );
        let start = trailing_run_start::<history_cell::AgentMessageCell>(&self.transcript_cells);
        let tagged_indices = source_item
            .as_ref()
            .map_or_else(Vec::new, |(turn_id, item_id)| {
                self.transcript_cells
                    .iter()
                    .enumerate()
                    .filter_map(|(index, cell)| {
                        (cell
                            .as_any()
                            .downcast_ref::<history_cell::AgentMarkdownCell>()
                            .and_then(history_cell::AgentMarkdownCell::source_item)
                            == Some((turn_id.as_str(), item_id.as_str())))
                        .then_some(index)
                    })
                    .collect::<Vec<_>>()
            });
        let tagged_stream_indices =
            source_item
                .as_ref()
                .map_or_else(Vec::new, |(turn_id, item_id)| {
                    self.transcript_cells
                        .iter()
                        .enumerate()
                        .filter_map(|(index, cell)| {
                            (cell
                                .as_any()
                                .downcast_ref::<history_cell::AgentMessageCell>()
                                .and_then(history_cell::AgentMessageCell::source_item)
                                == Some((turn_id.as_str(), item_id.as_str())))
                            .then_some(index)
                        })
                        .collect::<Vec<_>>()
                });
        let mut tagged_answer_indices = tagged_stream_indices.clone();
        tagged_answer_indices.extend(tagged_indices.iter().copied());
        tagged_answer_indices.sort_unstable();
        tagged_answer_indices.dedup();
        if !tagged_stream_indices.is_empty() {
            let mut replacement = history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                source,
                &cwd,
                inline_visualization_context.clone(),
            );
            if let Some((turn_id, item_id)) = source_item {
                replacement.set_source_item(turn_id, item_id);
            }
            let replacement: Arc<dyn HistoryCell> = Arc::new(replacement);
            let existing_cells = tagged_answer_indices
                .iter()
                .map(|index| self.transcript_cells[*index].clone())
                .collect::<Vec<_>>();
            self.native_history
                .consolidate(&existing_cells, &replacement);
            let first_index = tagged_answer_indices[0];
            for index in tagged_answer_indices {
                let cell = if index == first_index {
                    replacement.clone()
                } else {
                    Arc::new(
                        history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                            String::new(),
                            &cwd,
                            inline_visualization_context.clone(),
                        ),
                    )
                };
                if tui.is_owned_screen() {
                    self.transcript_view.replace_group(
                        &self.transcript_cells,
                        index..index + 1,
                        &cell,
                    );
                } else {
                    self.transcript_view.replace_range(
                        &self.transcript_cells,
                        index..index + 1,
                        &cell,
                    );
                }
                self.transcript_cells[index] = cell.clone();
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.regroup_cells(index..index + 1, cell);
                    tui.frame_requester().schedule_frame();
                }
            }
            self.finish_agent_message_consolidation(tui, scrollback_reflow)?;
        } else if start < end && source_item.is_none() {
            tracing::debug!(
                "ConsolidateAgentMessage: replacing cells [{start}..{end}] with AgentMarkdownCell"
            );
            let mut cell = history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                source,
                &cwd,
                inline_visualization_context,
            );
            if let Some((turn_id, item_id)) = source_item {
                cell.set_source_item(turn_id, item_id);
            }
            let consolidated: Arc<dyn HistoryCell> = Arc::new(cell);
            self.native_history
                .consolidate(&self.transcript_cells[start..end], &consolidated);
            if tui.is_owned_screen() {
                self.transcript_view.replace_group(
                    &self.transcript_cells,
                    start..end,
                    &consolidated,
                );
            } else {
                self.transcript_view.replace_range(
                    &self.transcript_cells,
                    start..end,
                    &consolidated,
                );
            }
            self.transcript_cells
                .splice(start..end, std::iter::once(consolidated.clone()));

            if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                t.regroup_cells(start..end, consolidated.clone());
                tui.frame_requester().schedule_frame();
            }

            self.finish_agent_message_consolidation(tui, scrollback_reflow)?;
        } else if let Some((turn_id, item_id)) = source_item {
            let mut replacement = history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                source.clone(),
                &cwd,
                inline_visualization_context.clone(),
            );
            replacement.set_source_item(turn_id.clone(), item_id.clone());
            let replacement: Arc<dyn HistoryCell> = Arc::new(replacement);
            let existing_indices = self
                .transcript_cells
                .iter()
                .enumerate()
                .filter_map(|(index, cell)| {
                    (cell
                        .as_any()
                        .downcast_ref::<history_cell::AgentMarkdownCell>()
                        .and_then(history_cell::AgentMarkdownCell::source_item)
                        == Some((turn_id.as_str(), item_id.as_str())))
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            if !existing_indices.is_empty() {
                let existing_cells = existing_indices
                    .iter()
                    .map(|index| self.transcript_cells[*index].clone())
                    .collect::<Vec<_>>();
                self.native_history
                    .consolidate(&existing_cells, &replacement);
                let first_index = existing_indices[0];
                for index in existing_indices {
                    let cell = if index == first_index {
                        replacement.clone()
                    } else {
                        Arc::new(
                            history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                                String::new(),
                                &cwd,
                                inline_visualization_context.clone(),
                            ),
                        )
                    };
                    if tui.is_owned_screen() {
                        self.transcript_view.replace_group(
                            &self.transcript_cells,
                            index..index + 1,
                            &cell,
                        );
                    } else {
                        self.transcript_view.replace_range(
                            &self.transcript_cells,
                            index..index + 1,
                            &cell,
                        );
                    }
                    self.transcript_cells[index] = cell.clone();
                    if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                        t.regroup_cells(index..index + 1, cell);
                        tui.frame_requester().schedule_frame();
                    }
                }
                self.finish_agent_message_consolidation(
                    tui,
                    ConsolidationScrollbackReflow::Required,
                )?;
            } else {
                let mut fallback = history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                    source,
                    &cwd,
                    inline_visualization_context,
                );
                fallback.set_source_item(turn_id, item_id);
                self.insert_history_cell(tui, Box::new(fallback));
            }
        } else {
            tracing::debug!(
                "ConsolidateAgentMessage: no cells to consolidate(start={start}, end={end})",
            );
            self.maybe_finish_stream_reflow(tui)?;
        }

        Ok(())
    }

    fn finish_agent_message_consolidation(
        &mut self,
        tui: &mut tui::Tui,
        scrollback_reflow: ConsolidationScrollbackReflow,
    ) -> Result<()> {
        if tui.is_owned_screen() {
            self.transcript_reflow.clear_pending_reflow();
            self.transcript_reflow.clear_stream_flags();
            tui.frame_requester().schedule_frame();
            return Ok(());
        }
        match scrollback_reflow {
            ConsolidationScrollbackReflow::IfResizeReflowRan => {
                self.maybe_finish_stream_reflow(tui)?;
            }
            ConsolidationScrollbackReflow::Required => {
                self.finish_required_stream_reflow(tui)?;
            }
        }

        Ok(())
    }
}
