use crate::config::Config;
use crate::thread_manager::build_models_manager;
use codex_login::AuthManager;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::openai_models::ModelsResponse;
use std::sync::Arc;

/// Identifies the provider and authoritative catalog bound to a model manager.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelsManagerBinding {
    model_provider_id: String,
    model_provider: ModelProviderInfo,
    model_catalog: Option<ModelsResponse>,
}

impl ModelsManagerBinding {
    pub(crate) fn new(
        model_provider_id: String,
        model_provider: ModelProviderInfo,
        model_catalog: Option<ModelsResponse>,
    ) -> Self {
        Self {
            model_provider_id,
            model_provider,
            model_catalog,
        }
    }

    pub(crate) fn from_config(config: &Config) -> Self {
        Self::new(
            config.model_provider_id.clone(),
            config.model_provider.clone(),
            config.model_catalog.clone(),
        )
    }
}

/// Reuses the inherited manager for an identical provider/catalog binding, otherwise builds the
/// manager for the effective config with the inherited Codex authentication manager.
pub(crate) fn select_models_manager(
    config: &Config,
    auth_manager: Arc<AuthManager>,
    inherited_manager: &SharedModelsManager,
    inherited_binding: &ModelsManagerBinding,
) -> SharedModelsManager {
    let effective_binding = ModelsManagerBinding::from_config(config);
    if &effective_binding == inherited_binding {
        Arc::clone(inherited_manager)
    } else {
        build_models_manager(config, auth_manager)
    }
}
