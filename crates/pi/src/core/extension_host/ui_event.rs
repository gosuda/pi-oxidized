//! Product projection of extension UI activity.
//!
//! `pi_ext` wire types are the transport contract; these are the product's.
//! Conversion runs exactly once, in `spawn_event_pump`, through the impls
//! below. Coherence permits exactly one `From`/`TryFrom` impl per pair, so no
//! second, divergent conversion can exist anywhere in the workspace.
//!
//! The types carry no serde derives: a product-type edit cannot move a wire
//! byte (ARC11 witness parity is structural).

use pi_ext::protocol::{NotifyLevel, NotifyRequest, ThemeSet, ThemeWire, UiControl};
use pi_ext::sanitize::SanitizedSlot;

/// Sanitized extension UI activity delivered to an active product mode.
#[derive(Debug, Clone)]
pub enum ExtensionUiEvent {
    /// Fire-and-forget notification.
    Notify(ExtensionNotice),
    /// Sanitized keyed slot update.
    Slot(SanitizedSlot),
    /// Keyed slot disposal.
    Dispose {
        /// Stable extension widget key to remove.
        key: String,
    },
    /// Extension `setTheme` application request, resolved to exactly one form.
    ThemeSet(ExtensionThemeRequest),
    /// Extension fire-and-forget UI control (`ui.setStatus`, `ui.setEditorText`, …).
    UiControl(ExtensionUiControl),
}

/// Notification severity as the product consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtensionNoticeLevel {
    /// Informational.
    #[default]
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

/// Fire-and-forget extension notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionNotice {
    /// Notification text.
    pub message: String,
    /// Severity.
    pub level: ExtensionNoticeLevel,
}

/// Product-owned `ui.*` control. Field normalization happens once, here:
/// `SetTitle` collapses `None` to `""` and `SetWorkingIndicator` reduces the
/// upstream options object to the only fact the product honors (hide on an
/// empty `frames` array).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionUiControl {
    /// `ui.setStatus(key, text)`; `None` clears the keyed entry.
    /// `Some("")` is preserved verbatim: the RPC surface forwards it as
    /// `"statusText": ""`, distinct from absent.
    SetStatus {
        /// Status entry key.
        key: String,
        /// Status text (`None` clears).
        text: Option<String>,
    },
    /// `ui.setWorkingMessage(message)`; `None` restores the default.
    SetWorkingMessage {
        /// Override for the streaming loader text.
        message: Option<String>,
    },
    /// `ui.setWorkingVisible(visible)`.
    SetWorkingVisible {
        /// Whether the working indicator is shown while streaming.
        visible: bool,
    },
    /// `ui.setWorkingIndicator(options)`: the product honors hide-or-nothing.
    /// Custom frames are not portable to the native braille spinner (ledgered).
    SetWorkingIndicator {
        /// Only `frames: []` hides the indicator; everything else is a no-op.
        hide: bool,
    },
    /// `ui.setHiddenThinkingLabel(label)`; `None` restores the default.
    SetHiddenThinkingLabel {
        /// Label shown instead of hidden thinking blocks.
        label: Option<String>,
    },
    /// `ui.setTitle(title)`; an absent title clears.
    SetTitle {
        /// Terminal title text (`""` clears).
        title: String,
    },
    /// `ui.pasteToEditor(text)`: insert at the cursor.
    PasteToEditor {
        /// Text to insert.
        text: String,
    },
    /// `ui.setEditorText(text)`: replace the editor content.
    SetEditorText {
        /// Replacement text.
        text: String,
    },
    /// `ui.setToolsExpanded(expanded)`.
    SetToolsExpanded {
        /// Whether tool blocks render expanded.
        expanded: bool,
    },
}

/// Extension `setTheme`: the wire's "exactly one of `name`/`theme`" resolved
/// once, at the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionThemeRequest {
    /// Object form (`setThemeInstance`): apply, never persist. When both
    /// forms are present the object wins, matching the prior mode behavior.
    Instance(ThemeWire),
    /// String form: apply the named theme (or `light/dark` pair); persist
    /// only when the host asked.
    Named {
        /// Raw theme name or slash pair.
        name: String,
        /// Whether the product should persist the setting on success.
        persist: bool,
    },
}

/// `theme.set` violated its exactly-one-of contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedThemeSet;

impl From<NotifyLevel> for ExtensionNoticeLevel {
    fn from(level: NotifyLevel) -> Self {
        match level {
            NotifyLevel::Info => Self::Info,
            NotifyLevel::Warning => Self::Warning,
            NotifyLevel::Error => Self::Error,
        }
    }
}

impl From<NotifyRequest> for ExtensionNotice {
    fn from(request: NotifyRequest) -> Self {
        Self {
            message: request.message,
            level: request.level.into(),
        }
    }
}

impl From<UiControl> for ExtensionUiControl {
    fn from(control: UiControl) -> Self {
        match control {
            UiControl::SetStatus { key, text } => Self::SetStatus { key, text },
            UiControl::SetWorkingMessage { message } => Self::SetWorkingMessage { message },
            UiControl::SetWorkingVisible { visible } => Self::SetWorkingVisible { visible },
            UiControl::SetWorkingIndicator { options } => Self::SetWorkingIndicator {
                hide: options
                    .as_ref()
                    .and_then(|value| value.get("frames"))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(Vec::is_empty),
            },
            UiControl::SetHiddenThinkingLabel { label } => Self::SetHiddenThinkingLabel { label },
            UiControl::SetTitle { title } => Self::SetTitle {
                title: title.unwrap_or_default(),
            },
            UiControl::PasteToEditor { text } => Self::PasteToEditor { text },
            UiControl::SetEditorText { text } => Self::SetEditorText { text },
            UiControl::SetToolsExpanded { expanded } => Self::SetToolsExpanded { expanded },
        }
    }
}

impl TryFrom<ThemeSet> for ExtensionThemeRequest {
    type Error = MalformedThemeSet;

    fn try_from(set: ThemeSet) -> Result<Self, Self::Error> {
        // Object form wins when both are present; it never persists.
        if let Some(theme) = set.theme {
            return Ok(Self::Instance(theme));
        }
        match set.name {
            Some(name) => Ok(Self::Named {
                name,
                persist: set.persist,
            }),
            None => Err(MalformedThemeSet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notify_level_maps_one_to_one() {
        assert_eq!(
            ExtensionNoticeLevel::from(NotifyLevel::Info),
            ExtensionNoticeLevel::Info
        );
        assert_eq!(
            ExtensionNoticeLevel::from(NotifyLevel::Warning),
            ExtensionNoticeLevel::Warning
        );
        assert_eq!(
            ExtensionNoticeLevel::from(NotifyLevel::Error),
            ExtensionNoticeLevel::Error
        );
    }

    #[test]
    fn ui_control_hides_only_on_empty_frames_array() {
        let convert = |options: Option<serde_json::Value>| {
            let control: ExtensionUiControl = UiControl::SetWorkingIndicator { options }.into();
            let ExtensionUiControl::SetWorkingIndicator { hide } = control else {
                panic!("expected SetWorkingIndicator");
            };
            hide
        };
        assert!(!convert(None), "no options is a no-op");
        assert!(!convert(Some(json!({}))), "missing frames is a no-op");
        assert!(convert(Some(json!({"frames": []}))), "empty frames hides");
        assert!(
            !convert(Some(json!({"frames": ["a"]}))),
            "custom frames no-op"
        );
        assert!(
            !convert(Some(json!({"frames": 3}))),
            "non-array frames no-op"
        );
    }

    #[test]
    fn ui_control_normalizes_absent_title_to_empty() {
        let control: ExtensionUiControl = UiControl::SetTitle { title: None }.into();
        assert_eq!(
            control,
            ExtensionUiControl::SetTitle {
                title: String::new()
            }
        );
        let control: ExtensionUiControl = UiControl::SetTitle {
            title: Some("x".to_owned()),
        }
        .into();
        assert_eq!(
            control,
            ExtensionUiControl::SetTitle {
                title: "x".to_owned()
            }
        );
    }

    #[test]
    fn ui_control_preserves_empty_status_text_as_some() {
        // The RPC surface forwards Some("") as `"statusText": ""`, distinct
        // from absent; collapsing it here would change the RPC JSON.
        let control: ExtensionUiControl = UiControl::SetStatus {
            key: "k".to_owned(),
            text: Some(String::new()),
        }
        .into();
        assert_eq!(
            control,
            ExtensionUiControl::SetStatus {
                key: "k".to_owned(),
                text: Some(String::new())
            }
        );
    }

    #[test]
    fn theme_set_object_form_wins_over_name() {
        let theme = ThemeWire {
            name: None,
            source_path: None,
            color_mode: "truecolor".to_owned(),
            fg: Default::default(),
            bg: Default::default(),
        };
        let request = ExtensionThemeRequest::try_from(ThemeSet {
            name: Some("named".to_owned()),
            theme: Some(theme.clone()),
            persist: true,
        })
        .expect("both set resolves");
        assert_eq!(request, ExtensionThemeRequest::Instance(theme));
    }

    #[test]
    fn theme_set_named_carries_persist() {
        for persist in [false, true] {
            let request = ExtensionThemeRequest::try_from(ThemeSet {
                name: Some("classic-dark".to_owned()),
                theme: None,
                persist,
            })
            .expect("name form resolves");
            assert_eq!(
                request,
                ExtensionThemeRequest::Named {
                    name: "classic-dark".to_owned(),
                    persist
                }
            );
        }
    }

    #[test]
    fn theme_set_without_name_or_theme_is_rejected() {
        let result = ExtensionThemeRequest::try_from(ThemeSet {
            name: None,
            theme: None,
            persist: true,
        });
        assert_eq!(result, Err(MalformedThemeSet));
    }
}
