//! JSON codec for the canonical chord value tree.
//!
//! Syntax validation is delegated exactly once to `serde_json`'s borrowed
//! [`RawValue`] capture — the crate's only `raw_value` callsite — which walks
//! the whole document with an explicit stack and no depth limit. The captured
//! text is then materialized into the canonical [`JsonValue`] domain by a byte
//! cursor: strings decode to UTF-16 code units (surrogate pairs stay stored as
//! split units; lone surrogates are preserved), numbers parse straight to
//! binary64 (negative zero kept, JSON overflow becoming ±infinity), and
//! objects collect into [`JsObject`] where duplicate keys resolve last-wins,
//! matching ECMAScript `JSON.parse`. Materialization and writing are both
//! iterative, so arbitrarily deep documents never touch the Rust stack.

use std::collections::btree_map;

use serde_json::value::RawValue;
use thiserror::Error;

use super::{JsObject, JsString, JsonValue};

/// JSON parsing or serialization failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum JsonError {
    /// `serde_json` rejected the document, so it is not valid JSON syntax.
    #[error("invalid JSON syntax: {message}")]
    Syntax {
        /// The `serde_json` error text.
        message: String,
    },
    /// The cursor disagreed with the grammar `serde_json` had already
    /// validated. Unreachable absent a `serde_json` behavior change; never
    /// masks an input property.
    #[error("canonical materialization failed: {message}")]
    Internal {
        /// Which internal invariant failed.
        message: String,
    },
}

/// Parses one JSON document into the canonical value domain.
///
/// The document is validated once via `serde_json::from_str::<&RawValue>`,
/// then materialized with an explicit container stack and no nesting cap:
/// documents deeper than `serde_json`'s recursive `Value` limit parse fine, and
/// only [`super::is_json_value`] admission applies depth bounds.
///
/// # Errors
/// Returns [`JsonError::Syntax`] when the document is not valid JSON, and
/// [`JsonError::Internal`] only if the cursor met validated text it could not
/// follow (a bug, never an input property).
pub fn parse_json(text: &str) -> Result<JsonValue, JsonError> {
    let raw = serde_json::from_str::<&RawValue>(text).map_err(|error| JsonError::Syntax {
        message: error.to_string(),
    })?;
    materialize(raw.get())
}

/// One open container on the materialization stack.
enum Frame {
    Array {
        items: Vec<JsonValue>,
    },
    Object {
        map: JsObject,
        key: Option<JsString>,
    },
}

/// Where the materializer stands relative to the container stack.
enum Phase {
    /// A value is expected at the cursor.
    Value,
    /// The top array is fresh and wants a first element or immediate close.
    ArrayEntry,
    /// The top object is fresh and wants a first key or immediate close.
    ObjectEntry,
    /// A value was just attached; containers resume or close.
    After,
}

/// Materializes one validated JSON document into the canonical tree.
fn materialize(text: &str) -> Result<JsonValue, JsonError> {
    let mut cursor = Cursor { text, pos: 0 };
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<JsonValue> = None;
    let mut phase = Phase::Value;
    loop {
        phase = match phase {
            Phase::Value => step_value(&mut cursor, &mut stack, &mut root)?,
            Phase::ArrayEntry => step_array_entry(&mut cursor, &mut stack, &mut root)?,
            Phase::ObjectEntry => step_object_entry(&mut cursor, &mut stack, &mut root)?,
            Phase::After => {
                if stack.is_empty() {
                    let Some(value) = root.take() else {
                        return Err(internal("document without root"));
                    };
                    if cursor.skip_ws().is_some() {
                        return Err(internal("trailing content"));
                    }
                    return Ok(value);
                }
                step_after(&mut cursor, &mut stack, &mut root)?
            }
        };
    }
}

/// Runs one `Phase::Value` step: attaches the next scalar or opens a
/// container, then reports the phase that follows.
fn step_value(
    cursor: &mut Cursor<'_>,
    stack: &mut Vec<Frame>,
    root: &mut Option<JsonValue>,
) -> Result<Phase, JsonError> {
    let Some(byte) = cursor.skip_ws() else {
        return Err(internal("expected a value"));
    };
    match byte {
        b'n' => {
            cursor.expect_ident("null")?;
            attach(stack, root, JsonValue::Null)?;
        }
        b't' => {
            cursor.expect_ident("true")?;
            attach(stack, root, JsonValue::Bool(true))?;
        }
        b'f' => {
            cursor.expect_ident("false")?;
            attach(stack, root, JsonValue::Bool(false))?;
        }
        b'"' => {
            let string = cursor.parse_string()?;
            attach(stack, root, JsonValue::String(string))?;
        }
        b'-' | b'0'..=b'9' => {
            let number = cursor.parse_number()?;
            attach(stack, root, JsonValue::Number(number))?;
        }
        b'[' => {
            cursor.bump();
            stack.push(Frame::Array { items: Vec::new() });
            return Ok(Phase::ArrayEntry);
        }
        b'{' => {
            cursor.bump();
            stack.push(Frame::Object {
                map: JsObject::new(),
                key: None,
            });
            return Ok(Phase::ObjectEntry);
        }
        _ => return Err(internal("unexpected byte in validated text")),
    }
    Ok(Phase::After)
}

/// Runs one `Phase::ArrayEntry` step: closes a fresh empty array or defers
/// to `Phase::Value` for the first element.
fn step_array_entry(
    cursor: &mut Cursor<'_>,
    stack: &mut Vec<Frame>,
    root: &mut Option<JsonValue>,
) -> Result<Phase, JsonError> {
    match cursor.skip_ws() {
        Some(b']') => {
            cursor.bump();
            let Some(Frame::Array { items }) = stack.pop() else {
                return Err(internal("array close without array"));
            };
            attach(stack, root, JsonValue::Array(items))?;
            Ok(Phase::After)
        }
        // Any other byte starts the first element; Value reads it.
        Some(_) => Ok(Phase::Value),
        None => Err(internal("unterminated array")),
    }
}

/// Runs one `Phase::ObjectEntry` step: reads the first key or closes a fresh
/// empty object.
fn step_object_entry(
    cursor: &mut Cursor<'_>,
    stack: &mut Vec<Frame>,
    root: &mut Option<JsonValue>,
) -> Result<Phase, JsonError> {
    let Some(byte) = cursor.skip_ws() else {
        return Err(internal("unterminated object"));
    };
    match byte {
        b'"' => {
            read_object_key(cursor, stack)?;
            Ok(Phase::Value)
        }
        b'}' => {
            cursor.bump();
            let Some(Frame::Object { map, .. }) = stack.pop() else {
                return Err(internal("object close without object"));
            };
            attach(stack, root, JsonValue::Object(map))?;
            Ok(Phase::After)
        }
        _ => Err(internal("expected object key")),
    }
}

/// Runs one `Phase::After` step on a nonempty stack: consumes the next
/// comma (and object key) or closes the innermost container.
fn step_after(
    cursor: &mut Cursor<'_>,
    stack: &mut Vec<Frame>,
    root: &mut Option<JsonValue>,
) -> Result<Phase, JsonError> {
    let in_array = matches!(stack.last(), Some(Frame::Array { .. }));
    let Some(byte) = cursor.skip_ws() else {
        return Err(internal("unterminated container"));
    };
    match (in_array, byte) {
        (true, b',') => {
            cursor.bump();
            Ok(Phase::Value)
        }
        (true, b']') => {
            cursor.bump();
            let Some(Frame::Array { items }) = stack.pop() else {
                return Err(internal("array close without array"));
            };
            attach(stack, root, JsonValue::Array(items))?;
            Ok(Phase::After)
        }
        (false, b',') => {
            cursor.bump();
            read_object_key(cursor, stack)?;
            Ok(Phase::Value)
        }
        (false, b'}') => {
            cursor.bump();
            let Some(Frame::Object { map, .. }) = stack.pop() else {
                return Err(internal("object close without object"));
            };
            attach(stack, root, JsonValue::Object(map))?;
            Ok(Phase::After)
        }
        _ => Err(internal("expected container comma or close")),
    }
}

/// Reads one `"key":` head into the innermost object frame.
fn read_object_key(cursor: &mut Cursor<'_>, stack: &mut [Frame]) -> Result<(), JsonError> {
    let Some(byte) = cursor.skip_ws() else {
        return Err(internal("unterminated object"));
    };
    if byte != b'"' {
        return Err(internal("expected object key after comma"));
    }
    let key = cursor.parse_string()?;
    cursor.expect_colon()?;
    let Some(Frame::Object { key: slot, .. }) = stack.last_mut() else {
        return Err(internal("object entry without object"));
    };
    *slot = Some(key);
    Ok(())
}

/// Files one completed value into the innermost container, or into the root.
fn attach(
    stack: &mut [Frame],
    root: &mut Option<JsonValue>,
    value: JsonValue,
) -> Result<(), JsonError> {
    match stack.last_mut() {
        None => {
            if root.is_some() {
                return Err(internal("duplicate root"));
            }
            *root = Some(value);
        }
        Some(Frame::Array { items }) => items.push(value),
        Some(Frame::Object { map, key }) => {
            let Some(key) = key.take() else {
                return Err(internal("object value without key"));
            };
            // Duplicate keys resolve last-wins, matching ECMAScript JSON.parse.
            map.insert(key, value);
        }
    }
    Ok(())
}

/// Byte cursor over one validated JSON document.
struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl Cursor<'_> {
    /// Advances past JSON whitespace and peeks the next byte.
    fn skip_ws(&mut self) -> Option<u8> {
        let bytes = self.text.as_bytes();
        while let Some(&byte) = bytes.get(self.pos) {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => return Some(byte),
            }
        }
        None
    }

    /// Consumes one byte.
    fn bump(&mut self) {
        self.pos += 1;
    }

    /// Consumes a literal keyword whose first byte the caller peeked.
    fn expect_ident(&mut self, word: &str) -> Result<(), JsonError> {
        if self
            .text
            .as_bytes()
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(word.as_bytes()))
        {
            self.pos += word.len();
            Ok(())
        } else {
            Err(internal("expected literal"))
        }
    }

    /// Consumes the `:` after an object key.
    fn expect_colon(&mut self) -> Result<(), JsonError> {
        match self.skip_ws() {
            Some(b':') => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(internal("expected colon")),
        }
    }

    /// Parses a JSON string starting at its opening quote into UTF-16 code
    /// units, resolving escape sequences and raw UTF-8 scalars alike. Lone
    /// surrogates survive as their own units, whether escaped alone, escaped
    /// before a non-pairing unit, or adjacent to a mismatched unit.
    fn parse_string(&mut self) -> Result<JsString, JsonError> {
        if self.skip_ws() != Some(b'"') {
            return Err(internal("expected string"));
        }
        self.pos += 1;
        let mut units: Vec<u16> = Vec::new();
        loop {
            let Some(&byte) = self.text.as_bytes().get(self.pos) else {
                return Err(internal("unterminated string"));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(JsString::from_utf16(units));
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut units)?;
                }
                0x00..=0x7F => {
                    units.push(u16::from(byte));
                    self.pos += 1;
                }
                _ => {
                    // Raw UTF-8 scalar: the validated text is a Rust `str`,
                    // so the cursor always stands on a character boundary.
                    let Some(scalar) = self.text[self.pos..].chars().next() else {
                        return Err(internal("invalid utf-8 in string"));
                    };
                    let mut buffer = [0u16; 2];
                    for unit in scalar.encode_utf16(&mut buffer) {
                        units.push(*unit);
                    }
                    self.pos += scalar.len_utf8();
                }
            }
        }
    }

    /// Parses one escape sequence after its backslash, pushing the code units
    /// it spells.
    fn parse_escape(&mut self, units: &mut Vec<u16>) -> Result<(), JsonError> {
        let Some(&byte) = self.text.as_bytes().get(self.pos) else {
            return Err(internal("unterminated escape"));
        };
        self.pos += 1;
        let unit = match byte {
            b'"' => 0x22,
            b'\\' => 0x5C,
            b'/' => 0x2F,
            b'b' => 0x08,
            b'f' => 0x0C,
            b'n' => 0x0A,
            b'r' => 0x0D,
            b't' => 0x09,
            b'u' => {
                let first = self.parse_hex4()?;
                units.push(first);
                if (0xD800..=0xDBFF).contains(&first) {
                    // An adjacent \uXXXX may complete the pair; any other
                    // second unit stands alone exactly as written, keeping
                    // both escape readings lossless.
                    let bytes = self.text.as_bytes();
                    if bytes.get(self.pos) == Some(&b'\\') && bytes.get(self.pos + 1) == Some(&b'u')
                    {
                        self.pos += 2;
                        units.push(self.parse_hex4()?);
                    }
                }
                return Ok(());
            }
            _ => return Err(internal("unknown escape")),
        };
        units.push(unit);
        Ok(())
    }

    /// Consumes four hexadecimal digits of a `\u` escape.
    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        let bytes = self.text.as_bytes();
        let mut value: u16 = 0;
        for _ in 0..4 {
            let Some(&byte) = bytes.get(self.pos) else {
                return Err(internal("unterminated escape"));
            };
            let digit = match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a') + 10,
                b'A'..=b'F' => u16::from(byte - b'A') + 10,
                _ => return Err(internal("expected hex digit")),
            };
            value = (value << 4) | digit;
            self.pos += 1;
        }
        Ok(value)
    }

    /// Consumes a JSON number and parses it to binary64. Rust's float parser
    /// is correctly rounded, keeps `-0` negative, and maps JSON overflow
    /// (`1e999`) to ±infinity, which is the exact behavior the canonical
    /// number domain wants.
    fn parse_number(&mut self) -> Result<f64, JsonError> {
        let bytes = self.text.as_bytes();
        let start = self.pos;
        let mut end = start;
        while let Some(&byte) = bytes.get(end) {
            match byte {
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' => end += 1,
                _ => break,
            }
        }
        self.pos = end;
        self.text[start..end]
            .parse::<f64>()
            .map_err(|_| internal("invalid number"))
    }
}

/// Serializes one canonical value as compact JSON (no spaces).
///
/// Strings keep their UTF-16 content exact: valid surrogate pairs serialize as
/// UTF-8 scalars, unpaired units as `\udXXX` escapes, and controls, quotes,
/// and backslashes per RFC 8259. Numbers spell exactly like ECMAScript
/// `JSON.stringify`: both zeros as `0` and nonfinite numbers as `null`, so
/// stringify is not a bit-preserving round trip for every binary64.
/// Iterative, so deep trees serialize without stack growth.
pub fn write_json(value: &JsonValue, out: &mut String) {
    /// Pending sibling iterators, innermost on top.
    enum WriteFrame<'a> {
        Array(std::slice::Iter<'a, JsonValue>),
        Object(btree_map::Iter<'a, JsString, JsonValue>),
    }

    /// Either the next value to write or a resume marker after one value.
    enum Item<'a> {
        Value(&'a JsonValue),
        Continue,
    }

    let mut stack: Vec<WriteFrame<'_>> = Vec::new();
    let mut current = Item::Value(value);
    loop {
        match current {
            Item::Value(value) => match value {
                JsonValue::Null => out.push_str("null"),
                JsonValue::Bool(true) => out.push_str("true"),
                JsonValue::Bool(false) => out.push_str("false"),
                JsonValue::Number(number) => write_number(*number, out),
                JsonValue::String(string) => write_string(string, out),
                JsonValue::Array(items) => {
                    out.push('[');
                    let mut items = items.iter();
                    match items.next() {
                        Some(first) => {
                            stack.push(WriteFrame::Array(items));
                            current = Item::Value(first);
                            continue;
                        }
                        None => out.push(']'),
                    }
                }
                JsonValue::Object(map) => {
                    out.push('{');
                    let mut entries = map.iter();
                    match entries.next() {
                        Some((key, first)) => {
                            write_string(key, out);
                            out.push(':');
                            stack.push(WriteFrame::Object(entries));
                            current = Item::Value(first);
                            continue;
                        }
                        None => out.push('}'),
                    }
                }
            },
            Item::Continue => match stack.last_mut() {
                None => return,
                Some(WriteFrame::Array(items)) => {
                    if let Some(next) = items.next() {
                        out.push(',');
                        current = Item::Value(next);
                        continue;
                    }
                    out.push(']');
                    stack.pop();
                }
                Some(WriteFrame::Object(entries)) => {
                    if let Some((key, next)) = entries.next() {
                        out.push(',');
                        write_string(key, out);
                        out.push(':');
                        current = Item::Value(next);
                        continue;
                    }
                    out.push('}');
                    stack.pop();
                }
            },
        }
        current = Item::Continue;
    }
}

/// Serializes one canonical value as compact JSON.
#[must_use]
pub fn stringify_json(value: &JsonValue) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

/// Writes one JSON number exactly like ECMAScript `JSON.stringify`: nonfinite
/// numbers as `null` and both zeros as `0`. Parsing still preserves negative
/// zero internally; stringify simply does not promise bit-preserving round
/// trips for every binary64, matching JavaScript.
fn write_number(value: f64, out: &mut String) {
    if value.is_finite() {
        out.push_str(&js_number_to_string(value));
    } else {
        out.push_str("null");
    }
}

/// Writes one JSON string with RFC 8259 escapes over UTF-16 code units.
fn write_string(value: &JsString, out: &mut String) {
    out.push('"');
    let units = value.as_utf16();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if matches!(unit, 0xD800..=0xDBFF) && matches!(units.get(index + 1), Some(0xDC00..=0xDFFF))
        {
            // A stored pair serializes as its scalar; re-parsing re-splits it.
            let scalar = 0x1_0000
                + ((u32::from(unit) - 0xD800) << 10)
                + (u32::from(units[index + 1]) - 0xDC00);
            if let Some(scalar) = char::from_u32(scalar) {
                out.push(scalar);
            } else {
                push_escaped_unit(unit, out);
                push_escaped_unit(units[index + 1], out);
            }
            index += 2;
            continue;
        }
        match unit {
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0C => out.push_str("\\f"),
            0x0D => out.push_str("\\r"),
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x00..=0x1F | 0xD800..=0xDFFF => push_escaped_unit(unit, out),
            _ => match char::from_u32(u32::from(unit)) {
                Some(scalar) => out.push(scalar),
                // Unreachable: every non-surrogate unit is a scalar.
                None => push_escaped_unit(unit, out),
            },
        }
        index += 1;
    }
    out.push('"');
}

/// Appends one code unit as a lowercase `\udXXX` escape.
fn push_escaped_unit(unit: u16, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('\\');
    out.push('u');
    out.push(char::from(HEX[(unit >> 12) as usize & 0x0F]));
    out.push(char::from(HEX[(unit >> 8) as usize & 0x0F]));
    out.push(char::from(HEX[(unit >> 4) as usize & 0x0F]));
    out.push(char::from(HEX[unit as usize & 0x0F]));
}

/// Formats one binary64 exactly as ECMAScript `Number::toString` does:
/// `"NaN"`, `"Infinity"`, `"-Infinity"`, `"0"` for both zeros, integral
/// values without a fractional part, shortest round-trip digits otherwise,
/// and exponential form with an explicit exponent sign beyond `1e21` or below
/// `1e-6`. Digits come from `serde_json`'s own shortest float formatter (the
/// installed formatter is reused through `Number::to_string`), re-placed per
/// the ES rules; no extra dependency is added.
#[must_use]
pub fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }
    if value == 0.0 {
        // ECMAScript prints both zeros as "0"; parsing still keeps `-0` text
        // as a negative zero inside the value.
        return "0".to_owned();
    }

    let negative = value.is_sign_negative();
    let (digits, exponent) = shortest_digits(value.abs());
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    place_digits(&digits, exponent, &mut out);
    out
}

/// Extracts the shortest round-trip decimal digits `d1..dk` and the ECMAScript
/// exponent `n` with `0.d1..dk * 10^n` equal to the nonzero finite `value`.
fn shortest_digits(value: f64) -> (String, i32) {
    // serde_json spells floats with its installed shortest-round-trip
    // formatter, so the module reuses that exact digit choice instead of
    // adding a dependency; `normalize_decimal` re-normalizes its decimal or
    // scientific decoration into bare digits plus the ECMAScript exponent.
    let Some(number) = serde_json::Number::from_f64(value) else {
        // Unreachable: callers pass nonzero finite magnitudes only.
        return (String::new(), 0);
    };
    normalize_decimal(&number.to_string())
}

/// Splits one `serde_json` decimal spelling (`18446744073709552000.0`,
/// `0.0001`, `1.5e-10`) into normalized digits and exponent `n` with the
/// value equal to `0.digits * 10^n`.
fn normalize_decimal(text: &str) -> (String, i32) {
    let (mantissa, exponent) = match text.split_once('e').or_else(|| text.split_once('E')) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().unwrap_or(0)),
        None => (text, 0),
    };
    let mut digits = String::with_capacity(mantissa.len());
    let mut before_dot = 0i32;
    let mut seen_dot = false;
    for byte in mantissa.bytes() {
        match byte {
            b'0'..=b'9' => {
                if !seen_dot {
                    before_dot += 1;
                }
                digits.push(char::from(byte));
            }
            b'.' => seen_dot = true,
            // A sign cannot appear: only nonzero finite magnitudes arrive.
            _ => {}
        }
    }
    // Leading zeros shift the exponent down; trailing zeros are decimal
    // notation decoration and drop without touching the exponent.
    let leading = digits.bytes().take_while(|&byte| byte == b'0').count();
    let end = digits.len()
        - digits
            .bytes()
            .rev()
            .take_while(|&byte| byte == b'0')
            .count();
    if leading >= end {
        // Unreachable for nonzero magnitudes; spell a plain zero.
        return (String::from("0"), 1);
    }
    let exponent = before_dot + exponent - i32::try_from(leading).unwrap_or(i32::MAX);
    (digits[leading..end].to_owned(), exponent)
}

/// Places digits per the ECMAScript `Number::toString` selection rules.
fn place_digits(digits: &str, exponent: i32, out: &mut String) {
    let count = i32::try_from(digits.len()).unwrap_or(i32::MAX);
    if count <= exponent && exponent <= 21 {
        // Integer with trailing zeros.
        out.push_str(digits);
        for _ in 0..(exponent - count) {
            out.push('0');
        }
    } else if exponent > 0 && exponent <= 21 {
        // Decimal point inside the digit string.
        let point = usize::try_from(exponent).unwrap_or(0);
        out.push_str(&digits[..point]);
        out.push('.');
        out.push_str(&digits[point..]);
    } else if exponent > -6 && exponent <= 0 {
        // Leading "0." with zeros between.
        out.push_str("0.");
        for _ in 0..(-exponent) {
            out.push('0');
        }
        out.push_str(digits);
    } else {
        // Exponential: d[.ddd]e±(n-1) with an explicit exponent sign.
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        let mantissa_exponent = exponent - 1;
        if mantissa_exponent < 0 {
            out.push('-');
        } else {
            out.push('+');
        }
        out.push_str(&mantissa_exponent.unsigned_abs().to_string());
    }
}

/// Builds the module-local "cannot happen" error.
fn internal(message: &'static str) -> JsonError {
    JsonError::Internal {
        message: message.to_owned(),
    }
}
