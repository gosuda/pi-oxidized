//! Explicit `serde_json` adapters for the canonical chord value tree.
//!
//! `from_serde_json` deliberately coerces `serde_json`'s integer variants into
//! binary64 because binary64 is the Chord number domain. The reverse adapter
//! refuses nonfinite numbers and unpaired UTF-16 code units instead of
//! replacing or narrowing them. Both directions use explicit work stacks and
//! never recurse through a value tree.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value as SerdeValue};

use super::{JsObject, JsString, JsonValue, ValueError};

/// Converts a `serde_json` tree into the canonical domain.
///
/// Every `serde_json` number is intentionally converted to `f64`; a `u64` or
/// `i64` outside the exact binary64 integer range therefore rounds exactly as
/// JavaScript number construction does.
#[must_use]
pub fn from_serde_json(value: SerdeValue) -> JsonValue {
    let mut stack: Vec<FromFrame> = Vec::new();
    let mut pending = Some(value);
    let mut root: Option<JsonValue> = None;

    loop {
        if let Some(input) = pending.take() {
            match input {
                SerdeValue::Null => attach_from(&mut stack, &mut root, JsonValue::Null),
                SerdeValue::Bool(value) => {
                    attach_from(&mut stack, &mut root, JsonValue::Bool(value));
                }
                SerdeValue::Number(value) => {
                    attach_from(
                        &mut stack,
                        &mut root,
                        JsonValue::Number(number_to_f64(&value)),
                    );
                }
                SerdeValue::String(value) => {
                    attach_from(
                        &mut stack,
                        &mut root,
                        JsonValue::String(JsString::from(value)),
                    );
                }
                SerdeValue::Array(mut values) => {
                    let capacity = values.len();
                    // `rest.pop()` drives depth-first conversion. Reverse once
                    // so the resulting array retains source order.
                    values.reverse();
                    let first = values.pop();
                    stack.push(FromFrame::Array {
                        built: Vec::with_capacity(capacity),
                        rest: values,
                    });
                    pending = first;
                    continue;
                }
                SerdeValue::Object(values) => {
                    let mut values: Vec<(String, SerdeValue)> = values.into_iter().collect();
                    values.reverse();
                    let (key, first) = if let Some((key, value)) = values.pop() {
                        (Some(JsString::from(key)), Some(value))
                    } else {
                        (None, None)
                    };
                    stack.push(FromFrame::Object {
                        built: BTreeMap::new(),
                        key,
                        rest: values,
                    });
                    pending = first;
                    continue;
                }
            }
            continue;
        }

        let mut close = false;
        if let Some(frame) = stack.last_mut() {
            match frame {
                FromFrame::Array { rest, .. } => {
                    if let Some(next) = rest.pop() {
                        pending = Some(next);
                    } else {
                        close = true;
                    }
                }
                FromFrame::Object { key, rest, .. } => {
                    if let Some((next_key, next)) = rest.pop() {
                        *key = Some(JsString::from(next_key));
                        pending = Some(next);
                    } else {
                        close = true;
                    }
                }
            }
        }

        if close {
            if let Some(frame) = stack.pop() {
                let value = match frame {
                    FromFrame::Array { built, .. } => JsonValue::Array(built),
                    FromFrame::Object { built, .. } => JsonValue::Object(built),
                };
                attach_from(&mut stack, &mut root, value);
            }
            continue;
        }

        if stack.is_empty() {
            return root.unwrap_or(JsonValue::Null);
        }
    }
}

/// Converts the canonical tree into `serde_json` without lossy substitutions.
///
/// # Errors
/// Returns [`ValueError::NonFinite`] for NaN or either infinity, and
/// [`ValueError::LoneSurrogate`] when a string or object key contains an
/// unpaired UTF-16 code unit. Finite scalar strings and numbers are converted
/// exactly into `serde_json`'s representable domain.
pub fn try_into_serde_json(value: JsonValue) -> Result<SerdeValue, ValueError> {
    validate_for_serde(&value)?;

    // `JsonValue` implements `Drop`, so containers cannot be destructured out
    // of an owned value; each pending value is matched by `&mut` and its
    // fields are taken, which still consumes the whole input tree.
    let mut frames: Vec<IntoFrame> = Vec::new();
    let mut pending = Some(value);
    let mut completed: Option<SerdeValue> = None;
    loop {
        if let Some(value) = completed.take() {
            if let Some(root) = attach_into(&mut frames, value) {
                return Ok(root);
            }
            continue;
        }

        if let Some(mut value) = pending.take() {
            match &mut value {
                JsonValue::Null => completed = Some(SerdeValue::Null),
                JsonValue::Bool(value) => completed = Some(SerdeValue::Bool(*value)),
                JsonValue::Number(value) => {
                    let Some(number) = Number::from_f64(*value) else {
                        return Err(ValueError::NonFinite(*value));
                    };
                    completed = Some(SerdeValue::Number(number));
                }
                JsonValue::String(value) => {
                    completed = Some(SerdeValue::String(into_utf8(value)?));
                }
                JsonValue::Array(values) => {
                    let capacity = values.len();
                    let mut rest = std::mem::take(values).into_iter();
                    let first = rest.next();
                    frames.push(IntoFrame::Array {
                        built: Vec::with_capacity(capacity),
                        rest,
                    });
                    pending = first;
                }
                JsonValue::Object(values) => {
                    let mut rest = std::mem::take(values).into_iter();
                    let (key, first) = if let Some((key, value)) = rest.next() {
                        (Some(into_utf8(&key)?), Some(value))
                    } else {
                        (None, None)
                    };
                    frames.push(IntoFrame::Object {
                        built: Map::new(),
                        key,
                        rest,
                    });
                    pending = first;
                }
            }
            continue;
        }

        let mut close = false;
        if let Some(frame) = frames.last_mut() {
            match frame {
                IntoFrame::Array { rest, .. } => {
                    if let Some(next) = rest.next() {
                        pending = Some(next);
                    } else {
                        close = true;
                    }
                }
                IntoFrame::Object { key, rest, .. } => {
                    if let Some((next_key, next)) = rest.next() {
                        *key = Some(into_utf8(&next_key)?);
                        pending = Some(next);
                    } else {
                        close = true;
                    }
                }
            }
        }

        if close && let Some(frame) = frames.pop() {
            completed = Some(match frame {
                IntoFrame::Array { built, .. } => SerdeValue::Array(built),
                IntoFrame::Object { built, .. } => SerdeValue::Object(built),
            });
            continue;
        }

        if frames.is_empty() {
            return Ok(SerdeValue::Null);
        }
    }
}

/// One open container while converting `serde_json` into the canonical
/// domain.
enum FromFrame {
    /// An array collecting converted items in source order.
    Array {
        /// Items converted so far.
        built: Vec<JsonValue>,
        /// Siblings not yet converted, stored in reverse for `pop` order.
        rest: Vec<SerdeValue>,
    },
    /// An object collecting converted entries in sorted key order.
    Object {
        /// Entries converted so far.
        built: JsObject,
        /// The key waiting for its value.
        key: Option<JsString>,
        /// Entries not yet converted, stored in reverse for `pop` order.
        rest: Vec<(String, SerdeValue)>,
    },
}

/// One open container while converting the canonical domain into
/// `serde_json`.
enum IntoFrame {
    /// An array collecting converted items in source order.
    Array {
        /// Items converted so far.
        built: Vec<SerdeValue>,
        /// Siblings not yet converted.
        rest: std::vec::IntoIter<JsonValue>,
    },
    /// An object collecting converted entries in sorted key order.
    Object {
        /// Entries converted so far.
        built: Map<String, SerdeValue>,
        /// The key waiting for its value.
        key: Option<String>,
        /// Entries not yet converted.
        rest: std::collections::btree_map::IntoIter<JsString, JsonValue>,
    },
}

/// Files one converted value into the innermost `FromFrame`, or into the
/// root.
fn attach_from(stack: &mut [FromFrame], root: &mut Option<JsonValue>, value: JsonValue) {
    match stack.last_mut() {
        None => *root = Some(value),
        Some(FromFrame::Array { built, .. }) => built.push(value),
        Some(FromFrame::Object { built, key, .. }) => {
            if let Some(key) = key.take() {
                built.insert(key, value);
            }
        }
    }
}

/// Files one completed `serde_json` value into the innermost `IntoFrame`, or
/// returns it once no frame remains.
fn attach_into(frames: &mut [IntoFrame], value: SerdeValue) -> Option<SerdeValue> {
    match frames.last_mut() {
        None => Some(value),
        Some(IntoFrame::Array { built, .. }) => {
            built.push(value);
            None
        }
        Some(IntoFrame::Object { built, key, .. }) => {
            if let Some(key) = key.take() {
                built.insert(key, value);
            }
            None
        }
    }
}

/// Converts one `serde_json` number to binary64: wide integers round and a
/// JSON numeric overflow maps to the matching infinity, exactly as JavaScript
/// number construction does.
fn number_to_f64(value: &Number) -> f64 {
    if let Some(value) = value.as_f64() {
        return value;
    }
    let text = value.to_string();
    match text.parse::<f64>() {
        Ok(value) => value,
        Err(_) if text.starts_with('-') => f64::NEG_INFINITY,
        Err(_) => f64::INFINITY,
    }
}

/// Converts one UTF-16 string to owned UTF-8, or reports the index of its
/// first unpaired code unit.
fn into_utf8(value: &JsString) -> Result<String, ValueError> {
    value
        .try_to_utf8()
        .map_err(|error| ValueError::LoneSurrogate { index: error.index })
}

/// Checks every scalar and key before the reverse adapter allocates an output
/// tree. This keeps an error path from leaving a deeply nested `serde_json`
/// tree whose recursive drop glue could overflow the stack.
fn validate_for_serde(value: &JsonValue) -> Result<(), ValueError> {
    let mut work = vec![value];
    while let Some(value) = work.pop() {
        match value {
            JsonValue::Number(value) => {
                if Number::from_f64(*value).is_none() {
                    return Err(ValueError::NonFinite(*value));
                }
            }
            JsonValue::String(value) => {
                if let Err(error) = value.try_to_utf8() {
                    return Err(ValueError::LoneSurrogate { index: error.index });
                }
            }
            JsonValue::Array(values) => work.extend(values),
            JsonValue::Object(values) => {
                for (key, value) in values {
                    if let Err(error) = key.try_to_utf8() {
                        return Err(ValueError::LoneSurrogate { index: error.index });
                    }
                    work.push(value);
                }
            }
            JsonValue::Null | JsonValue::Bool(_) => {}
        }
    }
    Ok(())
}
