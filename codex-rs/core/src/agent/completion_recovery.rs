//! Observe due plan reviews and bounded readiness for the supported external blocker.
// Modified by RenCrow Switch Core, 2026-11-11.

use crate::codex_thread::CodexThread;
use crate::context::AutonomousCompletionContinue;
use crate::context::ContextualUserFragment;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout;
use tokio_util::task::AbortOnDropHandle;

pub(crate) const SUPPORTED_RECHECK_CONDITION: &str = "current_model_provider_ready";
const CHECK_FRESHNESS_SECONDS: i64 = 120;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OBSERVER_INTERVAL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelReadiness {
    pub checked_at: i64,
}

/// Starts the single bounded readiness observer owned by an opted-in parent thread.
///
/// When `defer_until_next_user_turn` is true, the observer does no network work until a real
/// runtime turn advances beyond the startup baseline. The observer uses a weak thread reference
/// and an abort-on-drop handle stored in SessionState, so it cannot keep a closed session alive.
pub(crate) async fn start(parent: &Arc<CodexThread>, defer_until_next_user_turn: bool) {
    let initial_config = parent.config().await;
    if !initial_config.rencrow_autonomous_completion
        && !parent
            .session
            .has_pending_completion_review_deadline()
            .await
    {
        return;
    }
    let startup_turn_id = parent
        .session
        .autonomous_completion_previous_turn_id()
        .await;
    let parent_weak = Arc::downgrade(parent);
    let task_parent = parent_weak.clone();
    let handle = tokio::spawn(async move {
        let mut deferred = defer_until_next_user_turn;
        let mut interval =
            tokio::time::interval_at(Instant::now() + OBSERVER_INTERVAL, OBSERVER_INTERVAL);
        loop {
            interval.tick().await;
            let Some(parent) = task_parent.upgrade() else {
                return;
            };
            let config = parent.config().await;
            parent
                .session
                .enforce_due_completion_review_deadlines()
                .await;
            if !config.rencrow_autonomous_completion {
                if parent
                    .session
                    .has_pending_completion_review_deadline()
                    .await
                {
                    continue;
                }
                parent.session.clear_completion_recovery_task().await;
                return;
            }
            if parent.session.autonomous_completion_paused().await {
                continue;
            }
            if deferred {
                if parent
                    .session
                    .autonomous_completion_previous_turn_id()
                    .await
                    == startup_turn_id
                {
                    continue;
                }
                deferred = false;
            }
            wake_due_plan_review(&parent).await;
            let Some((root_task_id, observed_state)) = parent
                .session
                .autonomous_completion_recovery_candidate()
                .await
            else {
                continue;
            };
            if parent
                .session
                .autonomous_completion_has_unresolved_runtime_work()
                .await
            {
                continue;
            }
            let thread_config = parent.config_snapshot().await;
            if thread_config.model_provider_id != "rencrow" {
                continue;
            }
            let Some(base_url) = config
                .model_provider
                .base_url
                .as_deref()
                .filter(|url| !url.trim().is_empty())
            else {
                continue;
            };
            let client_factory = config.http_client_factory();
            let checked_at = chrono::Utc::now().timestamp();
            if probe_model_readiness(&client_factory, base_url, &thread_config.model, checked_at)
                .await
                .is_none()
            {
                continue;
            }
            match parent
                .session
                .clear_recovered_external_blocker(&root_task_id, &observed_state)
                .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    tracing::warn!(%error, "failed to persist recovered external blocker state");
                    continue;
                }
            }
            if parent.session.autonomous_completion_paused().await
                || !parent.session.autonomous_completion_pending().await
                || parent
                    .session
                    .autonomous_completion_has_unresolved_runtime_work()
                    .await
            {
                continue;
            }
            let Some(previous_turn_id) = parent
                .session
                .autonomous_completion_previous_turn_id()
                .await
            else {
                continue;
            };
            let Some((_, state)) = parent.session.autonomous_completion_current_state().await
            else {
                continue;
            };
            let next_action = state
                .next_action
                .filter(|action| !action.trim().is_empty())
                .unwrap_or_else(|| {
                    "Recheck the recovered service and continue a safe ready unit".into()
                });
            let event_key = format!("external-recovery:{root_task_id}:{checked_at}");
            if parent
                .session
                .autonomous_completion_event_seen(&event_key)
                .await
            {
                continue;
            }
            let request = TurnInputRequest::new(TurnInput::ResponseItem(
                Box::new(AutonomousCompletionContinue { next_action }).into_boxed_response_item(),
            ));
            match parent
                .continue_autonomous_turn_if_idle(request, previous_turn_id.clone())
                .await
            {
                Ok(TurnInputSubmission::Started { .. }) => {
                    parent
                        .session
                        .mark_autonomous_completion_continued(&previous_turn_id, &event_key)
                        .await;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "external blocker recovery continuation was not admitted")
                }
            }
        }
    });
    parent
        .session
        .install_completion_recovery_task(AbortOnDropHandle::new(handle))
        .await;
}

/// A parent that yielded before its review deadline still receives one review wake.
/// Ordinary ticks and unchanged review windows never cause repeated model calls.
async fn wake_due_plan_review(parent: &CodexThread) {
    let Some(event_key) = due_plan_review_event(&parent.session).await else {
        return;
    };
    let Some(previous_turn_id) = parent
        .session
        .autonomous_completion_previous_turn_id()
        .await
    else {
        return;
    };
    let request = TurnInputRequest::new(TurnInput::ResponseItem(
        Box::new(AutonomousCompletionContinue {
            next_action: "The saved plan review is due. Review the unmet conditions and failed approaches, record a changed plan with evidence, and continue safe ready work. Do not reset product progress or repeat an unchanged failed approach.".into(),
        }).into_boxed_response_item(),
    ));
    match parent
        .continue_autonomous_turn_if_idle(request, previous_turn_id.clone())
        .await
    {
        Ok(TurnInputSubmission::Started { .. }) => {
            parent
                .session
                .mark_autonomous_completion_continued(&previous_turn_id, &event_key)
                .await;
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "due plan review continuation was not admitted"),
    }
}

pub(crate) async fn due_plan_review_event(
    session: &crate::session::session::Session,
) -> Option<String> {
    if session.has_pending_completion_check().await {
        return None;
    }
    if !session.autonomous_completion_pending().await
        || !session.autonomous_completion_plan_review_due().await
        || session
            .autonomous_completion_has_unresolved_runtime_work()
            .await
        || session.has_uncollected_completion_children().await
    {
        return None;
    }
    let (root, state) = session.autonomous_completion_current_state().await?;
    if state.paused || state.blocker.is_some() {
        return None;
    }
    let event_key = format!(
        "idle-plan-review:{root}:{}:{}:{}",
        state.contract_revision,
        state.plan_review_at.unwrap_or_default(),
        state.units_completed_since_review,
    );
    (!session.autonomous_completion_event_seen(&event_key).await).then_some(event_key)
}

#[derive(Deserialize)]
struct ModelList {
    #[serde(alias = "models")]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    alive: bool,
    checked_at: String,
}

/// Probe only the caller-supplied, host-configured provider URL and model.
///
/// The caller must establish that this is the configured RenCrow provider and that the
/// recheck-condition exactly matches [`SUPPORTED_RECHECK_CONDITION`]. This function never reads
/// model-written URLs, executes commands, follows redirects, or treats an ordinary `/models`
/// listing as readiness: the matching entry must include `alive: true` and a fresh host-service
/// `checked_at` timestamp.
pub(crate) async fn probe_model_readiness(
    http_client_factory: &HttpClientFactory,
    provider_base_url: &str,
    requested_model: &str,
    now: i64,
) -> Option<ModelReadiness> {
    let base_url = provider_base_url.trim_end_matches('/');
    if base_url.is_empty() || requested_model.trim().is_empty() {
        return None;
    }
    let endpoint = format!("{base_url}/models");
    let client = HttpClientBuilder::new()
        .without_request_logging()
        .without_redirects()
        .build_respecting_outbound_proxy_policy(
            http_client_factory,
            &endpoint,
            ClientRouteClass::Api,
        )
        .ok()?;
    let response = timeout(REQUEST_TIMEOUT, async {
        let response = client.get(&endpoint).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return None;
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<ModelList>(&bytes).ok()
    })
    .await
    .ok()??;
    response.data.into_iter().find_map(|entry| {
        if entry.id != requested_model || !entry.alive {
            return None;
        }
        let checked_at = chrono::DateTime::parse_from_rfc3339(&entry.checked_at)
            .ok()?
            .timestamp();
        let age = now.checked_sub(checked_at)?;
        (0..=CHECK_FRESHNESS_SECONDS)
            .contains(&age)
            .then_some(ModelReadiness { checked_at })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::OutboundProxyPolicy;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn factory() -> HttpClientFactory {
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
    }

    #[tokio::test]
    async fn ready_model_requires_true_alive_and_fresh_check_time() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id":"other-model","alive":true,"checked_at":"1970-01-01T00:16:39Z"},
                    {"id":"wanted-model","alive":true,"checked_at":"1970-01-01T00:15:40Z"}
                ]
            })))
            .mount(&server)
            .await;

        let result = probe_model_readiness(
            &factory(),
            &format!("{}/v1/", server.uri()),
            "wanted-model",
            1000,
        )
        .await;

        assert_eq!(result, Some(ModelReadiness { checked_at: 940 }));
    }

    #[tokio::test]
    async fn stale_dead_and_unlisted_models_are_not_ready() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [
                    {"id":"stale","alive":true,"checked_at":"1970-01-01T00:14:39Z"},
                    {"id":"dead","alive":false,"checked_at":"1970-01-01T00:16:39Z"}
                ]
            })))
            .mount(&server)
            .await;

        assert_eq!(
            probe_model_readiness(&factory(), &server.uri(), "stale", 1000).await,
            None
        );
        assert_eq!(
            probe_model_readiness(&factory(), &server.uri(), "dead", 1000).await,
            None
        );
        assert_eq!(
            probe_model_readiness(&factory(), &server.uri(), "missing", 1000).await,
            None
        );
    }

    #[tokio::test]
    async fn errors_redirects_and_malformed_payloads_remain_blocked() {
        for (status, body) in [
            (503, json!({"data": []})),
            (200, json!({"data": [{"id":"wanted","alive":true}]})),
            (
                200,
                json!({"data": [{"id":"wanted","alive":true,"checked_at":"not-a-time"}]}),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            assert_eq!(
                probe_model_readiness(&factory(), &server.uri(), "wanted", 1000).await,
                None
            );
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/ready"))
            .mount(&server)
            .await;
        assert_eq!(
            probe_model_readiness(&factory(), &server.uri(), "wanted", 1000).await,
            None
        );
    }
}
