//! Presentation and session plugin service contracts.
//!
//! Source mapping: `experimental/services/plugins.ts`. Plugin bundle
//! execution remains owned by the product facet bridge; this module only
//! describes the service wire shapes.

use pi_agent::service::error::ServiceError;
use pi_agent::service::value::JsonValue;

use super::{array, json_object, object, required, string, ProductJsonConvert};

/// Chord service identifier for server-built presentation plugin generations.
pub const PRESENTATION_PLUGINS_ID: &str = "pi.presentation-plugins";
/// Chord service identifier for the attached session worker's plugins.
pub const SESSION_PLUGINS_ID: &str = "pi.session-plugins";

/// Presentation plugin member names.
pub const PRESENTATION_PLUGINS_PREPARE_SESSION_MEMBER: &str = "prepareSession";
pub const PRESENTATION_PLUGINS_RELOAD_MEMBER: &str = "reload";
/// Session plugin member name.
pub const SESSION_PLUGINS_RELOAD_MEMBER: &str = "reload";

/// Object key carrying presentation facet bundle artifacts.
pub const PRESENTATION_FACET_BUNDLES_KEY: &str = "presentationFacetBundles";

/// Request passed to `PresentationPlugins.prepareSession`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareSessionRequest {
    /// Session to prepare.
    pub session_id: String,
    /// Explicit package selection, or `null` for the source default.
    pub package_paths: Option<Vec<String>>,
}

/// Known result envelope returned by the plugin host.
///
/// Each bundle is intentionally opaque to native code. The extension-host
/// bridge owns the bundle schema and only forwards canonical JSON values.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparePluginsResult {
    /// Bundle artifacts for presentation facets.
    pub presentation_facet_bundles: Vec<JsonValue>,
}

impl ProductJsonConvert for PrepareSessionRequest {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "plugin preparation request")?;
        let session_id = string(
            required(fields, "sessionId", "plugin preparation request.sessionId")?,
            "plugin preparation request.sessionId",
        )?;
        let package_paths = nullable_string_array(required(
            fields,
            "packagePaths",
            "plugin preparation request.packagePaths",
        )?)?;
        Ok(Self { session_id, package_paths })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let package_paths = match self.package_paths {
            Some(paths) => JsonValue::Array(
                paths
                    .into_iter()
                    .map(|path| JsonValue::String(path.into()))
                    .collect(),
            ),
            None => JsonValue::Null,
        };
        Ok(json_object([
            ("sessionId", JsonValue::String(self.session_id.into())),
            ("packagePaths", package_paths),
        ]))
    }
}

impl ProductJsonConvert for PreparePluginsResult {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "plugin preparation result")?;
        let bundles = array(
            required(
                fields,
                PRESENTATION_FACET_BUNDLES_KEY,
                "plugin preparation result.presentationFacetBundles",
            )?,
            "plugin preparation result.presentationFacetBundles",
        )?
        .to_vec();
        Ok(Self { presentation_facet_bundles: bundles })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([(
            PRESENTATION_FACET_BUNDLES_KEY,
            JsonValue::Array(self.presentation_facet_bundles),
        )]))
    }
}

fn nullable_string_array(value: &JsonValue) -> Result<Option<Vec<String>>, ServiceError> {
    if value.is_null() {
        return Ok(None);
    }
    let values = array(value, "plugin preparation request.packagePaths")?;
    values
        .iter()
        .map(|value| string(value, "plugin preparation request.packagePaths[]"))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

