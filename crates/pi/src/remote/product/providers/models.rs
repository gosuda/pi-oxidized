//! Native provider for the experimental model catalogue and selection service.
//!
//! The source service keeps its catalogue and configuration in one mutable
//! replicated state.  This provider follows the same publication boundaries:
//! refresh publishes `refreshing` before model-runtime I/O, and every completed
//! mutation publishes one coherent state revision.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pi_agent::context::Context;
use pi_agent::harness::api::AgentLane;
use pi_agent::harness::result::HarnessError;
use pi_agent::service::error::{RemoteServiceErrorCode, ServiceError};
use pi_agent::service::provider::{ServiceImplementation, ServiceMember, ServiceMethod};
use pi_agent::service::replicated::MutableReplicatedState;
use pi_agent::service::value::{JsInteger, JsObject, JsString, JsonValue};
use pi_agent::session::ModelIdentity;
use pi_ai::types::ModelThinkingLevel;
use thiserror::Error;

use crate::core::agent_session::model::supported_thinking_levels;
use crate::core::model_runtime::{ModelRuntime, ModelsRefreshOptions};
use crate::core::settings::SettingsManager;
use crate::remote::product::services::ProductJsonConvert;
use crate::remote::product::services::models::{
    MODELS_CYCLE_THINKING_MEMBER, MODELS_GET_THINKING_LEVELS_MEMBER, MODELS_REFRESH_MEMBER,
    MODELS_SELECT_MEMBER, MODELS_SELECT_THINKING_MEMBER, MODELS_STATE_MEMBER, ModelRef,
    ModelSummary, ModelsCatalog, ModelsConfiguration, ModelsRefreshState, ModelsState,
};

/// Native implementation of the singleton `pi.models` service.
///
/// The service owns no external subscriptions.  Dropping the returned `Arc`
/// therefore is the complete teardown operation; the worker owns the provider
/// registration that publishes this implementation.
pub struct ModelsService {
    state: Arc<MutableReplicatedState>,
    lane: Arc<dyn AgentLane>,
    model_runtime: Option<Arc<ModelRuntime>>,
    settings: Option<Arc<Mutex<SettingsManager>>>,
    catalog_revision: Mutex<JsInteger>,
}

impl std::fmt::Debug for ModelsService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelsService")
            .field("sequence", &self.state.sequence())
            .field("catalog_revision", &lock(&self.catalog_revision))
            .finish_non_exhaustive()
    }
}

impl ModelsService {
    /// Creates an inactive service with the source's initial state.
    #[must_use]
    pub fn new(
        lane: Arc<dyn AgentLane>,
        model_runtime: Option<Arc<ModelRuntime>>,
        settings: Option<Arc<Mutex<SettingsManager>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: MutableReplicatedState::new(initial_state()),
            lane,
            model_runtime,
            settings,
            catalog_revision: Mutex::new(JsInteger::zero()),
        })
    }

    /// Returns the singleton implementation published through the canonical
    /// `RemoteServiceProvider`.
    #[must_use]
    pub fn implementation(self: &Arc<Self>) -> ServiceImplementation {
        let mut implementation = ServiceImplementation::new();
        implementation.insert(
            member(MODELS_STATE_MEMBER),
            ServiceMember::State(Arc::clone(&self.state)),
        );

        let service = Arc::clone(self);
        implementation.insert(
            member(MODELS_CYCLE_THINKING_MEMBER),
            ServiceMember::Method(method(move |args, context| {
                let service = Arc::clone(&service);
                async move {
                    expect_no_args(&args, MODELS_CYCLE_THINKING_MEMBER)?;
                    service.cycle_thinking(context).await?;
                    Ok(None)
                }
            })),
        );

        let service = Arc::clone(self);
        implementation.insert(
            member(MODELS_GET_THINKING_LEVELS_MEMBER),
            ServiceMember::Method(method(move |args, context| {
                let service = Arc::clone(&service);
                async move {
                    expect_no_args(&args, MODELS_GET_THINKING_LEVELS_MEMBER)?;
                    let levels = service.get_thinking_levels(&context).await?;
                    Ok(Some(JsonValue::Array(
                        levels.into_iter().map(thinking_level_json).collect(),
                    )))
                }
            })),
        );

        let service = Arc::clone(self);
        implementation.insert(
            member(MODELS_REFRESH_MEMBER),
            ServiceMember::Method(method(move |args, context| {
                let service = Arc::clone(&service);
                async move {
                    expect_no_args(&args, MODELS_REFRESH_MEMBER)?;
                    service.refresh(context).await?;
                    Ok(None)
                }
            })),
        );

        let service = Arc::clone(self);
        implementation.insert(
            member(MODELS_SELECT_MEMBER),
            ServiceMember::Method(method(move |args, context| {
                let service = Arc::clone(&service);
                async move {
                    let model = ModelRef::from_json(one_arg(&args, MODELS_SELECT_MEMBER)?)?;
                    service.select(model, context).await?;
                    Ok(None)
                }
            })),
        );

        let service = Arc::clone(self);
        implementation.insert(
            member(MODELS_SELECT_THINKING_MEMBER),
            ServiceMember::Method(method(move |args, context| {
                let service = Arc::clone(&service);
                async move {
                    let level =
                        parse_thinking_level(&one_arg(&args, MODELS_SELECT_THINKING_MEMBER)?)?;
                    service.select_thinking(level, context).await?;
                    Ok(None)
                }
            })),
        );

        implementation
    }

    /// Reads the initial catalogue/configuration and publishes activation.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the catalogue or configuration cannot be
    /// read, or the initial state cannot be published.
    pub async fn activate(&self, context: Context) -> Result<(), ServiceError> {
        let (catalog, configuration) = tokio::try_join!(
            self.read_catalog(&context),
            self.read_configuration(&context),
        )?;
        self.publish_state(
            ModelsState {
                catalog,
                configuration,
                refresh: ModelsRefreshState::Idle,
            },
            context,
        )
    }

    async fn cycle_thinking(&self, context: Context) -> Result<(), ServiceError> {
        let levels = self.read_thinking_levels(&context).await?;
        let current = into_service_result(self.lane.get_thinking_level(&context).await)?;
        let next = if levels.is_empty() {
            ModelThinkingLevel::Off
        } else {
            let index = levels
                .iter()
                .position(|level| *level == current)
                .map_or(0, |index| (index + 1) % levels.len());
            levels
                .get(index)
                .copied()
                .unwrap_or(ModelThinkingLevel::Off)
        };
        into_service_result(self.lane.set_thinking_level(next, &context).await)?;
        let configuration = self.read_configuration(&context).await?;
        self.update_configuration(configuration, context)
    }

    async fn get_thinking_levels(
        &self,
        context: &Context,
    ) -> Result<Vec<ModelThinkingLevel>, ServiceError> {
        self.read_thinking_levels(context).await
    }

    async fn refresh(&self, context: Context) -> Result<(), ServiceError> {
        self.update_refresh(ModelsRefreshState::Refreshing, context.clone())?;

        let Some(model_runtime) = self.model_runtime.as_ref() else {
            let (catalog, configuration) = tokio::try_join!(
                self.read_catalog(&context),
                self.read_configuration(&context),
            )?;
            return self.publish_state(
                ModelsState {
                    catalog,
                    configuration,
                    refresh: ModelsRefreshState::Done,
                },
                context,
            );
        };

        let result = model_runtime
            .refresh(ModelsRefreshOptions {
                signal: context.token().cloned(),
                ..ModelsRefreshOptions::default()
            })
            .await
            .map_err(ServiceError::handler)?;
        let errors = result.errors;
        let (catalog, configuration) = tokio::try_join!(
            self.read_catalog(&context),
            self.read_configuration(&context),
        )?;
        let refresh = if errors.is_empty() {
            ModelsRefreshState::Done
        } else {
            ModelsRefreshState::Warning { errors }
        };
        self.publish_state(
            ModelsState {
                catalog,
                configuration,
                refresh,
            },
            context,
        )
    }

    async fn select(&self, model: ModelRef, context: Context) -> Result<(), ServiceError> {
        let selected = self
            .model_runtime
            .as_ref()
            .and_then(|runtime| runtime.get_model(&model.provider, &model.model_id))
            .ok_or_else(|| {
                source_error(format!(
                    "Unknown model: {}/{}",
                    model.provider, model.model_id
                ))
            })?;
        into_service_result(
            self.lane
                .set_model(
                    ModelIdentity {
                        provider: selected.provider.clone(),
                        model_id: selected.id.clone(),
                        api: Some(selected.api.clone()),
                    },
                    &context,
                )
                .await,
        )?;

        if let Some(settings) = &self.settings {
            let mut settings = lock(settings);
            settings.set_default_model_and_provider(&selected.provider, &selected.id);
            settings.flush();
        }

        let configuration = self.read_configuration(&context).await?;
        self.update_configuration(configuration, context)
    }

    async fn select_thinking(
        &self,
        level: ModelThinkingLevel,
        context: Context,
    ) -> Result<(), ServiceError> {
        let levels = self.read_thinking_levels(&context).await?;
        if !levels.contains(&level) {
            let available = levels
                .iter()
                .copied()
                .map(thinking_level_name)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(source_error(format!(
                "Thinking level {} is unavailable; choose one of: {available}",
                thinking_level_name(level),
            )));
        }
        into_service_result(self.lane.set_thinking_level(level, &context).await)?;
        let configuration = self.read_configuration(&context).await?;
        self.update_configuration(configuration, context)
    }

    async fn read_configuration(
        &self,
        context: &Context,
    ) -> Result<ModelsConfiguration, ServiceError> {
        let selected = async { into_service_result(self.lane.get_model(context).await) };
        let thinking_level =
            async { into_service_result(self.lane.get_thinking_level(context).await) };
        let (selected, thinking_level) = tokio::try_join!(selected, thinking_level)?;
        Ok(ModelsConfiguration {
            model: selected.map(|model| ModelRef {
                provider: model.provider,
                model_id: model.id,
            }),
            thinking_level,
        })
    }

    async fn read_thinking_levels(
        &self,
        context: &Context,
    ) -> Result<Vec<ModelThinkingLevel>, ServiceError> {
        let selected = into_service_result(self.lane.get_model(context).await)?;
        Ok(match selected.as_ref() {
            None => vec![ModelThinkingLevel::Off],
            Some(model) => supported_thinking_levels(model),
        })
    }

    async fn read_catalog(&self, context: &Context) -> Result<ModelsCatalog, ServiceError> {
        let selected = into_service_result(self.lane.get_model(context).await)?;
        let mut available = match &self.model_runtime {
            Some(runtime) => runtime.get_available_snapshot(),
            None => Vec::new(),
        };
        if let Some(selected) = selected {
            let present = available
                .iter()
                .any(|model| model.provider == selected.provider && model.id == selected.id);
            if !present {
                available.push(selected);
            }
        }
        let revision = self.next_catalog_revision();
        Ok(ModelsCatalog {
            revision,
            available_models: available
                .into_iter()
                .map(|model| ModelSummary {
                    provider: model.provider,
                    model_id: model.id,
                    name: model.name,
                    reasoning: model.reasoning,
                })
                .collect(),
        })
    }

    fn read_state(&self) -> Result<ModelsState, ServiceError> {
        ModelsState::from_json(self.state.state().as_ref().clone())
    }

    fn update_refresh(
        &self,
        refresh: ModelsRefreshState,
        context: Context,
    ) -> Result<(), ServiceError> {
        let mut state = self.read_state()?;
        state.refresh = refresh;
        self.publish_state(state, context)
    }

    fn update_configuration(
        &self,
        configuration: ModelsConfiguration,
        context: Context,
    ) -> Result<(), ServiceError> {
        let mut state = self.read_state()?;
        state.configuration = configuration;
        self.publish_state(state, context)
    }

    fn publish_state(&self, state: ModelsState, context: Context) -> Result<(), ServiceError> {
        let value = state.into_json()?;
        self.state.with_state_mut(|current| *current = value);
        self.state.publish(context)
    }

    fn next_catalog_revision(&self) -> JsInteger {
        let mut revision = lock(&self.catalog_revision);
        let next = revision.next();
        *revision = next;
        next
    }
}

fn initial_state() -> JsonValue {
    JsonValue::Object(JsObject::from([
        (
            JsString::from_utf8("catalog"),
            JsonValue::Object(JsObject::from([
                (JsString::from_utf8("revision"), JsonValue::Number(0.0)),
                (
                    JsString::from_utf8("availableModels"),
                    JsonValue::Array(Vec::new()),
                ),
            ])),
        ),
        (
            JsString::from_utf8("configuration"),
            JsonValue::Object(JsObject::from([
                (JsString::from_utf8("model"), JsonValue::Null),
                (
                    JsString::from_utf8("thinkingLevel"),
                    JsonValue::String(JsString::from_utf8("off")),
                ),
            ])),
        ),
        (
            JsString::from_utf8("refresh"),
            JsonValue::Object(JsObject::from([(
                JsString::from_utf8("status"),
                JsonValue::String(JsString::from_utf8("idle")),
            )])),
        ),
    ]))
}

fn method<F, Fut>(handler: F) -> ServiceMethod
where
    F: Fn(Vec<JsonValue>, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<JsonValue>, ServiceError>> + Send + 'static,
{
    Arc::new(move |args, context| Box::pin(handler(args, context)))
}

fn member(name: &str) -> JsString {
    JsString::from_utf8(name)
}

fn one_arg(args: &[JsonValue], member_name: &str) -> Result<JsonValue, ServiceError> {
    if args.len() != 1 {
        return Err(invalid_value(format!(
            "{member_name} expects one argument, got {}",
            args.len()
        )));
    }
    args.first()
        .cloned()
        .ok_or_else(|| invalid_value(format!("{member_name} expects one argument")))
}

fn expect_no_args(args: &[JsonValue], member_name: &str) -> Result<(), ServiceError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(invalid_value(format!("{member_name} expects no arguments")))
    }
}

fn parse_thinking_level(value: &JsonValue) -> Result<ModelThinkingLevel, ServiceError> {
    let value = value
        .as_str()
        .ok_or_else(|| invalid_value("selectThinking expects one thinking level string"))?
        .try_to_utf8()
        .map_err(|error| {
            invalid_value(format!(
                "selectThinking argument is not valid UTF-8: {error}"
            ))
        })?;
    match value.as_str() {
        "off" => Ok(ModelThinkingLevel::Off),
        "minimal" => Ok(ModelThinkingLevel::Minimal),
        "low" => Ok(ModelThinkingLevel::Low),
        "medium" => Ok(ModelThinkingLevel::Medium),
        "high" => Ok(ModelThinkingLevel::High),
        "xhigh" => Ok(ModelThinkingLevel::Xhigh),
        "max" => Ok(ModelThinkingLevel::Max),
        _ => Err(invalid_value(
            "selectThinking expects a known thinking level",
        )),
    }
}

fn thinking_level_name(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

fn thinking_level_json(level: ModelThinkingLevel) -> JsonValue {
    JsonValue::String(JsString::from_utf8(thinking_level_name(level)))
}

fn invalid_value(message: impl Into<String>) -> ServiceError {
    ServiceError::remote(RemoteServiceErrorCode::ServiceInvalidValue, message)
}

#[derive(Debug, Error)]
#[error("{0}")]
struct ModelsServiceError(String);

fn source_error(message: impl Into<String>) -> ServiceError {
    ServiceError::handler(ModelsServiceError(message.into()))
}

fn into_service_result<T>(result: Result<T, HarnessError>) -> Result<T, ServiceError> {
    result.map_err(ServiceError::handler)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
