//! Endpoint planning: discovery classification, endpoint-plan construction,
//! generation assembly, and the reload flag-encoding pipeline.
//!
//! # Shape (WHY)
//!
//! `EndpointPlanner` is a pure configuration holder (discovered paths, load
//! cwd, trust, command provenance) plus the stateless build machinery. It
//! never touches published slots, bridge relays, or the replacement gate, so
//! every path here runs without a live facade. Choreography that must consult
//! facade state (the reloadable gate, published generation ids, and the
//! injected-reload test seam that triggers facade invalidation) stays on
//! `ExtensionRuntimeSet`, which delegates its build steps to this module.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use futures::stream::{FuturesUnordered, StreamExt};
use pi_ext::client::HostClient;
use pi_ext::host::{self, HostSource, HostSpec};
use pi_ext::protocol::FlagValueWire;
use serde_json::Value;

use super::{
    EndpointKind, ExtensionSetDiagnostic, Generation, HostClientError, PendingBridges,
    endpoint_diagnostic_path, generation_from_endpoints,
};
use crate::core::agent_session_runtime::spawn_runtime_safe;
use crate::core::extension_host::{HOOK_TIMEOUT, HostExtensionRunner, HostStartError};
use crate::core::extension_manifest::{ClassifiedExtension, ExtensionRuntime, classify};
use crate::core::resources::SourceInfo;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EndpointPlan {
    pub(super) position: usize,
    pub(super) kind: EndpointKind,
    pub(super) entries: Vec<String>,
    pub(super) diagnostic_paths: Vec<String>,
    pub(super) builtins: bool,
    pub(super) label: String,
}

#[derive(Clone, Copy)]
pub(super) enum GenerationBuildPolicy {
    BestEffortStart,
    // Only the RequireAll* build-policy tests construct this variant.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "constructed only by the RequireAll* tests")
    )]
    RequireAllEndpointStarts,
}

pub(super) struct GenerationBuild {
    pub(super) generation: Option<Generation>,
    pub(super) pending: PendingBridges,
    pub(super) diagnostics: Vec<ExtensionSetDiagnostic>,
    pub(super) endpoint_start_failure: Option<ExtensionSetDiagnostic>,
}

type EndpointStartOutcome = (
    usize,
    EndpointPlan,
    Result<Arc<HostExtensionRunner>, String>,
);

struct PreparedEndpoint {
    position: usize,
    kind: EndpointKind,
    label: String,
    runner: Arc<HostExtensionRunner>,
    plan: EndpointPlan,
}

struct GenerationStarts {
    endpoints: Vec<PreparedEndpoint>,
    diagnostics: Vec<ExtensionSetDiagnostic>,
    endpoint_start_failure: Option<ExtensionSetDiagnostic>,
    failed_builtins_owner: Option<EndpointPlan>,
}

/// Prepared replacement held between prepare and commit of a reload.
///
/// The fields stay `pub(super)` because the facade's `commit_reload` drains
/// them in place and the reload tests build empty variants; the module tree
/// boundary is unchanged from when the type lived beside the facade.
pub(crate) struct PreparedReload {
    pub(super) generation: Option<Generation>,
    pub(super) pending: PendingBridges,
    pub(super) diagnostics: Vec<ExtensionSetDiagnostic>,
}

#[cfg(test)]
impl PreparedReload {
    pub(crate) fn empty_for_test() -> Self {
        Self {
            generation: None,
            pending: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

impl Drop for PreparedReload {
    fn drop(&mut self) {
        if let Some(generation) = self.generation.take() {
            generation.abort_bridges();
            for endpoint in generation.endpoints.iter() {
                let runner = Arc::clone(&endpoint.runner);
                spawn_runtime_safe("prepared-reload-shutdown", async move {
                    runner.shutdown_once().await;
                });
            }
        }
    }
}

/// Classify every discovered path, collecting per-path failures instead of
/// aborting the batch: one unreadable extension must not hide the others.
pub(super) fn classify_paths(
    discovered_paths: &[String],
) -> (Vec<ClassifiedExtension>, Vec<ExtensionSetDiagnostic>) {
    let mut classified = Vec::new();
    let mut diagnostics = Vec::new();
    for path in discovered_paths {
        match classify(path) {
            Ok(extension) => classified.push(extension),
            Err(error) => diagnostics.push(ExtensionSetDiagnostic {
                path: path.clone(),
                message: error.to_string(),
            }),
        }
    }
    (classified, diagnostics)
}

pub(super) fn plan_endpoints(classified: &[ClassifiedExtension]) -> Vec<EndpointPlan> {
    let mut plans: Vec<EndpointPlan> = Vec::new();
    for extension in classified {
        let kind = match extension.runtime {
            ExtensionRuntime::TsCompat => EndpointKind::TsCompat,
            ExtensionRuntime::Native => EndpointKind::Native,
        };
        if kind != EndpointKind::Native
            && let Some(last) = plans.last_mut()
            && last.kind == kind
        {
            last.entries.push(extension.entry.clone());
            last.diagnostic_paths.push(extension.discovered.clone());
            continue;
        }
        plans.push(EndpointPlan {
            position: 0,
            kind,
            entries: vec![extension.entry.clone()],
            diagnostic_paths: vec![extension.discovered.clone()],
            builtins: false,
            label: extension.discovered.clone(),
        });
    }
    if plans.is_empty() {
        plans.push(EndpointPlan {
            position: 0,
            kind: EndpointKind::TsCompat,
            entries: Vec::new(),
            diagnostic_paths: Vec::new(),
            builtins: true,
            label: "<builtins>".to_owned(),
        });
    } else if let Some(compat) = plans
        .iter_mut()
        .find(|plan| plan.kind == EndpointKind::TsCompat)
    {
        compat.builtins = true;
    }
    for (position, plan) in plans.iter_mut().enumerate() {
        plan.position = position;
    }
    plans
}

pub(super) fn endpoint_host_spec(
    plan: &EndpointPlan,
    ts_spec: Option<&Result<HostSpec, host::HostError>>,
) -> Result<HostSpec, String> {
    match plan.kind {
        EndpointKind::Native => plan.entries.first().map(PathBuf::from).map_or_else(
            || Err("native endpoint plan is missing its executable".to_owned()),
            |program| {
                Ok(HostSpec {
                    source: HostSource::NativeExtension(program.clone()),
                    program,
                    args: Vec::new(),
                })
            },
        ),
        EndpointKind::TsCompat => match ts_spec {
            Some(Ok(spec)) => {
                let mut spec = spec.clone();
                if !plan.builtins {
                    spec.args.push("--no-builtins".to_owned());
                }
                Ok(spec)
            }
            Some(Err(error)) => Err(error.to_string()),
            None => Err("compatibility endpoint plan has no resolved host".to_owned()),
        },
    }
}

pub(super) fn resolve_typescript_host(
    plans: &[EndpointPlan],
) -> Option<Result<HostSpec, host::HostError>> {
    plans
        .iter()
        .any(|plan| plan.kind != EndpointKind::Native)
        .then(host::resolve_host)
}

fn collect_generation_starts(results: Vec<EndpointStartOutcome>) -> GenerationStarts {
    let mut endpoints = Vec::new();
    let mut diagnostics = Vec::new();
    let mut endpoint_start_failure = None;
    let mut failed_builtins_owner = None;
    for (position, plan, result) in results {
        match result {
            Ok(runner) => {
                for (path, message) in runner.load_errors() {
                    let path = plan
                        .entries
                        .iter()
                        .position(|entry| entry == &path)
                        .and_then(|index| plan.diagnostic_paths.get(index))
                        .cloned()
                        .unwrap_or(path);
                    diagnostics.push(ExtensionSetDiagnostic { path, message });
                }
                endpoints.push(PreparedEndpoint {
                    position,
                    kind: plan.kind,
                    label: plan.label.clone(),
                    runner,
                    plan,
                });
            }
            Err(message) => {
                if plan.builtins {
                    failed_builtins_owner = Some(plan.clone());
                }
                let paths = if plan.diagnostic_paths.is_empty() {
                    vec![plan.label]
                } else {
                    plan.diagnostic_paths
                };
                for path in paths {
                    let diagnostic = ExtensionSetDiagnostic {
                        path,
                        message: message.clone(),
                    };
                    if endpoint_start_failure.is_none() {
                        endpoint_start_failure = Some(diagnostic.clone());
                    }
                    diagnostics.push(diagnostic);
                }
            }
        }
    }
    GenerationStarts {
        endpoints,
        diagnostics,
        endpoint_start_failure,
        failed_builtins_owner,
    }
}

async fn start_endpoint(
    plan: EndpointPlan,
    spec: HostSpec,
    load_cwd: String,
    project_trusted: bool,
) -> Result<Arc<HostExtensionRunner>, String> {
    let client = Arc::new(HostClient::spawn(&spec).map_err(|error| error.to_string())?);
    let result = HostExtensionRunner::connect_with_cwd_and_trust(
        Arc::clone(&client),
        plan.entries,
        load_cwd,
        project_trusted,
        HOOK_TIMEOUT,
    )
    .await;
    if result.is_err() {
        let _ = client.shutdown().await;
    }
    result.map_err(|error| error.to_string())
}

/// Render every reload diagnostic in collection order so a failed prepare
/// surfaces the full path-scoped failure history, not just the terminal error.
pub(super) fn summarize_diagnostics(diagnostics: &[ExtensionSetDiagnostic]) -> String {
    diagnostics
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

pub(super) async fn apply_flags_to_generation(
    generation: &Generation,
    flags: &BTreeMap<String, FlagValueWire>,
) -> Result<Vec<ExtensionSetDiagnostic>, HostClientError> {
    if flags.is_empty() {
        return Ok(Vec::new());
    }
    let mut diagnostics = Vec::new();
    for endpoint in generation.endpoints.iter() {
        if let Err(error) = endpoint.runner.apply_flag_values(flags).await {
            diagnostics.push(ExtensionSetDiagnostic {
                path: endpoint_diagnostic_path(endpoint),
                message: error.to_string(),
            });
        }
    }
    Ok(diagnostics)
}

pub(super) fn encode_flags(
    values: HashMap<String, Value>,
) -> Result<BTreeMap<String, FlagValueWire>, HostStartError> {
    values
        .into_iter()
        .map(|(name, value)| {
            let value = match value {
                Value::Bool(value) => FlagValueWire::Boolean(value),
                Value::String(value) => FlagValueWire::String(value),
                other => {
                    return Err(HostStartError::FlagSync(format!(
                        "flag {name:?} has unsupported value {other}"
                    )));
                }
            };
            Ok((name, value))
        })
        .collect()
}

/// Classify and start all valid endpoint plans. Cold startup is best-effort.
pub(super) async fn build_generation(
    id: u64,
    plans: Vec<EndpointPlan>,
    load_cwd: &str,
    project_trusted: bool,
    policy: GenerationBuildPolicy,
) -> GenerationBuild {
    let ts_spec = resolve_typescript_host(&plans);
    build_generation_with_starter(
        id,
        plans,
        load_cwd,
        project_trusted,
        policy,
        ts_spec,
        start_endpoint,
    )
    .await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn build_generation_with_starter<Starter, StartFuture>(
    id: u64,
    plans: Vec<EndpointPlan>,
    load_cwd: &str,
    project_trusted: bool,
    policy: GenerationBuildPolicy,
    ts_spec: Option<Result<HostSpec, host::HostError>>,
    starter: Starter,
) -> GenerationBuild
where
    Starter: Fn(EndpointPlan, HostSpec, String, bool) -> StartFuture + Clone,
    StartFuture: Future<Output = Result<Arc<HostExtensionRunner>, String>>,
{
    let mut starts = FuturesUnordered::new();
    for plan in plans {
        let spec = endpoint_host_spec(&plan, ts_spec.as_ref());
        let cwd = load_cwd.to_owned();
        let starter = starter.clone();
        starts.push(async move {
            let position = plan.position;
            let result = match spec {
                Ok(spec) => starter(plan.clone(), spec, cwd, project_trusted).await,
                Err(message) => Err(message),
            };
            (position, plan, result)
        });
    }

    let mut results = Vec::new();
    while let Some(result) = starts.next().await {
        results.push(result);
    }
    results.sort_by_key(|(position, _, _)| *position);
    let GenerationStarts {
        mut endpoints,
        mut diagnostics,
        endpoint_start_failure,
        failed_builtins_owner,
    } = collect_generation_starts(results);

    if matches!(policy, GenerationBuildPolicy::RequireAllEndpointStarts)
        && endpoint_start_failure.is_some()
    {
        let mut stops = endpoints
            .iter()
            .map(|endpoint| endpoint.runner.shutdown_once())
            .collect::<FuturesUnordered<_>>();
        while stops.next().await.is_some() {}
        return GenerationBuild {
            generation: None,
            pending: Vec::new(),
            diagnostics,
            endpoint_start_failure,
        };
    }

    if matches!(policy, GenerationBuildPolicy::BestEffortStart)
        && !endpoints.is_empty()
        && failed_builtins_owner.is_some()
        && let Some(index) = endpoints
            .iter()
            .position(|endpoint| endpoint.kind == EndpointKind::TsCompat && !endpoint.plan.builtins)
    {
        let mut promotion_plan = endpoints[index].plan.clone();
        promotion_plan.builtins = true;
        let result = match endpoint_host_spec(&promotion_plan, ts_spec.as_ref()) {
            Ok(spec) => {
                starter(
                    promotion_plan.clone(),
                    spec,
                    load_cwd.to_owned(),
                    project_trusted,
                )
                .await
            }
            Err(message) => Err(message),
        };
        match result {
            Ok(runner) => {
                for (path, message) in runner.load_errors() {
                    let path = promotion_plan
                        .entries
                        .iter()
                        .position(|entry| entry == &path)
                        .and_then(|entry_index| promotion_plan.diagnostic_paths.get(entry_index))
                        .cloned()
                        .unwrap_or(path);
                    diagnostics.push(ExtensionSetDiagnostic { path, message });
                }
                let position = endpoints[index].position;
                let old = std::mem::replace(
                    &mut endpoints[index],
                    PreparedEndpoint {
                        position,
                        kind: promotion_plan.kind,
                        label: promotion_plan.label.clone(),
                        runner,
                        plan: promotion_plan,
                    },
                );
                old.runner.shutdown_once().await;
            }
            Err(message) => {
                let label = endpoints[index].label.clone();
                diagnostics.push(ExtensionSetDiagnostic {
                    path: label.clone(),
                    message: format!("builtins promotion failed for {label}: {message}"),
                });
            }
        }
    }

    if endpoints.is_empty() {
        return GenerationBuild {
            generation: None,
            pending: Vec::new(),
            diagnostics,
            endpoint_start_failure,
        };
    }
    endpoints.sort_by_key(|endpoint| endpoint.position);
    let endpoints = endpoints
        .into_iter()
        .map(|endpoint| (endpoint.kind, endpoint.label, endpoint.runner))
        .collect();
    let (generation, pending) = generation_from_endpoints(id, endpoints);
    GenerationBuild {
        generation: Some(generation),
        pending,
        diagnostics,
        endpoint_start_failure,
    }
}

/// stateless machinery that turns a discovery batch into a [`GenerationBuild`].
pub(super) struct EndpointPlanner {
    discovered_paths: Vec<String>,
    load_cwd: String,
    project_trusted: bool,
    command_source_infos: StdMutex<HashMap<String, SourceInfo>>,
}

impl EndpointPlanner {
    pub(super) fn new(
        discovered_paths: Vec<String>,
        load_cwd: String,
        project_trusted: bool,
    ) -> Self {
        Self {
            discovered_paths,
            load_cwd,
            project_trusted,
            command_source_infos: StdMutex::new(HashMap::new()),
        }
    }

    /// Classify and plan the planner's own discovery batch. The diagnostics
    /// precede any build diagnostics, matching the facade's collection order.
    pub(super) fn plan(&self) -> (Vec<EndpointPlan>, Vec<ExtensionSetDiagnostic>) {
        let (classified, diagnostics) = classify_paths(&self.discovered_paths);
        (plan_endpoints(&classified), diagnostics)
    }

    /// Classify an explicit discovery batch into ordered endpoint plans. Used
    /// by cold startup, which builds before the facade (and its planner) exist.
    pub(super) fn plan_paths(
        discovered_paths: &[String],
    ) -> (Vec<EndpointPlan>, Vec<ExtensionSetDiagnostic>) {
        let (classified, diagnostics) = classify_paths(discovered_paths);
        (plan_endpoints(&classified), diagnostics)
    }

    /// Build a generation from plans using this planner's load configuration.
    pub(super) async fn build(
        &self,
        id: u64,
        plans: Vec<EndpointPlan>,
        policy: GenerationBuildPolicy,
    ) -> GenerationBuild {
        build_generation(id, plans, &self.load_cwd, self.project_trusted, policy).await
    }

    /// Install resource-loader provenance for command-owning extension paths.
    pub(super) fn set_command_source_infos(
        &self,
        infos: impl IntoIterator<Item = (String, SourceInfo)>,
    ) {
        let mut current = self
            .command_source_infos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.clear();
        current.extend(infos);
    }

    /// Resolve resource-loader provenance for one command-owning path.
    #[must_use]
    pub(super) fn command_source_info(&self, path: &str) -> Option<SourceInfo> {
        self.command_source_infos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
    }

    /// Point-in-time provenance map for the facade's catalog enrichment. A
    /// single lock-then-clone keeps the facade's lookup behavior unchanged.
    #[must_use]
    pub(super) fn command_source_infos_snapshot(&self) -> HashMap<String, SourceInfo> {
        self.command_source_infos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
