//! Local presentation UI service contracts.
//!
//! Source mapping: `experimental/services/presentation-ui.ts`. The actual
//! TUI bridge uses the existing pi-ext select/notify methods; this module
//! only carries the source-shaped arguments and member names.

use pi_agent::service::error::ServiceError;
use pi_agent::service::value::JsonValue;

use super::{ProductJsonConvert, array, object, required, string};

/// Chord service identifier for process-local presentation UI.
pub const PRESENTATION_UI_ID: &str = "pi.local.presentation-ui";

/// Wire method that asks the local presentation UI service to open a selection dialog.
pub const PRESENTATION_UI_SELECT_MEMBER: &str = "select";
/// Wire method that asks the local presentation UI service to show a status message.
pub const PRESENTATION_UI_SHOW_STATUS_MEMBER: &str = "showStatus";

/// One selectable item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentationSelectItem {
    /// Value returned when selected.
    pub value: String,
    /// Display label.
    pub label: String,
    /// Optional explanatory text.
    pub description: Option<String>,
}

/// Positional arguments of `PresentationUI.select`.
///
/// The source method takes `(title, items, selectedValue?)` rather than one
/// object, so the conversion is a two- or three-element Chord argument array.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentationSelectArgs {
    /// Dialog title.
    pub title: String,
    /// Items shown by the dialog.
    pub items: Vec<PresentationSelectItem>,
    /// Previously selected value, or `undefined` when no value is selected.
    pub selected_value: Option<String>,
}

impl ProductJsonConvert for PresentationSelectItem {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "presentation select item")?;
        let value = string(
            required(fields, "value", "presentation select item.value")?,
            "presentation select item.value",
        )?;
        let label = string(
            required(fields, "label", "presentation select item.label")?,
            "presentation select item.label",
        )?;
        let description = match fields.get(&pi_agent::service::value::JsString::from_utf8(
            "description",
        )) {
            None => None,
            Some(value) if value.is_null() => {
                return Err(super::invalid("presentation select item.description"));
            }
            Some(value) => Some(string(value, "presentation select item.description")?),
        };
        Ok(Self {
            value,
            label,
            description,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("value".into(), JsonValue::String(self.value.into()));
        fields.insert("label".into(), JsonValue::String(self.label.into()));
        if let Some(description) = self.description {
            fields.insert("description".into(), JsonValue::String(description.into()));
        }
        Ok(JsonValue::Object(fields))
    }
}

impl ProductJsonConvert for PresentationSelectArgs {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let values = array(&value, "presentation select arguments")?;
        if !(2..=3).contains(&values.len()) {
            return Err(super::invalid("presentation select arguments"));
        }
        let title = string(&values[0], "presentation select title")?;
        let items = array(&values[1], "presentation select items")?
            .iter()
            .cloned()
            .map(PresentationSelectItem::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let selected_value = match values.get(2) {
            None => None,
            Some(value) => Some(string(value, "presentation select selectedValue")?),
        };
        Ok(Self {
            title,
            items,
            selected_value,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let items = self
            .items
            .into_iter()
            .map(PresentationSelectItem::into_json)
            .collect::<Result<Vec<_>, _>>()?;
        let mut values = vec![
            JsonValue::String(self.title.into()),
            JsonValue::Array(items),
        ];
        if let Some(selected_value) = self.selected_value {
            values.push(JsonValue::String(selected_value.into()));
        }
        Ok(JsonValue::Array(values))
    }
}
