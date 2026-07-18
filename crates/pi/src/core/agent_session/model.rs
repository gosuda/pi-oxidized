//! Model set / cycle / scope / thinking-clamp impls.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/agent-session.ts`
//! `setModel`, `cycleModel`, `setThinkingLevel`, `cycleThinkingLevel`,
//! `getAvailableThinkingLevels`, `supportsThinking`, plus the private helpers
//! `_cycleScopedModel`, `_cycleAvailableModel`, `_getThinkingLevelForModelSwitch`,
//! `_clampThinkingLevel`, and `_emitModelSelect`.
//!
//! Behaviour preserved from the TypeScript contract:
//! - Scoped cycling (`--models`) filters by configured auth and falls back to
//!   the first scoped model when the current model is not in the scoped set.
//! - Available cycling uses the model-runtime auth-configured snapshot.
//! - Both call `set_thinking_level`, which clamps to the new model's supported
//!   set and only persists/emits when the effective level actually changes.
//! - Auth checks are skipped when no model runtime is attached (tests /
//!   pre-runtime builds).
//!
//! Lock order: never hold `AgentSessionInner` across `.await`. The session
//! manager async mutex is acquired for append-only persistence and released
//! before extension emits.

use std::sync::Arc;

use pi_ai::{Model, ModelThinkingLevel, ThinkingLevelMap};

use crate::core::model_runtime::ModelRuntime;
use crate::core::sessions::SessionError;

use super::events::{AgentSessionEvent, ModelSelectSource};
use super::{AgentSession, ScopedModel};

/// Result of [`AgentSession::cycle_model`].
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCycleResult {
    /// Model now active on the agent.
    pub model: Model,
    /// Effective thinking level after clamping to model capabilities.
    pub thinking_level: ModelThinkingLevel,
    /// Whether cycling happened across scoped (`--models`) entries or all
    /// available models.
    pub is_scoped: bool,
}

/// Direction of model cycling (TypeScript `"forward" | "backward"`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CycleDirection {
    /// Advance to the next entry.
    Forward,
    /// Step back to the previous entry.
    Backward,
}

/// Errors returned by [`AgentSession::set_model`].
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// No credential / OAuth configured for the provider.
    #[error("No API key for {0}/{1}")]
    NoAuth(String, String),
    /// Session persistence failed while appending the model-change entry.
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Canonical thinking-level ordering used by `clampThinkingLevel`
/// (TypeScript `EXTENDED_THINKING_LEVELS`).
const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// Index of `level` in [`EXTENDED_THINKING_LEVELS`], or `None` when unknown.
fn extended_level_index(level: ModelThinkingLevel) -> Option<usize> {
    EXTENDED_THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
}

/// Equality by `id` and `provider` (TypeScript `modelsAreEqual`).
fn models_are_equal(a: &Model, b: &Model) -> bool {
    a.id == b.id && a.provider == b.provider
}

/// Levels supported by `model` (TypeScript `getSupportedThinkingLevels`).
///
/// Non-reasoning models support only `Off`. Otherwise levels come from
/// [`Model::thinking_level_map`]; an explicit `None` value marks a level
/// unsupported, while `xhigh` and `max` are included only when explicitly
/// mapped (mirrors the TypeScript filter exactly).
pub(super) fn supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    let Some(map) = model.thinking_level_map.as_ref() else {
        return EXTENDED_THINKING_LEVELS.to_vec();
    };
    EXTENDED_THINKING_LEVELS
        .iter()
        .filter(|level| level_supported_by_map(**level, map))
        .copied()
        .collect()
}

fn level_supported_by_map(level: ModelThinkingLevel, map: &ThinkingLevelMap) -> bool {
    match map.get(&level) {
        // Explicit null → unsupported.
        Some(None) => false,
        // Explicit value → supported.
        Some(Some(_)) => true,
        // Absent: supported except for xhigh/max which require an explicit entry.
        None => !matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max),
    }
}

/// Clamp `level` to a level supported by `model`.
///
/// Walks forward first, then backward through [`EXTENDED_THINKING_LEVELS`]
/// (TypeScript `clampThinkingLevel`).
pub(super) fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = supported_thinking_levels(model);
    let has_mapped_candidates = model
        .thinking_level_map
        .as_ref()
        .is_some_and(|map| map.values().any(Option::is_some));
    let is_clamp_candidate = |candidate: &ModelThinkingLevel| {
        if !available.contains(candidate) {
            return false;
        }
        !has_mapped_candidates
            || model
                .thinking_level_map
                .as_ref()
                .is_some_and(|map| map.get(candidate).is_some_and(Option::is_some))
    };

    if is_clamp_candidate(&level) {
        return level;
    }
    let Some(requested_index) = extended_level_index(level) else {
        return available
            .first()
            .copied()
            .unwrap_or(ModelThinkingLevel::Off);
    };
    for candidate in &EXTENDED_THINKING_LEVELS[requested_index..] {
        if is_clamp_candidate(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if is_clamp_candidate(candidate) {
            return *candidate;
        }
    }
    available
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

impl AgentSession {
    /// Clone the typed model-runtime handle when this session has one.
    ///
    /// Pre-built-agent tests may omit the runtime; model set / cycle methods
    /// treat that as "auth checks skipped".
    pub(super) fn model_runtime(&self) -> Option<Arc<ModelRuntime>> {
        self.model_runtime_handle()
    }

    /// Set the current model.
    ///
    /// Validates auth via the attached runtime (when present), updates agent
    /// state, appends a `model_change` session entry, mutates settings, and
    /// re-clamps the thinking level to the new model's capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NoAuth`] when the runtime reports no configured
    /// credential for the model's provider, or [`ModelError::Session`] when
    /// persistence fails.
    pub async fn set_model(&self, model: Model) -> Result<(), ModelError> {
        if let Some(runtime) = self.model_runtime()
            && runtime.check_auth(&model.provider).await.is_none()
        {
            return Err(ModelError::NoAuth(model.provider.clone(), model.id.clone()));
        }
        let previous = self.model();
        if models_are_equal(&previous, &model) {
            return Ok(());
        }
        let thinking = self.thinking_level_for_model_switch(None);
        self.agent.set_model(model.clone());
        {
            let mut manager = self.session_manager.lock().await;
            manager.append_model_change(&model.provider, &model.id)?;
        }
        self.lock_settings()
            .set_default_model_and_provider(&model.provider, &model.id);
        self.set_thinking_level(thinking).await;
        self.emit_model_select(&model, Some(&previous), ModelSelectSource::Set)
            .await;
        Ok(())
    }

    /// Cycle to the next or previous model.
    ///
    /// Uses scoped models (`--models`) when present, otherwise cycles across
    /// all auth-configured models from the runtime. Returns `None` when only
    /// one candidate is available.
    pub async fn cycle_model(&self, direction: CycleDirection) -> Option<ModelCycleResult> {
        if self.scoped_models().is_empty() {
            self.cycle_available_model(direction).await
        } else {
            self.cycle_scoped_model(direction).await
        }
    }

    async fn cycle_scoped_model(&self, direction: CycleDirection) -> Option<ModelCycleResult> {
        let scoped = self.scoped_models();
        let filtered = self.filter_scoped_by_auth(scoped).await;
        if filtered.len() <= 1 {
            return None;
        }
        let current = self.model();
        let current_index = filtered
            .iter()
            .position(|scoped| models_are_equal(&scoped.model, &current))
            .unwrap_or(0);
        let len = filtered.len();
        let next_index = match direction {
            CycleDirection::Forward => (current_index + 1) % len,
            CycleDirection::Backward => (current_index + len - 1) % len,
        };
        let next = &filtered[next_index];
        let thinking = self.thinking_level_for_model_switch(next.thinking_level);
        self.agent.set_model(next.model.clone());
        {
            let mut manager = self.session_manager.lock().await;
            let _ = manager.append_model_change(&next.model.provider, &next.model.id);
        }
        self.lock_settings()
            .set_default_model_and_provider(&next.model.provider, &next.model.id);
        self.set_thinking_level(thinking).await;
        self.emit_model_select(&next.model, Some(&current), ModelSelectSource::Cycle)
            .await;
        Some(ModelCycleResult {
            model: next.model.clone(),
            thinking_level: self.thinking_level(),
            is_scoped: true,
        })
    }

    async fn cycle_available_model(&self, direction: CycleDirection) -> Option<ModelCycleResult> {
        let runtime = self.model_runtime()?;
        let available = runtime.get_available_snapshot();
        if available.len() <= 1 {
            return None;
        }
        let current = self.model();
        let current_index = available
            .iter()
            .position(|model| models_are_equal(model, &current))
            .unwrap_or(0);
        let len = available.len();
        let next_index = match direction {
            CycleDirection::Forward => (current_index + 1) % len,
            CycleDirection::Backward => (current_index + len - 1) % len,
        };
        let next_model = available[next_index].clone();
        let thinking = self.thinking_level_for_model_switch(None);
        self.agent.set_model(next_model.clone());
        {
            let mut manager = self.session_manager.lock().await;
            let _ = manager.append_model_change(&next_model.provider, &next_model.id);
        }
        self.lock_settings()
            .set_default_model_and_provider(&next_model.provider, &next_model.id);
        self.set_thinking_level(thinking).await;
        self.emit_model_select(&next_model, Some(&current), ModelSelectSource::Cycle)
            .await;
        Some(ModelCycleResult {
            model: next_model,
            thinking_level: self.thinking_level(),
            is_scoped: false,
        })
    }

    /// Filter scoped models by configured auth (TypeScript `_cycleScopedModel`
    /// `auth !== undefined` filter).
    async fn filter_scoped_by_auth(&self, scoped: Vec<ScopedModel>) -> Vec<ScopedModel> {
        let Some(runtime) = self.model_runtime() else {
            return scoped;
        };
        let mut keep = Vec::with_capacity(scoped.len());
        for entry in scoped {
            if runtime.check_auth(&entry.model.provider).await.is_some() {
                keep.push(entry);
            }
        }
        keep
    }

    /// Set the thinking level.
    ///
    /// Clamps to the current model's supported levels. Only persists / emits
    /// when the effective level actually changes. The reference is synchronous
    /// because JavaScript is single-threaded; this Rust port is `async` so it
    /// can append to the session manager (held under a `tokio::Mutex`) and
    /// await extension emits without spawning.
    pub async fn set_thinking_level(&self, level: ModelThinkingLevel) {
        let available = self.available_thinking_levels();
        let effective = if available.contains(&level) {
            level
        } else {
            clamp_thinking_level(&self.model(), level)
        };
        let previous = self.thinking_level();
        if effective == previous {
            return;
        }
        self.agent.set_thinking_level(effective);
        {
            let mut manager = self.session_manager.lock().await;
            let _ = manager.append_thinking_level_change(level_str(effective));
        }
        // Persist as the default only when the model supports reasoning, or the
        // new level is not "off" (TypeScript `supportsThinking() ||
        // effectiveLevel !== "off"`).
        if self.supports_thinking() || effective != ModelThinkingLevel::Off {
            self.lock_settings().set_default_thinking_level(effective);
        }
        self.emit_public(super::events::AgentSessionEvent::ThinkingLevelChanged {
            level: effective,
        });
        let runner = self.hooks.runner();
        let _ = runner
            .emit(super::events::AgentSessionEvent::ThinkingLevelChanged { level: effective })
            .await;
    }

    /// Cycle to the next supported thinking level.
    ///
    /// Returns the new level, or `None` when the current model does not
    /// support reasoning.
    pub async fn cycle_thinking_level(&self) -> Option<ModelThinkingLevel> {
        if !self.supports_thinking() {
            return None;
        }
        let levels = self.available_thinking_levels();
        if levels.is_empty() {
            return None;
        }
        let current = self.thinking_level();
        let current_index = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        let len = levels.len();
        let next_index = (current_index + 1) % len;
        let next_level = levels[next_index];
        self.set_thinking_level(next_level).await;
        Some(next_level)
    }

    /// Supported thinking levels for the current model.
    #[must_use]
    pub fn available_thinking_levels(&self) -> Vec<ModelThinkingLevel> {
        supported_thinking_levels(&self.model())
    }

    /// Whether the current model supports reasoning.
    #[must_use]
    pub fn supports_thinking(&self) -> bool {
        self.model().reasoning
    }

    /// Resolve the thinking level to apply when switching models.
    ///
    /// - Explicit `scoped_level` (from `--models`) always wins.
    /// - Otherwise, when the current model does not support reasoning, fall
    ///   back to the settings default then `Medium` (TypeScript
    ///   `DEFAULT_THINKING_LEVEL`).
    /// - Otherwise inherit the current session thinking level.
    pub(super) fn thinking_level_for_model_switch(
        &self,
        scoped_level: Option<ModelThinkingLevel>,
    ) -> ModelThinkingLevel {
        if let Some(explicit) = scoped_level {
            return explicit;
        }
        if !self.supports_thinking() {
            return self
                .lock_settings()
                .get_default_thinking_level()
                .unwrap_or(ModelThinkingLevel::Medium);
        }
        self.thinking_level()
    }

    /// Emit `model_select` to extensions when the model actually changed.
    async fn emit_model_select(
        &self,
        next_model: &Model,
        previous_model: Option<&Model>,
        source: ModelSelectSource,
    ) {
        if matches!(previous_model, Some(prev) if models_are_equal(prev, next_model)) {
            return;
        }
        let runner = self.hooks.runner();
        if !runner.has_handlers("model_select") {
            return;
        }
        if let Err(error) = runner
            .emit(AgentSessionEvent::ModelSelect {
                model: Box::new(next_model.clone()),
                previous_model: previous_model.map(|model| Box::new(model.clone())),
                source,
            })
            .await
        {
            runner.emit_error(error.to_string());
        }
    }
}

/// Wire string for a thinking level (matches TypeScript `ThinkingLevel` union).
fn level_str(level: ModelThinkingLevel) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, provider: &str, reasoning: bool) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api: "test-api".to_owned(),
            provider: provider.to_owned(),
            base_url: String::new(),
            reasoning,
            thinking_level_map: None,
            input: vec![pi_ai::ModelInput::Text],
            cost: pi_ai::ModelCost::default(),
            context_window: 8_192,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn supported_levels_non_reasoning_is_off_only() {
        let levels = supported_thinking_levels(&model("a", "p", false));
        assert_eq!(levels, vec![ModelThinkingLevel::Off]);
    }

    #[test]
    fn supported_levels_reasoning_without_map_is_full_set() {
        let levels = supported_thinking_levels(&model("a", "p", true));
        assert_eq!(levels, EXTENDED_THINKING_LEVELS.to_vec());
    }

    #[test]
    fn clamp_high_to_non_reasoning_is_off() {
        let m = model("a", "p", false);
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::High),
            ModelThinkingLevel::Off
        );
    }

    #[test]
    fn clamp_walks_forward_then_backward() {
        // Map: off and high supported; requested medium.
        // Forward from medium: high is supported → high.
        let mut m = model("a", "p", true);
        let mut map = ThinkingLevelMap::new();
        map.insert(ModelThinkingLevel::Off, None);
        map.insert(ModelThinkingLevel::High, Some("high".to_owned()));
        m.thinking_level_map = Some(map);
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::Medium),
            ModelThinkingLevel::High
        );
    }

    #[test]
    fn clamp_walks_backward_when_no_higher_supported() {
        // Map: off and low supported; requested medium.
        // Forward from medium: high/xhigh/max absent → none.
        // Backward from medium-1=low: low supported → low.
        let mut m = model("a", "p", true);
        let mut map = ThinkingLevelMap::new();
        map.insert(ModelThinkingLevel::Off, None);
        map.insert(ModelThinkingLevel::Low, Some("low".to_owned()));
        m.thinking_level_map = Some(map);
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::Medium),
            ModelThinkingLevel::Low
        );
    }

    #[test]
    fn models_are_equal_checks_id_and_provider() {
        let a = model("a", "p", false);
        let b = model("a", "p", true);
        let c = model("a", "q", false);
        assert!(models_are_equal(&a, &b), "id+provider match");
        assert!(!models_are_equal(&a, &c), "provider differs");
    }

    #[test]
    fn xhigh_max_require_explicit_map_entry() {
        let mut m = model("a", "p", true);
        let mut map = ThinkingLevelMap::new();
        // Off + low present; xhigh absent → not supported.
        map.insert(ModelThinkingLevel::Off, None);
        map.insert(ModelThinkingLevel::Low, Some("low".to_owned()));
        m.thinking_level_map = Some(map);
        let levels = supported_thinking_levels(&m);
        assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(!levels.contains(&ModelThinkingLevel::Max));
        // Add xhigh explicitly → supported.
        let Some(map_ref) = m.thinking_level_map.as_mut() else {
            unreachable!();
        };
        map_ref.insert(ModelThinkingLevel::Xhigh, Some("xhigh".to_owned()));
        let levels = supported_thinking_levels(&m);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn explicit_null_disables_level() {
        let mut m = model("a", "p", true);
        let mut map = ThinkingLevelMap::new();
        map.insert(ModelThinkingLevel::Off, None);
        map.insert(ModelThinkingLevel::Medium, None);
        m.thinking_level_map = Some(map);
        let levels = supported_thinking_levels(&m);
        assert!(!levels.contains(&ModelThinkingLevel::Medium));
    }

    #[test]
    fn level_str_matches_wire_union() {
        assert_eq!(level_str(ModelThinkingLevel::Off), "off");
        assert_eq!(level_str(ModelThinkingLevel::Medium), "medium");
        assert_eq!(level_str(ModelThinkingLevel::Xhigh), "xhigh");
        assert_eq!(level_str(ModelThinkingLevel::Max), "max");
    }
}
