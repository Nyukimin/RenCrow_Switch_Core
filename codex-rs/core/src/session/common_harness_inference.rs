// Modified by RenCrow Switch Core, 2026-11-27.
// Reads persisted task identity and current step settings for inference receipts.

use crate::session::step_settings::ResolvedStepSettings;
use crate::session::turn_context::TurnContext;
use serde_json::json;

use super::session::Session;

impl Session {
    /// Creates a call receipt context only when persisted Common Harness state
    /// is active for this turn. The awaited getter reads the canonical session
    /// snapshot; no model-authored value can select the phase or task identity.
    pub(crate) async fn common_harness_inference_receipt_context(
        &self,
        turn: &TurnContext,
        settings: &ResolvedStepSettings,
    ) -> Option<super::model_settings_receipt::CommonHarnessInferenceReceiptContext> {
        let (_, state) = self.common_harness_tool_state_for_turn(turn).await?;
        let selected = settings.selected();
        let requested_tier = selected.service_tier.as_deref();
        let model_info = settings.model_info.as_ref();
        let model_route = format!(
            "{}:{:?}",
            turn.provider.info().name,
            turn.provider.info().wire_api
        );

        let wire_requested = json!({
            "model": selected.collaboration_mode.settings.model.as_str(),
            "reasoning_effort": settings.reasoning_effort().map(ToString::to_string),
            "reasoning_summary": selected.reasoning_summary.as_ref(),
            "max_output_tokens": model_info.max_output_tokens,
            "verbosity": turn.config.model_verbosity.as_ref(),
            "service_tier": requested_tier,
        });
        let support = super::model_settings_receipt::ModelSettingsSupport {
            supports_reasoning_summary: model_info.supports_reasoning_summary_parameter,
            supports_verbosity: model_info.support_verbosity,
            supports_requested_service_tier: requested_tier
                .map(|tier| model_info.supports_service_tier(tier))
                .unwrap_or(true),
            used_fallback_model_metadata: model_info.used_fallback_model_metadata,
        };

        Some(
            super::model_settings_receipt::CommonHarnessInferenceReceiptContext::from_persisted_run_state(
                &state.run_state,
                model_route,
                wire_requested,
                support,
            ),
        )
    }
}
