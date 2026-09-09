//! Model catalogue and selection service contracts.
//!
//! Source mapping: `experimental/services/models.ts`. Model resolution and
//! state publication remain provider-owned; this module only carries the
//! source-shaped payloads.

use std::collections::BTreeMap;

use pi_agent::service::error::ServiceError;
use pi_agent::service::value::{JsObject, JsString, JsonValue};
use pi_ai::types::ModelThinkingLevel;

use super::{
    ProductJsonConvert, array, bool_value, integer, invalid, json_object, nullable, object,
    required, string,
};

/// Chord service identifier for model catalogue and selection.
pub const MODELS_ID: &str = "pi.models";
/// Replicated state member name.
pub const MODELS_STATE_MEMBER: &str = "state";
/// Wire method that asks `pi.models` to cycle the selected model's thinking level.
pub const MODELS_CYCLE_THINKING_MEMBER: &str = "cycleThinking";
/// Wire method that asks `pi.models` for thinking levels supported by the active model.
pub const MODELS_GET_THINKING_LEVELS_MEMBER: &str = "getThinkingLevels";
/// Wire method that asks `pi.models` to refresh its model catalogue.
pub const MODELS_REFRESH_MEMBER: &str = "refresh";
/// Wire method that asks `pi.models` to select a provider-scoped model.
pub const MODELS_SELECT_MEMBER: &str = "select";
/// Wire method that asks `pi.models` to select the active model's thinking level.
pub const MODELS_SELECT_THINKING_MEMBER: &str = "selectThinking";

/// Provider-scoped model identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRef {
    /// Provider key.
    pub provider: String,
    /// Provider-scoped model identifier.
    pub model_id: String,
}

/// One model in the available catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSummary {
    /// Provider key.
    pub provider: String,
    /// Provider-scoped model identifier.
    pub model_id: String,
    /// Display name.
    pub name: String,
    /// Whether the model supports reasoning.
    pub reasoning: bool,
}

/// Replicated model catalogue state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelsCatalog {
    /// Source binary64 catalogue revision.
    pub revision: pi_agent::service::value::JsInteger,
    /// Available models.
    pub available_models: Vec<ModelSummary>,
}

/// Current model and thinking-level configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelsConfiguration {
    /// Selected model, or `null` when none is selected.
    pub model: Option<ModelRef>,
    /// Current thinking level.
    pub thinking_level: ModelThinkingLevel,
}

/// Refresh lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelsRefreshState {
    /// No refresh has started.
    Idle,
    /// A refresh is in progress.
    Refreshing,
    /// The last refresh completed without reported errors.
    Done,
    /// The last refresh completed with provider errors.
    Warning {
        /// Provider/model error messages keyed by source id.
        errors: BTreeMap<String, String>,
    },
}

/// Complete replicated state of the model service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelsState {
    /// Model catalogue and revision.
    pub catalog: ModelsCatalog,
    /// Active configuration.
    pub configuration: ModelsConfiguration,
    /// Refresh status.
    pub refresh: ModelsRefreshState,
}

impl ProductJsonConvert for ModelRef {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "model reference")?;
        let provider = string(
            required(fields, "provider", "model reference.provider")?,
            "model reference.provider",
        )?;
        let model_id = string(
            required(fields, "modelId", "model reference.modelId")?,
            "model reference.modelId",
        )?;
        Ok(Self { provider, model_id })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([
            ("provider", JsonValue::String(self.provider.into())),
            ("modelId", JsonValue::String(self.model_id.into())),
        ]))
    }
}

impl ProductJsonConvert for ModelSummary {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "model summary")?;
        let identity = ModelRef::from_json(value.clone())?;
        let name = string(
            required(fields, "name", "model summary.name")?,
            "model summary.name",
        )?;
        let reasoning = bool_value(
            required(fields, "reasoning", "model summary.reasoning")?,
            "model summary.reasoning",
        )?;
        Ok(Self {
            provider: identity.provider,
            model_id: identity.model_id,
            name,
            reasoning,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([
            ("provider", JsonValue::String(self.provider.into())),
            ("modelId", JsonValue::String(self.model_id.into())),
            ("name", JsonValue::String(self.name.into())),
            ("reasoning", JsonValue::Bool(self.reasoning)),
        ]))
    }
}

impl ProductJsonConvert for ModelsCatalog {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "models catalog")?;
        let revision = integer(
            required(fields, "revision", "models catalog.revision")?,
            "models catalog.revision",
        )?;
        let available_models = array(
            required(fields, "availableModels", "models catalog.availableModels")?,
            "models catalog.availableModels",
        )?
        .iter()
        .cloned()
        .map(ModelSummary::from_json)
        .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            revision,
            available_models,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let available_models = self
            .available_models
            .into_iter()
            .map(ModelSummary::into_json)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json_object([
            ("revision", JsonValue::Number(self.revision.as_f64())),
            ("availableModels", JsonValue::Array(available_models)),
        ]))
    }
}

impl ProductJsonConvert for ModelsConfiguration {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "models configuration")?;
        let model = nullable(
            required(fields, "model", "models configuration.model")?,
            |value| ModelRef::from_json(value.clone()),
        )?;
        let thinking_level = thinking_level(required(
            fields,
            "thinkingLevel",
            "models configuration.thinkingLevel",
        )?)?;
        Ok(Self {
            model,
            thinking_level,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let model = match self.model {
            Some(model) => model.into_json()?,
            None => JsonValue::Null,
        };
        Ok(json_object([
            ("model", model),
            ("thinkingLevel", thinking_level_json(self.thinking_level)),
        ]))
    }
}

impl ProductJsonConvert for ModelsRefreshState {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "models refresh state")?;
        let status = string(
            required(fields, "status", "models refresh state.status")?,
            "models refresh state.status",
        )?;
        match status.as_str() {
            "idle" => Ok(Self::Idle),
            "refreshing" => Ok(Self::Refreshing),
            "done" => Ok(Self::Done),
            "warning" => {
                let error_fields = object(
                    required(fields, "errors", "models refresh state.errors")?,
                    "models refresh state.errors",
                )?;
                let mut errors = BTreeMap::new();
                for (key, value) in error_fields {
                    let key = key.try_to_utf8().map_err(|error| {
                        invalid(format!("models refresh state.errors key: {error}"))
                    })?;
                    errors.insert(key, string(value, "models refresh state.errors value")?);
                }
                Ok(Self::Warning { errors })
            }
            _ => Err(invalid("models refresh state.status")),
        }
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        match self {
            Self::Idle => Ok(json_object([("status", JsonValue::String("idle".into()))])),
            Self::Refreshing => Ok(json_object([(
                "status",
                JsonValue::String("refreshing".into()),
            )])),
            Self::Done => Ok(json_object([("status", JsonValue::String("done".into()))])),
            Self::Warning { errors } => {
                let mut error_fields = JsObject::new();
                for (key, value) in errors {
                    error_fields.insert(JsString::from_utf8(&key), JsonValue::String(value.into()));
                }
                Ok(json_object([
                    ("status", JsonValue::String("warning".into())),
                    ("errors", JsonValue::Object(error_fields)),
                ]))
            }
        }
    }
}

impl ProductJsonConvert for ModelsState {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "models state")?;
        let catalog =
            ModelsCatalog::from_json(required(fields, "catalog", "models state.catalog")?.clone())?;
        let configuration = ModelsConfiguration::from_json(
            required(fields, "configuration", "models state.configuration")?.clone(),
        )?;
        let refresh = ModelsRefreshState::from_json(
            required(fields, "refresh", "models state.refresh")?.clone(),
        )?;
        Ok(Self {
            catalog,
            configuration,
            refresh,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([
            ("catalog", self.catalog.into_json()?),
            ("configuration", self.configuration.into_json()?),
            ("refresh", self.refresh.into_json()?),
        ]))
    }
}

fn thinking_level(value: &JsonValue) -> Result<ModelThinkingLevel, ServiceError> {
    let value = string(value, "models thinkingLevel")?;
    match value.as_str() {
        "off" => Ok(ModelThinkingLevel::Off),
        "minimal" => Ok(ModelThinkingLevel::Minimal),
        "low" => Ok(ModelThinkingLevel::Low),
        "medium" => Ok(ModelThinkingLevel::Medium),
        "high" => Ok(ModelThinkingLevel::High),
        "xhigh" => Ok(ModelThinkingLevel::Xhigh),
        "max" => Ok(ModelThinkingLevel::Max),
        _ => Err(invalid("models thinkingLevel")),
    }
}

fn thinking_level_json(value: ModelThinkingLevel) -> JsonValue {
    let name = match value {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    };
    JsonValue::String(name.into())
}
