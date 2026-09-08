//! Development-only Chord facet host wire contracts.
//!
//! This module is intentionally not registered in the installed protocol yet.
//! The product example enables it after the shared protocol/client plumbing is
//! landed. Values in service calls and updates use pi-agent's canonical Chord
//! domain so an omitted result remains distinct from an explicit `null`.

use std::collections::BTreeMap;

use pi_agent::service::value::{JsObject, JsString, JsonValue};
use pi_agent::service::wire::{
    parse_service_call, parse_service_catalogue, parse_service_provider_update, ServiceCall,
    ServiceCatalogueEntry, ServiceProviderUpdate, WireError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Build one plugin package's facet bundle.
pub const FACET_BUNDLE_BUILD_METHOD: &str = "facet.bundle.build";
/// Load a facet host generation.
pub const FACET_HOST_LOAD_METHOD: &str = "facet.host.load";
/// Reload a facet host generation.
pub const FACET_HOST_RELOAD_METHOD: &str = "facet.host.reload";
/// Dispose a facet host.
pub const FACET_HOST_DISPOSE_METHOD: &str = "facet.host.dispose";
/// Invoke a remote Chord service member.
pub const FACET_SERVICE_INVOKE_METHOD: &str = "facet.service.invoke";
/// Publish a remote Chord service update.
pub const FACET_SERVICE_UPDATE_METHOD: &str = "facet.service.update";
/// Run one local slash command.
pub const FACET_SLASH_RUN_METHOD: &str = "facet.slash.run";
/// Complete one local slash command's argument.
pub const FACET_SLASH_COMPLETE_METHOD: &str = "facet.slash.complete";
/// Publish the current local slash-command metadata.
pub const FACET_SLASH_CHANGED_METHOD: &str = "facet.slash.changed";

/// External import resolved by the product facet host.
pub const PI_PLUGIN_API: &str = "@earendil-works/pi-coding-agent/experimental/plugin";
/// Key carrying presentation facet artifacts in a product service result.
pub const PRESENTATION_FACET_BUNDLES_KEY: &str = "presentationFacetBundles";

/// Facet entry selected for one host generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FacetHostEntry {
    /// Session-worker entry.
    Session,
    /// Presentation/TUI entry.
    Tui,
}

impl FacetHostEntry {
    /// Source spelling used on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Tui => "tui",
        }
    }

    /// Parse the source spelling.
    pub fn parse(value: &JsonValue) -> Result<Self, FacetWireError> {
        match value.as_str().and_then(|value| value.try_to_utf8().ok()).as_deref() {
            Some("session") => Ok(Self::Session),
            Some("tui") => Ok(Self::Tui),
            _ => Err(FacetWireError::Invalid("facet host entry")),
        }
    }
}

/// Bundle manifest entry exchanged as part of an artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetBundleEntryWire {
    /// Relative JavaScript file name.
    pub file: String,
    /// SHA-256 subresource-integrity value.
    pub integrity: String,
    /// External package specifiers intentionally left for the host.
    pub external_imports: Vec<String>,
    /// Optional relative source-map file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_map: Option<String>,
}

/// Bundle package identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FacetBundlePluginWire {
    /// Package name.
    pub id: String,
    /// Package version, when supplied by package metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One transportable facet bundle artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetBundleArtifactWire {
    /// Artifact format tag.
    pub format: String,
    /// Artifact schema version.
    pub format_version: u32,
    /// Package identity.
    pub plugin: FacetBundlePluginWire,
    /// Entry selected from the package manifest.
    pub entry_name: String,
    /// Manifest entry metadata.
    pub entry: FacetBundleEntryWire,
    /// Content-addressed CommonJS source.
    pub source: String,
    /// Optional source-map contents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_map_contents: Option<String>,
}

/// Build request decoded at the canonical boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetBundleBuildRequest {
    /// Package directory or package.json path.
    pub package_path: String,
    /// Output directory for the generated bundle.
    pub outdir: String,
    /// Application-selected entry conventions.
    pub default_facets: BTreeMap<String, String>,
}

/// Build result returned by the host.
#[derive(Clone, Debug, PartialEq)]
pub struct FacetBundleBuildResponse {
    /// Absolute (or host-resolved) manifest path.
    pub manifest_path: String,
    /// Optional `tui` artifact ready for transport.
    pub tui_artifacts: Vec<JsonValue>,
}

/// Load request decoded at the canonical boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct FacetHostLoadRequest {
    /// Host identity scoped to the server/session owner.
    pub host_id: String,
    /// Entry to execute.
    pub entry: FacetHostEntry,
    /// On-disk manifests to load.
    pub manifest_paths: Option<Vec<String>>,
    /// Already-materialized artifacts to load.
    pub artifacts: Option<Vec<JsonValue>>,
    /// Services supplied by the native endpoint.
    pub builtin_catalogue: Vec<ServiceCatalogueEntry>,
}

/// Catalogue and slash metadata returned after a load or reload.
#[derive(Clone, Debug, PartialEq)]
pub struct FacetHostLoadResponse {
    /// Services provided by loaded facets.
    pub catalogue: Vec<ServiceCatalogueEntry>,
    /// Registered slash-command metadata.
    pub slash_commands: Vec<JsonValue>,
}

/// Reload request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetHostReloadRequest {
    /// Host identity.
    pub host_id: String,
}

/// Dispose request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetHostDisposeRequest {
    /// Host identity.
    pub host_id: String,
}

/// Host-to-native service invocation request.
#[derive(Clone, Debug, PartialEq)]
pub struct FacetServiceInvokeRequest {
    /// Host identity.
    pub host_id: JsString,
    /// Chord service call.
    pub call: ServiceCall,
}

/// Host-to-native service invocation result.
#[derive(Clone, Debug, PartialEq)]
pub enum FacetServiceInvokeResult {
    /// JavaScript `undefined`: omit the result field.
    Absent,
    /// A JSON value, including an explicit `null`.
    Present(JsonValue),
}

/// Native-to-host service update event.
#[derive(Clone, Debug, PartialEq)]
pub struct FacetServiceUpdateEvent {
    /// Host identity.
    pub host_id: JsString,
    /// Subscription identity allocated by the receiving host.
    pub subscription_id: JsString,
    /// Canonical Chord update.
    pub update: ServiceProviderUpdate<pi_agent::service::delta::DeltaOp>,
}

/// Run request for one slash command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetSlashRunRequest {
    /// Host identity.
    pub host_id: String,
    /// Command name without `/`.
    pub name: String,
    /// Raw argument text.
    pub args: String,
}

/// Complete request for one slash command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetSlashCompleteRequest {
    /// Host identity.
    pub host_id: String,
    /// Command name without `/`.
    pub name: String,
    /// Prefix supplied to the command's completion callback.
    pub argument_prefix: String,
}

/// Structured facet wire decoding error.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum FacetWireError {
    /// The value failed a facet-specific shape check.
    #[error("invalid {0}")]
    Invalid(&'static str),
    /// The nested service grammar rejected a canonical value.
    #[error(transparent)]
    Service(#[from] WireError),
}

impl FacetServiceInvokeRequest {
    /// Decode `{hostId, call}` while retaining canonical Chord values.
    pub fn from_json(value: &JsonValue) -> Result<Self, FacetWireError> {
        let fields = record(value, "facet service invocation")?;
        assert_keys(fields, &["hostId", "call"], &[])?;
        let host_id = required_string(fields, "hostId", "facet service invocation.hostId")?;
        let call = parse_service_call(required(fields, "call")?)?;
        Ok(Self { host_id, call })
    }

    /// Encode `{hostId, call}`.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        JsonValue::Object(JsObject::from([
            (key("hostId"), JsonValue::String(self.host_id)),
            (key("call"), self.call.into_json()),
        ]))
    }
}

impl FacetServiceInvokeResult {
    /// Decode a result envelope. Absent `result` is not the same as `null`.
    pub fn from_json(value: &JsonValue) -> Result<Self, FacetWireError> {
        let fields = record(value, "facet service invocation result")?;
        assert_keys(fields, &[], &["result"])?;
        match fields.get(&key("result")) {
            None => Ok(Self::Absent),
            Some(value) => Ok(Self::Present(value.clone())),
        }
    }

    /// Encode a result envelope.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::Absent => JsonValue::Object(JsObject::new()),
            Self::Present(value) => JsonValue::Object(JsObject::from([(key("result"), value)])),
        }
    }
}

impl FacetServiceUpdateEvent {
    /// Decode `{hostId, subscriptionId, update}`.
    pub fn from_json(value: &JsonValue) -> Result<Self, FacetWireError> {
        let fields = record(value, "facet service update")?;
        assert_keys(fields, &["hostId", "subscriptionId", "update"], &[])?;
        let host_id = required_string(fields, "hostId", "facet service update.hostId")?;
        let subscription_id = required_string(fields, "subscriptionId", "facet service update.subscriptionId")?;
        let update = parse_service_provider_update(required(fields, "update")?)?;
        Ok(Self { host_id, subscription_id, update })
    }

    /// Encode a service update event.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        JsonValue::Object(JsObject::from([
            (key("hostId"), JsonValue::String(self.host_id)),
            (key("subscriptionId"), JsonValue::String(self.subscription_id)),
            (key("update"), self.update.into_json()),
        ]))
    }
}

/// Convert a canonical service catalogue to its wire array.
#[must_use]
pub fn catalogue_into_json(catalogue: Vec<ServiceCatalogueEntry>) -> JsonValue {
    JsonValue::Array(catalogue.into_iter().map(ServiceCatalogueEntry::into_json).collect())
}

/// Parse a canonical service catalogue.
pub fn catalogue_from_json(value: &JsonValue) -> Result<Vec<ServiceCatalogueEntry>, FacetWireError> {
    parse_service_catalogue(value).map_err(FacetWireError::from)
}

fn key(name: &str) -> JsString {
    JsString::from_utf8(name)
}

fn record<'a>(value: &'a JsonValue, description: &'static str) -> Result<&'a JsObject, FacetWireError> {
    value.as_object().ok_or(FacetWireError::Invalid(description))
}

fn required<'a>(fields: &'a JsObject, name: &str) -> Result<&'a JsonValue, FacetWireError> {
    fields.get(&key(name)).ok_or(FacetWireError::Invalid("missing required facet field"))
}

fn required_string(
    fields: &JsObject,
    name: &str,
    description: &'static str,
) -> Result<JsString, FacetWireError> {
    let value = required(fields, name)?;
    let Some(text) = value.as_str() else {
        return Err(FacetWireError::Invalid(description));
    };
    if text.as_utf16().is_empty() {
        return Err(FacetWireError::Invalid(description));
    }
    Ok(text.clone())
}

fn assert_keys(
    fields: &JsObject,
    required_names: &[&str],
    optional_names: &[&str],
) -> Result<(), FacetWireError> {
    let required = required_names.iter().map(|name| key(name)).collect::<Vec<_>>();
    let optional = optional_names.iter().map(|name| key(name)).collect::<Vec<_>>();
    if fields.keys().any(|field| {
        !required.iter().any(|required| required == field)
            && !optional.iter().any(|optional| optional == field)
    }) {
        return Err(FacetWireError::Invalid("facet wire field set"));
    }
    if required.iter().any(|field| !fields.contains_key(field)) {
        return Err(FacetWireError::Invalid("missing required facet field"));
    }
    Ok(())
}
