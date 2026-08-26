//! Product-side telemetry adapter selection.

use std::sync::Arc;

use pi_agent::telemetry::TelemetryContext;

use crate::core::settings::SettingsManager;

/// Resolves the telemetry context for a session.
///
/// The C18 install-telemetry gate owns opt-in. No exporter backend is shipped
/// in this port yet, so both paths remain passive and dependency-free.
#[must_use]
pub fn resolve_session_telemetry(settings: &SettingsManager) -> Arc<dyn TelemetryContext> {
    if !crate::core::provider_attribution::is_install_telemetry_enabled(settings) {
        return pi_agent::telemetry::noop_context();
    }
    installed_exporter_context()
}

fn installed_exporter_context() -> Arc<dyn TelemetryContext> {
    pi_agent::telemetry::noop_context()
}
