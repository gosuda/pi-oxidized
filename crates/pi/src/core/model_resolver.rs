//! Model resolution, scoping, and CLI/model-pattern matching.
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/core/model-resolver.ts`.
//! Initial-model priority and session restore live in
//! [`crate::core::agent_session_services`] and are re-exported here so entry
//! and callers share one implementation.

use std::cmp::Ordering;
use std::collections::HashMap;

use globset::GlobBuilder;
use pi_ai::types::{Model, ModelThinkingLevel};

use super::agent_session_services::{
    DEFAULT_THINKING_LEVEL, FindInitialModelOptions, InitialModelResult, ScopedModel,
    default_model_per_provider, find_initial_model,
};
use super::model_runtime::ModelRuntime;

// Re-export session restore so entry can import resolver + restore from one module.
pub use super::agent_session_services::restore_model_from_session;

/// Result of parsing a model pattern (optional thinking suffix).
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedModelResult {
    /// Matched model, when any.
    pub model: Option<Model>,
    /// Thinking level when the pattern specified a valid one.
    pub thinking_level: Option<ModelThinkingLevel>,
    /// Non-fatal warning (invalid thinking suffix in scope mode).
    pub warning: Option<String>,
}

/// Options for [`parse_model_pattern`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ParseModelPatternOptions {
    /// When `false` (CLI `--model`), an invalid `:suffix` fails the match
    /// instead of warning and falling back. Defaults to `true` (scope mode).
    pub allow_invalid_thinking_level_fallback: Option<bool>,
}

/// Structured diagnostic from model-scope resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelScopeDiagnostic {
    /// Always `"warning"` for the TypeScript contract.
    pub kind: ModelScopeDiagnosticKind,
    /// Human-readable message (exact TypeScript wording).
    pub message: String,
    /// Pattern that produced the diagnostic.
    pub pattern: String,
}

/// Diagnostic severity for scope resolution (currently only warning).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelScopeDiagnosticKind {
    /// Non-fatal scope warning.
    Warning,
}

/// Result of [`resolve_model_scope_with_diagnostics`].
#[derive(Clone, Debug, PartialEq)]
pub struct ResolveModelScopeResult {
    /// Deduplicated scoped models in pattern order.
    pub scoped_models: Vec<ScopedModel>,
    /// Warnings for invalid thinking suffixes / unmatched patterns.
    pub diagnostics: Vec<ModelScopeDiagnostic>,
}

/// Result of [`resolve_cli_model`].
#[derive(Clone, Debug, PartialEq)]
pub struct ResolveCliModelResult {
    /// Resolved model, when successful.
    pub model: Option<Model>,
    /// Thinking level parsed from `<pattern>:<level>` when present.
    pub thinking_level: Option<ModelThinkingLevel>,
    /// Non-fatal warning (e.g. custom-model fallback notice).
    pub warning: Option<String>,
    /// Hard failure message; when set, `model` is `None`.
    pub error: Option<String>,
}

/// Options for [`resolve_cli_model`].
#[derive(Clone, Copy, Debug)]
pub struct ResolveCliModelOptions<'a> {
    /// `--provider` value.
    pub cli_provider: Option<&'a str>,
    /// `--model` value.
    pub cli_model: Option<&'a str>,
    /// Explicit `--thinking` (suppresses fallback suffix stripping).
    pub cli_thinking: Option<ModelThinkingLevel>,
    /// Model/auth runtime.
    pub model_runtime: &'a ModelRuntime,
}

/// Options for the TypeScript-shaped [`find_initial_model_full`] helper.
#[derive(Clone, Copy, Debug)]
pub struct FindInitialModelFullOptions<'a> {
    /// `--provider` (CLI path requires both provider and model).
    pub cli_provider: Option<&'a str>,
    /// `--model`.
    pub cli_model: Option<&'a str>,
    /// Scoped models for cycling.
    pub scoped_models: &'a [ScopedModel],
    /// Whether a session is being continued/resumed.
    pub is_continuing: bool,
    /// Settings default provider.
    pub default_provider: Option<&'a str>,
    /// Settings default model id.
    pub default_model_id: Option<&'a str>,
    /// Settings default thinking level.
    pub default_thinking_level: Option<ModelThinkingLevel>,
    /// Model runtime.
    pub model_runtime: &'a ModelRuntime,
}

/// Whether a model id looks like an alias (no `-YYYYMMDD` date suffix).
#[must_use]
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    // Date pattern: -YYYYMMDD at end of id.
    if id.len() >= 9 {
        let bytes = id.as_bytes();
        let start = bytes.len() - 9;
        if bytes[start] == b'-' {
            return !bytes[start + 1..].iter().all(u8::is_ascii_digit);
        }
    }
    true
}

/// Equality by `id` and `provider` (TypeScript `modelsAreEqual`).
#[must_use]
fn models_are_equal(a: &Model, b: &Model) -> bool {
    a.id == b.id && a.provider == b.provider
}

/// Parse a thinking-level token (`off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`).
#[must_use]
pub fn is_valid_thinking_level(level: &str) -> bool {
    parse_thinking_level(level).is_some()
}

#[must_use]
fn parse_thinking_level(level: &str) -> Option<ModelThinkingLevel> {
    match level {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Find an exact model reference match.
///
/// Supports a bare model id or a canonical `provider/modelId` reference.
/// When matching by bare id, ambiguous matches across providers are rejected.
#[must_use]
pub fn find_exact_model_reference_match(
    model_reference: &str,
    available_models: &[Model],
) -> Option<Model> {
    let trimmed = model_reference.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_ascii_lowercase();

    let mut canonical_matches = available_models.iter().filter(|model| {
        format!("{}/{}", model.provider, model.id).to_ascii_lowercase() == normalized
    });
    match (canonical_matches.next(), canonical_matches.next()) {
        (Some(only), None) => return Some(only.clone()),
        (Some(_), Some(_)) => return None,
        (None, _) => {}
    }

    if let Some(slash_index) = trimmed.find('/') {
        let provider = trimmed[..slash_index].trim();
        let model_id = trimmed[slash_index + 1..].trim();
        if !provider.is_empty() && !model_id.is_empty() {
            let mut provider_matches = available_models.iter().filter(|model| {
                model.provider.eq_ignore_ascii_case(provider)
                    && model.id.eq_ignore_ascii_case(model_id)
            });
            match (provider_matches.next(), provider_matches.next()) {
                (Some(only), None) => return Some(only.clone()),
                (Some(_), Some(_)) => return None,
                (None, _) => {}
            }
        }
    }

    let mut id_matches = available_models
        .iter()
        .filter(|model| model.id.eq_ignore_ascii_case(&normalized));
    match (id_matches.next(), id_matches.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

/// Try exact then fuzzy (partial id/name) match with alias/date preference.
#[must_use]
fn try_match_model(model_pattern: &str, available_models: &[Model]) -> Option<Model> {
    if let Some(exact) = find_exact_model_reference_match(model_pattern, available_models) {
        return Some(exact);
    }

    let needle = model_pattern.to_ascii_lowercase();
    let mut matches: Vec<&Model> = available_models
        .iter()
        .filter(|model| {
            model.id.to_ascii_lowercase().contains(&needle)
                || model.name.to_ascii_lowercase().contains(&needle)
        })
        .collect();

    if matches.is_empty() {
        return None;
    }

    let mut aliases: Vec<&Model> = matches
        .iter()
        .copied()
        .filter(|model| is_alias(&model.id))
        .collect();
    if !aliases.is_empty() {
        aliases.sort_by(|a, b| cmp_id_desc(&a.id, &b.id));
        return aliases.first().map(|model| (*model).clone());
    }

    matches.retain(|model| !is_alias(&model.id));
    matches.sort_by(|a, b| cmp_id_desc(&a.id, &b.id));
    matches.first().map(|model| (*model).clone())
}

/// Descending string compare approximating JS `localeCompare` for model ids.
fn cmp_id_desc(a: &str, b: &str) -> Ordering {
    b.cmp(a)
}

fn build_fallback_model(
    provider: &str,
    model_id: &str,
    available_models: &[Model],
) -> Option<Model> {
    let provider_models: Vec<&Model> = available_models
        .iter()
        .filter(|model| model.provider == provider)
        .collect();
    if provider_models.is_empty() {
        return None;
    }

    let default_id = default_model_per_provider()
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, id)| *id);

    let base = default_id
        .and_then(|default_id| {
            provider_models
                .iter()
                .find(|model| model.id == default_id)
                .copied()
        })
        .unwrap_or(provider_models[0]);
    let mut model = base.clone();
    model_id.clone_into(&mut model.id);
    model_id.clone_into(&mut model.name);
    Some(model)
}

/// Parse a pattern to extract model and optional thinking level.
///
/// Handles models with colons in their IDs (`OpenRouter` exacto-style suffixes).
#[must_use]
pub fn parse_model_pattern(
    pattern: &str,
    available_models: &[Model],
    options: ParseModelPatternOptions,
) -> ParsedModelResult {
    if let Some(exact_match) = try_match_model(pattern, available_models) {
        return ParsedModelResult {
            model: Some(exact_match),
            thinking_level: None,
            warning: None,
        };
    }

    let Some(last_colon_index) = pattern.rfind(':') else {
        return ParsedModelResult {
            model: None,
            thinking_level: None,
            warning: None,
        };
    };

    let prefix = &pattern[..last_colon_index];
    let suffix = &pattern[last_colon_index + 1..];

    if let Some(level) = parse_thinking_level(suffix) {
        let result = parse_model_pattern(prefix, available_models, options);
        if result.model.is_some() {
            return ParsedModelResult {
                model: result.model,
                thinking_level: if result.warning.is_some() {
                    None
                } else {
                    Some(level)
                },
                warning: result.warning,
            };
        }
        return result;
    }

    let allow_fallback = options
        .allow_invalid_thinking_level_fallback
        .unwrap_or(true);
    if !allow_fallback {
        return ParsedModelResult {
            model: None,
            thinking_level: None,
            warning: None,
        };
    }

    let result = parse_model_pattern(prefix, available_models, options);
    if result.model.is_some() {
        return ParsedModelResult {
            model: result.model,
            thinking_level: None,
            warning: Some(format!(
                "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
            )),
        };
    }
    result
}

fn pattern_has_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

fn glob_matches(candidate: &str, pattern: &str) -> bool {
    let Ok(glob) = GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
    else {
        return false;
    };
    glob.compile_matcher().is_match(candidate)
}

/// Resolve model patterns to scoped models plus structured diagnostics.
pub async fn resolve_model_scope_with_diagnostics(
    patterns: &[String],
    model_runtime: &ModelRuntime,
) -> ResolveModelScopeResult {
    let available = model_runtime.get_available(None).await.unwrap_or_default();
    resolve_model_scope_from_models(patterns, &available)
}

/// Pure scope resolution over an already-fetched model list (tests / offline).
#[must_use]
pub fn resolve_model_scope_from_models(
    patterns: &[String],
    available_models: &[Model],
) -> ResolveModelScopeResult {
    let mut scoped_models: Vec<ScopedModel> = Vec::new();
    let mut diagnostics: Vec<ModelScopeDiagnostic> = Vec::new();

    for pattern in patterns {
        if pattern_has_glob(pattern) {
            let mut glob_pattern = pattern.as_str();
            let mut thinking_level = None;
            if let Some(colon_idx) = pattern.rfind(':') {
                let suffix = &pattern[colon_idx + 1..];
                if let Some(level) = parse_thinking_level(suffix) {
                    thinking_level = Some(level);
                    glob_pattern = &pattern[..colon_idx];
                }
            }

            let matching: Vec<&Model> = available_models
                .iter()
                .filter(|model| {
                    let full_id = format!("{}/{}", model.provider, model.id);
                    glob_matches(&full_id, glob_pattern) || glob_matches(&model.id, glob_pattern)
                })
                .collect();

            if matching.is_empty() {
                diagnostics.push(ModelScopeDiagnostic {
                    kind: ModelScopeDiagnosticKind::Warning,
                    message: format!("No models match pattern \"{pattern}\""),
                    pattern: pattern.clone(),
                });
                continue;
            }

            for model in matching {
                if !scoped_models
                    .iter()
                    .any(|scoped| models_are_equal(&scoped.model, model))
                {
                    scoped_models.push(ScopedModel {
                        model: model.clone(),
                        thinking_level,
                    });
                }
            }
            continue;
        }

        let parsed = parse_model_pattern(
            pattern,
            available_models,
            ParseModelPatternOptions {
                allow_invalid_thinking_level_fallback: Some(true),
            },
        );

        if let Some(warning) = parsed.warning {
            diagnostics.push(ModelScopeDiagnostic {
                kind: ModelScopeDiagnosticKind::Warning,
                message: warning,
                pattern: pattern.clone(),
            });
        }

        let Some(model) = parsed.model else {
            diagnostics.push(ModelScopeDiagnostic {
                kind: ModelScopeDiagnosticKind::Warning,
                message: format!("No models match pattern \"{pattern}\""),
                pattern: pattern.clone(),
            });
            continue;
        };

        if !scoped_models
            .iter()
            .any(|scoped| models_are_equal(&scoped.model, &model))
        {
            scoped_models.push(ScopedModel {
                model,
                thinking_level: parsed.thinking_level,
            });
        }
    }

    ResolveModelScopeResult {
        scoped_models,
        diagnostics,
    }
}

/// Resolve model patterns to scoped models (diagnostics discarded).
///
/// TypeScript prints diagnostics with `console.warn`; callers that need
/// messages should use [`resolve_model_scope_with_diagnostics`].
pub async fn resolve_model_scope(
    patterns: &[String],
    model_runtime: &ModelRuntime,
) -> Vec<ScopedModel> {
    resolve_model_scope_with_diagnostics(patterns, model_runtime)
        .await
        .scoped_models
}

/// Resolve a single model from CLI flags (`--provider` / `--model` / optional thinking).
#[must_use]
pub fn resolve_cli_model(options: ResolveCliModelOptions<'_>) -> ResolveCliModelResult {
    resolve_cli_model_from(
        options.cli_provider,
        options.cli_model,
        options.cli_thinking,
        &options.model_runtime.get_models(None),
        |provider| options.model_runtime.has_configured_auth(provider),
    )
}

/// Pure CLI resolution over an explicit model list and auth probe.
#[must_use]
pub fn resolve_cli_model_from(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ModelThinkingLevel>,
    available_models: &[Model],
    has_configured_auth: impl Fn(&str) -> bool,
) -> ResolveCliModelResult {
    let Some(cli_model) = cli_model else {
        return empty_cli_result();
    };
    if available_models.is_empty() {
        return no_models_cli_result();
    }

    let provider_map = build_provider_map(available_models);
    let (provider, mut pattern, inferred_provider) =
        infer_provider_and_pattern(cli_provider, cli_model, &provider_map);

    if cli_provider.is_some() && provider.is_none() {
        return unknown_provider_cli_result(cli_provider);
    }

    if provider.is_none()
        && let Some(exact) = find_exact_cli_reference(cli_model, available_models)
    {
        return exact_cli_result(exact);
    }

    strip_explicit_provider_prefix(cli_provider, provider.as_deref(), cli_model, &mut pattern);

    let candidate_owned = filter_candidates(provider.as_deref(), available_models);
    let parsed = parse_model_pattern(
        &pattern,
        &candidate_owned,
        ParseModelPatternOptions {
            allow_invalid_thinking_level_fallback: Some(false),
        },
    );

    if let Some(model) = parsed.model {
        if let Some(preferred) = prefer_authenticated_raw_id(
            inferred_provider,
            cli_model,
            &model,
            available_models,
            &has_configured_auth,
        ) {
            return preferred;
        }
        return ResolveCliModelResult {
            model: Some(model),
            thinking_level: parsed.thinking_level,
            warning: parsed.warning,
            error: None,
        };
    }

    if let Some(result) =
        try_inferred_provider_fallback(inferred_provider, cli_model, available_models)
    {
        return result;
    }

    if let Some(provider_name) = provider.as_deref()
        && let Some(result) = fallback_custom_model(
            provider_name,
            &pattern,
            cli_thinking,
            available_models,
            parsed.warning.as_deref(),
        )
    {
        return result;
    }

    not_found_cli_result(provider.as_deref(), &pattern, cli_model, parsed.warning)
}

fn empty_cli_result() -> ResolveCliModelResult {
    ResolveCliModelResult {
        model: None,
        thinking_level: None,
        warning: None,
        error: None,
    }
}

fn no_models_cli_result() -> ResolveCliModelResult {
    ResolveCliModelResult {
        model: None,
        thinking_level: None,
        warning: None,
        error: Some(
            "No models available. Check your installation or add models to models.json.".to_owned(),
        ),
    }
}

fn unknown_provider_cli_result(cli_provider: Option<&str>) -> ResolveCliModelResult {
    ResolveCliModelResult {
        model: None,
        thinking_level: None,
        warning: None,
        error: Some(format!(
            "Unknown provider \"{}\". Use --list-models to see available providers/models.",
            cli_provider.unwrap_or_default()
        )),
    }
}

fn exact_cli_result(model: Model) -> ResolveCliModelResult {
    ResolveCliModelResult {
        model: Some(model),
        thinking_level: None,
        warning: None,
        error: None,
    }
}

fn not_found_cli_result(
    provider: Option<&str>,
    pattern: &str,
    cli_model: &str,
    warning: Option<String>,
) -> ResolveCliModelResult {
    let display = if let Some(provider_name) = provider {
        format!("{provider_name}/{pattern}")
    } else {
        cli_model.to_owned()
    };
    ResolveCliModelResult {
        model: None,
        thinking_level: None,
        warning,
        error: Some(format!(
            "Model \"{display}\" not found. Use --list-models to see available models."
        )),
    }
}

fn infer_provider_and_pattern(
    cli_provider: Option<&str>,
    cli_model: &str,
    provider_map: &HashMap<String, String>,
) -> (Option<String>, String, bool) {
    let mut provider =
        cli_provider.and_then(|value| provider_map.get(&value.to_ascii_lowercase()).cloned());
    let mut pattern = cli_model.to_owned();
    let mut inferred_provider = false;
    if let Some(slash_index) = cli_model.find('/')
        && provider.is_none()
    {
        let maybe_provider = &cli_model[..slash_index];
        if let Some(canonical) = provider_map.get(&maybe_provider.to_ascii_lowercase()) {
            provider = Some(canonical.clone());
            cli_model[slash_index + 1..].clone_into(&mut pattern);
            inferred_provider = true;
        }
    }
    (provider, pattern, inferred_provider)
}

fn strip_explicit_provider_prefix(
    cli_provider: Option<&str>,
    provider: Option<&str>,
    cli_model: &str,
    pattern: &mut String,
) {
    if let (Some(_cli_provider), Some(provider_name)) = (cli_provider, provider) {
        let prefix = format!("{provider_name}/");
        if cli_model
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
        {
            cli_model[prefix.len()..].clone_into(pattern);
        }
    }
}

fn try_inferred_provider_fallback(
    inferred_provider: bool,
    cli_model: &str,
    available_models: &[Model],
) -> Option<ResolveCliModelResult> {
    if !inferred_provider {
        return None;
    }
    if let Some(exact) = find_exact_cli_reference(cli_model, available_models) {
        return Some(exact_cli_result(exact));
    }
    let fallback = parse_model_pattern(
        cli_model,
        available_models,
        ParseModelPatternOptions {
            allow_invalid_thinking_level_fallback: Some(false),
        },
    );
    fallback.model.map(|model| ResolveCliModelResult {
        model: Some(model),
        thinking_level: fallback.thinking_level,
        warning: fallback.warning,
        error: None,
    })
}

fn build_provider_map(available_models: &[Model]) -> HashMap<String, String> {
    let mut provider_map = HashMap::new();
    for model in available_models {
        provider_map
            .entry(model.provider.to_ascii_lowercase())
            .or_insert_with(|| model.provider.clone());
    }
    provider_map
}

fn find_exact_cli_reference(cli_model: &str, available_models: &[Model]) -> Option<Model> {
    let lower = cli_model.to_ascii_lowercase();
    available_models
        .iter()
        .find(|model| {
            model.id.eq_ignore_ascii_case(&lower)
                || format!("{}/{}", model.provider, model.id).eq_ignore_ascii_case(&lower)
        })
        .cloned()
}

fn filter_candidates(provider: Option<&str>, available_models: &[Model]) -> Vec<Model> {
    match provider {
        Some(provider_name) => available_models
            .iter()
            .filter(|model| model.provider == provider_name)
            .cloned()
            .collect(),
        None => available_models.to_vec(),
    }
}

fn prefer_authenticated_raw_id(
    inferred_provider: bool,
    cli_model: &str,
    model: &Model,
    available_models: &[Model],
    has_configured_auth: &impl Fn(&str) -> bool,
) -> Option<ResolveCliModelResult> {
    if !inferred_provider {
        return None;
    }
    let raw_exact: Vec<&Model> = available_models
        .iter()
        .filter(|candidate| {
            candidate.id.eq_ignore_ascii_case(cli_model) && !models_are_equal(candidate, model)
        })
        .collect();
    if raw_exact.is_empty() || has_configured_auth(&model.provider) {
        return None;
    }
    let authenticated_raw: Vec<&Model> = raw_exact
        .into_iter()
        .filter(|candidate| has_configured_auth(&candidate.provider))
        .collect();
    if authenticated_raw.len() == 1 {
        return Some(ResolveCliModelResult {
            model: Some(authenticated_raw[0].clone()),
            thinking_level: None,
            warning: None,
            error: None,
        });
    }
    None
}

fn fallback_custom_model(
    provider_name: &str,
    pattern: &str,
    cli_thinking: Option<ModelThinkingLevel>,
    available_models: &[Model],
    warning: Option<&str>,
) -> Option<ResolveCliModelResult> {
    let mut fallback_pattern = pattern;
    let mut fallback_thinking = None;
    if cli_thinking.is_none()
        && let Some(last_colon) = pattern.rfind(':')
    {
        let suffix = &pattern[last_colon + 1..];
        if let Some(level) = parse_thinking_level(suffix) {
            fallback_pattern = &pattern[..last_colon];
            fallback_thinking = Some(level);
        }
    }

    let mut fallback_model =
        build_fallback_model(provider_name, fallback_pattern, available_models)?;
    let requested_thinking = cli_thinking.or(fallback_thinking);
    if requested_thinking.is_some_and(|level| level != ModelThinkingLevel::Off) {
        fallback_model.reasoning = true;
    }
    let fallback_warning = if let Some(warning) = warning {
        format!(
            "{warning} Model \"{fallback_pattern}\" not found for provider \"{provider_name}\". Using custom model id."
        )
    } else {
        format!(
            "Model \"{fallback_pattern}\" not found for provider \"{provider_name}\". Using custom model id."
        )
    };
    Some(ResolveCliModelResult {
        model: Some(fallback_model),
        thinking_level: fallback_thinking,
        warning: Some(fallback_warning),
        error: None,
    })
}

/// TypeScript `findInitialModel` shape: optional CLI provider/model strings,
/// then the shared services priority path.
///
/// CLI hard errors are returned as `Err` (TypeScript `process.exit(1)`).
///
/// # Errors
///
/// Returns the CLI hard-error string when `resolve_cli_model` fails (unknown
/// provider, empty catalog, or unresolved model id).
pub async fn find_initial_model_full(
    options: FindInitialModelFullOptions<'_>,
) -> Result<InitialModelResult, String> {
    if let (Some(cli_provider), Some(cli_model)) = (options.cli_provider, options.cli_model) {
        let resolved = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: Some(cli_provider),
            cli_model: Some(cli_model),
            cli_thinking: None,
            model_runtime: options.model_runtime,
        });
        if let Some(error) = resolved.error {
            return Err(error);
        }
        if let Some(model) = resolved.model {
            return Ok(InitialModelResult {
                model: Some(model),
                thinking_level: DEFAULT_THINKING_LEVEL,
                fallback_message: None,
            });
        }
    }

    Ok(find_initial_model(FindInitialModelOptions {
        cli_model: None,
        scoped_models: options.scoped_models,
        is_continuing: options.is_continuing,
        default_provider: options.default_provider,
        default_model_id: options.default_model_id,
        default_thinking_level: options.default_thinking_level,
        model_runtime: options.model_runtime,
    })
    .await)
}

/// Re-export table of default model ids for known providers.
#[must_use]
pub fn default_model_per_provider_map() -> &'static [(&'static str, &'static str)] {
    default_model_per_provider()
}

/// Look up the default model id for a known provider, if listed.
#[must_use]
pub fn default_model_id_for_provider(provider: &str) -> Option<&'static str> {
    default_model_per_provider()
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, id)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::{ModelCost, ModelInput};

    fn model(id: &str, name: &str, provider: &str, reasoning: bool) -> Model {
        Model {
            id: id.to_owned(),
            name: name.to_owned(),
            api: "anthropic-messages".to_owned(),
            provider: provider.to_owned(),
            base_url: format!("https://{provider}.example"),
            reasoning,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 1.0,
                tiers: None,
            },
            context_window: 128_000,
            max_tokens: 8_192,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::default(),
        }
    }

    fn all_models() -> Vec<Model> {
        vec![
            model("claude-sonnet-4-5", "Claude Sonnet 4.5", "anthropic", true),
            model("gpt-4o", "GPT-4o", "openai", false),
            model(
                "qwen/qwen3-coder:exacto",
                "Qwen3 Coder Exacto",
                "openrouter",
                true,
            ),
            model(
                "openai/gpt-4o:extended",
                "GPT-4o Extended",
                "openrouter",
                false,
            ),
        ]
    }

    #[test]
    fn parse_exact_and_partial_and_missing() {
        let models = all_models();
        let exact = parse_model_pattern(
            "claude-sonnet-4-5",
            &models,
            ParseModelPatternOptions::default(),
        );
        assert_eq!(
            exact.model.as_ref().map(|m| m.id.as_str()),
            Some("claude-sonnet-4-5")
        );
        assert!(exact.thinking_level.is_none());
        assert!(exact.warning.is_none());

        let partial = parse_model_pattern("sonnet", &models, ParseModelPatternOptions::default());
        assert_eq!(
            partial.model.as_ref().map(|m| m.id.as_str()),
            Some("claude-sonnet-4-5")
        );

        let missing =
            parse_model_pattern("nonexistent", &models, ParseModelPatternOptions::default());
        assert!(missing.model.is_none());
    }

    #[test]
    fn parse_valid_thinking_suffixes() {
        let models = all_models();
        for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let result = parse_model_pattern(
                &format!("sonnet:{level}"),
                &models,
                ParseModelPatternOptions::default(),
            );
            assert_eq!(
                result.model.as_ref().map(|m| m.id.as_str()),
                Some("claude-sonnet-4-5")
            );
            assert_eq!(result.thinking_level, parse_thinking_level(level));
            assert!(result.warning.is_none());
        }
    }

    #[test]
    fn parse_invalid_thinking_suffix_warns_in_scope_mode() {
        let models = all_models();
        let result = parse_model_pattern(
            "sonnet:random",
            &models,
            ParseModelPatternOptions::default(),
        );
        assert_eq!(
            result.model.as_ref().map(|m| m.id.as_str()),
            Some("claude-sonnet-4-5")
        );
        assert!(result.thinking_level.is_none());
        assert!(
            result
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("Invalid thinking level") && w.contains("random"))
        );
    }

    #[test]
    fn parse_openrouter_colon_ids() {
        let models = all_models();
        let exact = parse_model_pattern(
            "qwen/qwen3-coder:exacto",
            &models,
            ParseModelPatternOptions::default(),
        );
        assert_eq!(
            exact.model.as_ref().map(|m| m.id.as_str()),
            Some("qwen/qwen3-coder:exacto")
        );
        assert!(exact.thinking_level.is_none());

        let with_provider = parse_model_pattern(
            "openrouter/qwen/qwen3-coder:exacto",
            &models,
            ParseModelPatternOptions::default(),
        );
        assert_eq!(
            with_provider
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("openrouter", "qwen/qwen3-coder:exacto"))
        );

        let with_level = parse_model_pattern(
            "qwen/qwen3-coder:exacto:high",
            &models,
            ParseModelPatternOptions::default(),
        );
        assert_eq!(
            with_level.model.as_ref().map(|m| m.id.as_str()),
            Some("qwen/qwen3-coder:exacto")
        );
        assert_eq!(with_level.thinking_level, Some(ModelThinkingLevel::High));

        let invalid_tail = parse_model_pattern(
            "qwen/qwen3-coder:exacto:random",
            &models,
            ParseModelPatternOptions::default(),
        );
        assert_eq!(
            invalid_tail.model.as_ref().map(|m| m.id.as_str()),
            Some("qwen/qwen3-coder:exacto")
        );
        assert!(invalid_tail.thinking_level.is_none());
        assert!(invalid_tail.warning.is_some());
    }

    #[test]
    fn parse_empty_and_trailing_colon() {
        let models = all_models();
        let empty = parse_model_pattern("", &models, ParseModelPatternOptions::default());
        assert!(empty.model.is_some());

        let trailing = parse_model_pattern("sonnet:", &models, ParseModelPatternOptions::default());
        assert_eq!(
            trailing.model.as_ref().map(|m| m.id.as_str()),
            Some("claude-sonnet-4-5")
        );
        assert!(trailing.warning.is_some());
    }

    #[test]
    fn find_exact_rejects_ambiguous_bare_id() {
        let models = vec![
            model("shared", "A", "alpha", false),
            model("shared", "B", "beta", false),
        ];
        assert!(find_exact_model_reference_match("shared", &models).is_none());
        assert_eq!(
            find_exact_model_reference_match("alpha/shared", &models)
                .as_ref()
                .map(|m| m.provider.as_str()),
            Some("alpha")
        );
    }

    #[test]
    fn scope_diagnostics_and_duplicate_removal() {
        let models = all_models();
        let patterns = vec![
            "sonnet:high".to_owned(),
            "gpt-4o:invalid".to_owned(),
            "missing".to_owned(),
            "claude-sonnet-4-5".to_owned(),
        ];
        let result = resolve_model_scope_from_models(&patterns, &models);
        assert_eq!(
            result
                .scoped_models
                .iter()
                .map(|s| s.model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-sonnet-4-5", "gpt-4o"]
        );
        assert_eq!(
            result.scoped_models[0].thinking_level,
            Some(ModelThinkingLevel::High)
        );
        assert!(result.scoped_models[1].thinking_level.is_none());
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(
            result.diagnostics[0].message,
            "Invalid thinking level \"invalid\" in pattern \"gpt-4o:invalid\". Using default instead."
        );
        assert_eq!(
            result.diagnostics[1].message,
            "No models match pattern \"missing\""
        );
    }

    #[test]
    fn scope_glob_matches_id_or_provider_path() {
        let models = all_models();
        let patterns = vec!["*sonnet*".to_owned(), "openai/*".to_owned()];
        let result = resolve_model_scope_from_models(&patterns, &models);
        let ids: Vec<&str> = result
            .scoped_models
            .iter()
            .map(|s| s.model.id.as_str())
            .collect();
        assert!(ids.contains(&"claude-sonnet-4-5"));
        assert!(ids.contains(&"gpt-4o"));
    }

    #[test]
    fn scope_glob_thinking_suffix() {
        let models = all_models();
        let patterns = vec!["anthropic/*:high".to_owned()];
        let result = resolve_model_scope_from_models(&patterns, &models);
        assert!(!result.scoped_models.is_empty());
        assert!(
            result
                .scoped_models
                .iter()
                .all(|s| s.thinking_level == Some(ModelThinkingLevel::High))
        );
    }

    #[test]
    fn resolve_cli_provider_slash_and_fuzzy() {
        let models = all_models();
        let auth = |_p: &str| true;

        let by_slash = resolve_cli_model_from(None, Some("openai/gpt-4o"), None, &models, auth);
        assert_eq!(
            by_slash
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("openai", "gpt-4o"))
        );

        let fuzzy = resolve_cli_model_from(Some("openai"), Some("4o"), None, &models, auth);
        assert_eq!(fuzzy.model.as_ref().map(|m| m.id.as_str()), Some("gpt-4o"));

        let thinking = resolve_cli_model_from(None, Some("sonnet:high"), None, &models, auth);
        assert_eq!(
            thinking.model.as_ref().map(|m| m.id.as_str()),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(thinking.thinking_level, Some(ModelThinkingLevel::High));
    }

    #[test]
    fn resolve_cli_prefers_openrouter_style_raw_id_over_provider_inference() {
        let models = all_models();
        let result =
            resolve_cli_model_from(None, Some("openai/gpt-4o:extended"), None, &models, |_| {
                true
            });
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("openrouter", "openai/gpt-4o:extended"))
        );
    }

    #[test]
    fn resolve_cli_strict_invalid_suffix_keeps_custom_fallback_id() {
        let models = all_models();
        let result = resolve_cli_model_from(
            Some("openai"),
            Some("gpt-4o:extended"),
            None,
            &models,
            |_| true,
        );
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("openai", "gpt-4o:extended"))
        );
    }

    #[test]
    fn resolve_cli_custom_model_without_double_prefix() {
        let models = all_models();
        let result = resolve_cli_model_from(
            Some("openrouter"),
            Some("openrouter/openai/ghost-model"),
            None,
            &models,
            |_| true,
        );
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("openrouter", "openai/ghost-model"))
        );
        assert!(result.warning.is_some());
    }

    #[test]
    fn resolve_cli_no_models_error() {
        let result = resolve_cli_model_from(Some("openai"), Some("gpt-4o"), None, &[], |_| true);
        assert!(result.model.is_none());
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|e| e.contains("No models available"))
        );
    }

    #[test]
    fn resolve_cli_prefers_provider_split_over_gateway_id() {
        let mut models = all_models();
        models.push(model("glm-5", "GLM-5", "zai", true));
        models.push(model("zai/glm-5", "GLM-5", "vercel-ai-gateway", true));
        let result = resolve_cli_model_from(None, Some("zai/glm-5"), None, &models, |_| true);
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("zai", "glm-5"))
        );
    }

    #[test]
    fn resolve_cli_prefers_authenticated_raw_id_over_unauth_inferred_provider() {
        let mut models = all_models();
        models.push(model(
            "xiaomi/mimo-v2.5-pro",
            "Xiaomi MiMo via Commandcode",
            "commandcode",
            false,
        ));
        models.push(model("mimo-v2.5-pro", "Xiaomi MiMo", "xiaomi", false));
        let result = resolve_cli_model_from(
            None,
            Some("xiaomi/mimo-v2.5-pro"),
            None,
            &models,
            |provider| provider == "commandcode",
        );
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("commandcode", "xiaomi/mimo-v2.5-pro"))
        );
    }

    #[test]
    fn resolve_cli_provider_prefixed_fuzzy() {
        let models = all_models();
        let result = resolve_cli_model_from(None, Some("openrouter/qwen"), None, &models, |_| true);
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("openrouter", "qwen/qwen3-coder:exacto"))
        );
    }

    #[test]
    fn resolve_cli_fallback_strips_thinking_suffix() {
        let mut models = all_models();
        models.push(model(
            "some-base-model",
            "Some Base Model",
            "neuralwatt",
            false,
        ));
        let result = resolve_cli_model_from(
            None,
            Some("neuralwatt/zai-org/GLM-5.1-FP8:high"),
            None,
            &models,
            |_| true,
        );
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("neuralwatt", "zai-org/GLM-5.1-FP8"))
        );
        assert_eq!(result.model.as_ref().map(|m| m.reasoning), Some(true));
        assert_eq!(result.thinking_level, Some(ModelThinkingLevel::High));

        let invalid = resolve_cli_model_from(
            None,
            Some("neuralwatt/zai-org/GLM-5.1-FP8:banana"),
            None,
            &models,
            |_| true,
        );
        assert_eq!(
            invalid.model.as_ref().map(|m| m.id.as_str()),
            Some("zai-org/GLM-5.1-FP8:banana")
        );

        let explicit_thinking = resolve_cli_model_from(
            None,
            Some("neuralwatt/zai-org/GLM-5.1-FP8:high"),
            Some(ModelThinkingLevel::Medium),
            &models,
            |_| true,
        );
        assert_eq!(
            explicit_thinking.model.as_ref().map(|m| m.id.as_str()),
            Some("zai-org/GLM-5.1-FP8:high")
        );
        assert!(explicit_thinking.thinking_level.is_none());
    }

    #[test]
    fn default_model_table_tracks_current_ids() {
        assert_eq!(default_model_id_for_provider("openai"), Some("gpt-5.5"));
        assert_eq!(
            default_model_id_for_provider("openai-codex"),
            Some("gpt-5.5")
        );
        assert_eq!(default_model_id_for_provider("zai"), Some("glm-5.1"));
        assert_eq!(
            default_model_id_for_provider("minimax"),
            Some("MiniMax-M2.7")
        );
        assert_eq!(
            default_model_id_for_provider("vercel-ai-gateway"),
            Some("zai/glm-5.1")
        );
        assert_eq!(
            default_model_id_for_provider("ant-ling"),
            Some("Ring-2.6-1T")
        );
    }

    #[test]
    fn alias_preference_over_dated_versions() {
        let models = vec![
            model("claude-sonnet-4-5-20241022", "dated old", "anthropic", true),
            model("claude-sonnet-4-5-20250929", "dated new", "anthropic", true),
            model("claude-sonnet-4-5", "alias", "anthropic", true),
        ];
        let result = parse_model_pattern("sonnet", &models, ParseModelPatternOptions::default());
        assert_eq!(
            result.model.as_ref().map(|m| m.id.as_str()),
            Some("claude-sonnet-4-5")
        );

        let dated_only = vec![
            model("claude-sonnet-4-5-20241022", "dated old", "anthropic", true),
            model("claude-sonnet-4-5-20250929", "dated new", "anthropic", true),
        ];
        let latest =
            parse_model_pattern("sonnet", &dated_only, ParseModelPatternOptions::default());
        assert_eq!(
            latest.model.as_ref().map(|m| m.id.as_str()),
            Some("claude-sonnet-4-5-20250929")
        );
    }

    #[tokio::test]
    #[allow(clippy::panic)]
    async fn find_initial_model_full_cli_custom_and_available_fallback() {
        use std::sync::Arc;

        use pi_ai::auth::InMemoryCredentialStore;
        use pi_ai::models_store::InMemoryModelsStore;

        use crate::core::model_runtime::{
            CreateModelRuntimeOptions, ModelsJsonConfig, ProviderConfigInput,
            ProviderModelDefinition,
        };

        let runtime = match ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(Arc::new(InMemoryCredentialStore::new())),
            models_store: Some(Arc::new(InMemoryModelsStore::new())),
            models_config: Some(ModelsJsonConfig::empty()),
            allow_model_network: Some(false),
            ..CreateModelRuntimeOptions::default()
        })
        .await
        {
            Ok(runtime) => runtime,
            Err(error) => panic!("runtime: {error}"),
        };

        if let Err(error) = runtime.register_provider(
            "openrouter",
            ProviderConfigInput {
                base_url: Some("https://openrouter.ai/api/v1".into()),
                api: Some("openai-completions".into()),
                api_key: Some("sk-test".into()),
                models: Some(vec![ProviderModelDefinition {
                    id: "qwen/qwen3-coder:exacto".into(),
                    name: Some("Qwen".into()),
                    api: Some("openai-completions".into()),
                    base_url: Some("https://openrouter.ai/api/v1".into()),
                    reasoning: true,
                    thinking_level_map: None,
                    input: Some(vec![ModelInput::Text]),
                    cost: None,
                    context_window: Some(128_000),
                    max_tokens: Some(8192),
                    headers: None,
                    compat: None,
                }]),
                ..ProviderConfigInput::default()
            },
        ) {
            panic!("register: {error}");
        }

        let result = match find_initial_model_full(FindInitialModelFullOptions {
            cli_provider: Some("openrouter"),
            cli_model: Some("openrouter/openai/ghost-model"),
            scoped_models: &[],
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await
        {
            Ok(result) => result,
            Err(error) => panic!("ok: {error}"),
        };
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|m| (m.provider.as_str(), m.id.as_str())),
            Some(("openrouter", "openai/ghost-model"))
        );

        // Unauthenticated saved default is ignored by shared services path.
        let mut env = pi_ai::auth::ProviderEnv::new();
        env.insert("OPENAI_API_KEY".to_owned(), "sk-test".to_owned());
        let runtime_auth = match ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(Arc::new(InMemoryCredentialStore::new())),
            models_store: Some(Arc::new(InMemoryModelsStore::new())),
            models_config: Some(ModelsJsonConfig::empty()),
            allow_model_network: Some(false),
            auth_env: Some(env),
            ..CreateModelRuntimeOptions::default()
        })
        .await
        {
            Ok(runtime) => runtime,
            Err(error) => panic!("runtime: {error}"),
        };

        let ignored = find_initial_model(FindInitialModelOptions {
            cli_model: None,
            scoped_models: &[],
            is_continuing: false,
            default_provider: Some("deepseek"),
            default_model_id: Some("deepseek-v4-flash"),
            default_thinking_level: None,
            model_runtime: &runtime_auth,
        })
        .await;
        assert_eq!(
            ignored.model.as_ref().map(|m| m.provider.as_str()),
            Some("openai")
        );
    }
}
