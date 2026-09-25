//! Development-only product plugin package and facet-host integration.

use pi_agent::service::value::{JsString, JsonValue};

pub mod bridge;
pub mod package;
pub mod profile;

pub use bridge::{
    FacetHostTransport, PluginFacetHost, PluginFacetHostOptions, RemotePluginFacetHost,
};
pub use package::{
    PluginPackageBuilder, RemotePluginPackageBuilder, normalize_plugin_package_paths,
    plugin_build_directory_name, try_normalize_plugin_package_paths,
};
pub use pi_ext::facet::{PI_PLUGIN_API, PRESENTATION_FACET_BUNDLES_KEY};
pub use profile::{
    PLUGIN_PACKAGE_PROFILE_VERSION, PluginProfileError, read_server_plugin_package_profile,
    read_session_plugin_package_profile, remove_server_plugin_package_profile,
    remove_session_plugin_package_profile, restore_server_plugin_package_profile,
    write_server_plugin_package_profile, write_session_plugin_package_profile,
};

/// Encodes presentation-only facet artifacts under the stable service result key.
#[must_use]
pub fn create_presentation_facet_data(artifacts: Vec<JsonValue>) -> JsonValue {
    JsonValue::Object(
        [(
            JsString::from_utf8(PRESENTATION_FACET_BUNDLES_KEY),
            JsonValue::Array(artifacts),
        )]
        .into_iter()
        .collect(),
    )
}
