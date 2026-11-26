// Modified by RenCrow Switch Core, 2026-11-27.
// Host-owned per-call inference settings receipt projection.

use codex_api::ResponsesApiRequest;
use codex_history::task_context::TaskRunPhaseV1;
use codex_history::task_context::TaskRunStateV1;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Host facts captured from the persisted common-harness state for one model call.
/// The constructor belongs in Session because that is where canonical state and
/// ResolvedStepSettings meet. Serialization is used for trace-local transport;
/// these facts are never read from model input or used to grant authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CommonHarnessInferenceReceiptContext {
    pub(crate) task_id: String,
    pub(crate) request_revision: u64,
    pub(crate) attempt_no: u32,
    pub(crate) phase: TaskRunPhaseV1,
    pub(crate) state_version: u64,
    pub(crate) model_route: String,
    pub(crate) wire_requested: Value,
    pub(crate) support: ModelSettingsSupport,
    pub(crate) config_revision: Option<String>,
    pub(crate) config_revision_source: Option<String>,
}

impl CommonHarnessInferenceReceiptContext {
    pub(crate) fn from_persisted_run_state(
        run_state: &TaskRunStateV1,
        model_route: String,
        wire_requested: Value,
        support: ModelSettingsSupport,
    ) -> Self {
        Self {
            task_id: run_state.task_id.clone(),
            request_revision: run_state.request_revision,
            attempt_no: run_state.attempt_no,
            phase: run_state.phase,
            state_version: run_state.state_version,
            model_route,
            wire_requested,
            support,
            config_revision: None,
            config_revision_source: None,
        }
    }
}

/// Receipt facts captured from one final provider request and its host context.
/// Physical backend identity stays absent until the response header is observed.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ModelSettingsReceipt {
    pub(crate) task_id: String,
    pub(crate) request_revision: u64,
    pub(crate) attempt_no: u32,
    pub(crate) phase: TaskRunPhaseV1,
    pub(crate) state_version: u64,
    pub(crate) model_route: String,
    pub(crate) model_identity: ModelIdentityReceipt,
    pub(crate) wire_requested: Value,
    pub(crate) effective: ProviderRequestSettings,
    pub(crate) effective_source: &'static str,
    /// False means physical application of the provider-request settings is unknown.
    pub(crate) observed: bool,
    pub(crate) unsupported_fields: Vec<String>,
    pub(crate) fallback_reason: Option<String>,
    pub(crate) config_revision: Option<String>,
    pub(crate) config_revision_source: Option<String>,
    /// Filled by the trace attempt using its existing ID, never by the caller.
    pub(crate) call_ref: Option<String>,
    /// Filled by the trace attempt from its response payload, when available.
    pub(crate) usage_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ModelIdentityReceipt {
    pub(crate) requested: String,
    pub(crate) physical_backend: PhysicalBackendObservation,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PhysicalBackendObservation {
    /// Set only from the verified response header.
    pub(crate) value: Option<String>,
    pub(crate) source: Option<&'static str>,
    pub(crate) observed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProviderRequestSettings {
    pub(crate) model: String,
    pub(crate) max_output_tokens: Option<u64>,
    pub(crate) reasoning: Option<Value>,
    pub(crate) verbosity: Option<Value>,
    pub(crate) service_tier: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ModelSettingsSupport {
    pub(crate) supports_reasoning_summary: bool,
    pub(crate) supports_verbosity: bool,
    pub(crate) supports_requested_service_tier: bool,
    pub(crate) used_fallback_model_metadata: bool,
}

pub(crate) fn build_model_settings_receipt(
    context: &CommonHarnessInferenceReceiptContext,
    request: &ResponsesApiRequest,
) -> Result<ModelSettingsReceipt, serde_json::Error> {
    let effective = ProviderRequestSettings {
        model: request.model.clone(),
        max_output_tokens: request.max_output_tokens,
        reasoning: request
            .reasoning
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?,
        verbosity: request
            .text
            .as_ref()
            .and_then(|text| text.verbosity.as_ref())
            .map(serde_json::to_value)
            .transpose()?,
        service_tier: request.service_tier.clone(),
    };

    let mut unsupported_fields = Vec::new();
    if context
        .wire_requested
        .get("reasoning_summary")
        .is_some_and(Value::is_string)
        && !context.support.supports_reasoning_summary
    {
        unsupported_fields.push("reasoning_summary".to_string());
    }
    if context
        .wire_requested
        .get("verbosity")
        .is_some_and(Value::is_string)
        && !context.support.supports_verbosity
    {
        unsupported_fields.push("verbosity".to_string());
    }
    if context
        .wire_requested
        .get("service_tier")
        .and_then(Value::as_str)
        .is_some()
        && !context.support.supports_requested_service_tier
    {
        unsupported_fields.push("service_tier".to_string());
    }
    let fallback_reason = match (
        context.support.used_fallback_model_metadata,
        unsupported_fields.is_empty(),
    ) {
        (true, false) => {
            Some("fallback_model_metadata_used; unsupported_fields_omitted".to_string())
        }
        (true, true) => Some("fallback_model_metadata_used".to_string()),
        (false, false) => Some("unsupported_fields_omitted_provider_defaults".to_string()),
        (false, true) => None,
    };

    Ok(ModelSettingsReceipt {
        task_id: context.task_id.clone(),
        request_revision: context.request_revision,
        attempt_no: context.attempt_no,
        phase: context.phase,
        state_version: context.state_version,
        model_route: context.model_route.clone(),
        model_identity: ModelIdentityReceipt {
            requested: context
                .wire_requested
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&request.model)
                .to_owned(),
            physical_backend: PhysicalBackendObservation {
                value: None,
                source: None,
                observed: false,
            },
        },
        wire_requested: context.wire_requested.clone(),
        effective,
        effective_source: "provider_request",
        observed: false,
        unsupported_fields,
        fallback_reason,
        config_revision: context.config_revision.clone(),
        config_revision_source: context.config_revision_source.clone(),
        call_ref: None,
        usage_ref: None,
    })
}

#[cfg(test)]
#[path = "model_settings_receipt_tests.rs"]
mod tests;
