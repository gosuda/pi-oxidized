//! Canonical Chord value domain.
//!
//! Chord keeps JavaScript's value boundaries in-process: strings own UTF-16
//! code units (including unpaired surrogates), numbers are binary64, and
//! objects use a `BTreeMap` because service semantics do not expose
//! JavaScript property enumeration order at this layer. The JSON and
//! `serde_json` adapters are explicit submodules; the rest of pi-ai/session
//! continues to use its own `serde_json::Value` domain.
//!
//! `JsonValue` has an iterative `Drop` implementation. This is necessary
//! because JSON parsing deliberately accepts documents deeper than the
//! admission predicate's 512-level protocol limit. Consumers must therefore
//! not destructure an owned `JsonValue` directly (that is forbidden for a
//! type with `Drop`); match borrowed values or use `JsonValue::take`/
//! `mem::take` on fields instead.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use thiserror::Error;

mod bridge;
mod json;

pub use bridge::{from_serde_json, try_into_serde_json};
pub use json::{js_number_to_string, parse_json, stringify_json, write_json, JsonError};

/// Object storage for canonical values.
///
/// The map's sorted code-unit order is an implementation choice accepted by
/// the Chord boundary contract; property enumeration order is not observable
/// through this value module.
pub type JsObject = BTreeMap<JsString, JsonValue>;

/// A string represented by UTF-16 code units.
///
/// `from_utf16` accepts every code-unit sequence, including unpaired
/// surrogates. Use [`JsString::try_to_utf8`] only at a boundary that requires
/// valid Rust UTF-8; semantic Chord paths keep the units unchanged.
#[derive(Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct JsString {
    units: Vec<u16>,
}

impl JsString {
    /// Creates a string from owned UTF-16 code units without validation.
    #[must_use]
    pub fn from_utf16(units: Vec<u16>) -> Self {
        Self { units }
    }

    /// Encodes a valid Rust UTF-8 string into UTF-16 code units.
    #[must_use]
    pub fn from_utf8(text: &str) -> Self {
        Self { units: text.encode_utf16().collect() }
    }

    /// Borrows the exact UTF-16 code units.
    #[must_use]
    pub fn as_utf16(&self) -> &[u16] {
        &self.units
    }

    /// Converts to Rust UTF-8 without replacement characters.
    ///
    /// # Errors
    /// Returns [`UnpairedSurrogate`] at the first code-unit position that is
    /// not a scalar or a valid high/low surrogate pair.
    pub fn try_to_utf8(&self) -> Result<String, UnpairedSurrogate> {
        let mut text = String::new();
        let mut index = 0;
        while index < self.units.len() {
            let unit = self.units[index];
            index += 1;
            let scalar = if (0xD800..=0xDBFF).contains(&unit)
                && self.units.get(index).is_some_and(|next| (0xDC00..=0xDFFF).contains(next))
            {
                let low = self.units[index];
                index += 1;
                0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
            } else {
                u32::from(unit)
            };
            let Some(scalar) = char::from_u32(scalar) else {
                return Err(UnpairedSurrogate { index: index - 1 });
            };
            text.push(scalar);
        }
        Ok(text)
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        Self::from_utf8(text)
    }
}

impl From<String> for JsString {
    fn from(text: String) -> Self {
        Self::from_utf8(&text)
    }
}

impl Borrow<[u16]> for JsString {
    fn borrow(&self) -> &[u16] {
        self.as_utf16()
    }
}

/// Compares directly against UTF-16 encoding of a Rust string, never through
/// replacement-character conversion.
impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        self.units.iter().copied().eq(other.encode_utf16())
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        self.units.iter().copied().eq(other.encode_utf16())
    }
}

impl PartialEq<JsString> for str {
    fn eq(&self, other: &JsString) -> bool {
        self.encode_utf16().eq(other.units.iter().copied())
    }
}

impl PartialEq<JsString> for &str {
    fn eq(&self, other: &JsString) -> bool {
        self.encode_utf16().eq(other.units.iter().copied())
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JsString(")?;
        write!(formatter, "{:?}", String::from_utf16_lossy(&self.units))?;
        formatter.write_char(')')
    }
}

/// The first unpaired UTF-16 code unit encountered during strict decoding.
#[derive(Clone, Copy, Debug, Eq, Error, Hash, Ord, PartialEq, PartialOrd)]
#[error("unpaired surrogate at code unit {index}")]
pub struct UnpairedSurrogate {
    /// Zero-based UTF-16 code-unit position.
    pub index: usize,
}

/// A checked nonnegative finite integral binary64.
///
/// The original `f64` bits are stored, so negative zero remains observable
/// through [`JsInteger::as_f64`]. Equality, ordering, and hashing normalize
/// the two zero encodings. Values are not restricted to `u64` or the
/// JavaScript safe-integer range.
#[derive(Clone, Copy, Debug)]
pub struct JsInteger {
    value: f64,
}

impl JsInteger {
    /// Validates and stores one nonnegative finite integral binary64.
    ///
    /// Negative zero is accepted because it compares nonnegative and is an
    /// integer; its sign bit is retained in storage.
    ///
    /// # Errors
    /// Returns [`ValueError::NotInteger`] for NaN, either infinity, a negative
    /// nonzero value, or a nonintegral value.
    pub fn new(value: f64) -> Result<Self, ValueError> {
        if value.is_finite() && value >= 0.0 && value.fract() == 0.0 {
            Ok(Self { value })
        } else {
            Err(ValueError::NotInteger(value))
        }
    }

    /// Returns the stored binary64, including a negative-zero sign bit.
    #[must_use]
    pub const fn as_f64(&self) -> f64 {
        self.value
    }

    /// Returns positive zero.
    #[must_use]
    pub const fn zero() -> Self {
        Self { value: 0.0 }
    }

    /// Returns one.
    #[must_use]
    pub const fn one() -> Self {
        Self { value: 1.0 }
    }

    /// Adds one using binary64 arithmetic.
    ///
    /// At and above the binary64 precision boundary this may equal the input,
    /// matching JavaScript `Number` arithmetic.
    #[must_use]
    pub fn next(&self) -> Self {
        Self { value: self.value + 1.0 }
    }

    /// Clamps this counter to `len` before the bounded integer conversion.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "JsInteger::new rejects nonintegral values"
    )]
    #[expect(clippy::cast_sign_loss, reason = "JsInteger::new rejects negative values")]
    pub fn clamp_to_len(&self, len: usize) -> usize {
        // `JsInteger::new` guarantees a finite, nonnegative, integral binary64.
        // Values above `u128::MAX` saturate; values that cannot fit `usize` clamp to `len`.
        let value = self.value as u128;
        usize::try_from(value).map_or(len, |value| value.min(len))
    }

    /// Folds negative zero to positive zero for identity operations.
    fn normalized(self) -> f64 {
        if self.value == 0.0 { 0.0 } else { self.value }
    }
}

impl PartialEq for JsInteger {
    fn eq(&self, other: &Self) -> bool {
        self.normalized().to_bits() == other.normalized().to_bits()
    }
}

impl Eq for JsInteger {}

impl PartialOrd for JsInteger {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsInteger {
    fn cmp(&self, other: &Self) -> Ordering {
        self.normalized().partial_cmp(&other.normalized()).unwrap_or(Ordering::Equal)
    }
}

impl Hash for JsInteger {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.normalized().to_bits().hash(state);
    }
}

/// The canonical Chord value tree.
///
/// `Number` intentionally stores every binary64, including nonfinite values
/// produced by JSON numeric overflow. [`is_json_value`] and
/// [`try_into_serde_json`] apply their separate finite-value policies at their
/// respective boundaries.
pub enum JsonValue {
    /// JSON null.
    Null,
    /// A boolean.
    Bool(bool),
    /// A binary64 number, possibly nonfinite in the in-process domain.
    Number(f64),
    /// A UTF-16 code-unit string.
    String(JsString),
    /// An ordered sequence of values.
    Array(Vec<JsonValue>),
    /// A code-unit-keyed object.
    Object(JsObject),
}
impl Clone for JsonValue {
    fn clone(&self) -> Self {
        enum Frame<'a> {
            Array {
                built: Vec<JsonValue>,
                rest: std::slice::Iter<'a, JsonValue>,
            },
            Object {
                built: JsObject,
                key: Option<JsString>,
                rest: std::collections::btree_map::Iter<'a, JsString, JsonValue>,
            },
        }

        let mut frames: Vec<Frame<'_>> = Vec::new();
        let mut pending = Some(self);
        let mut completed: Option<JsonValue> = None;
        loop {
            if let Some(value) = completed.take() {
                match frames.last_mut() {
                    None => return value,
                    Some(Frame::Array { built, .. }) => built.push(value),
                    Some(Frame::Object { built, key, .. }) => {
                        if let Some(key) = key.take() {
                            built.insert(key, value);
                        }
                    }
                }
                continue;
            }

            if let Some(value) = pending.take() {
                match value {
                    Self::Null => completed = Some(Self::Null),
                    Self::Bool(value) => completed = Some(Self::Bool(*value)),
                    Self::Number(value) => completed = Some(Self::Number(*value)),
                    Self::String(value) => completed = Some(Self::String(value.clone())),
                    Self::Array(values) => {
                        let mut rest = values.iter();
                        let first = rest.next();
                        frames.push(Frame::Array { built: Vec::with_capacity(values.len()), rest });
                        pending = first;
                    }
                    Self::Object(values) => {
                        let mut rest = values.iter();
                        let first = rest.next();
                        frames.push(Frame::Object { built: JsObject::new(), key: None, rest });
                        if let Some((key, value)) = first {
                            if let Some(Frame::Object { key: slot, .. }) = frames.last_mut() {
                                *slot = Some(key.clone());
                            }
                            pending = Some(value);
                        }
                    }
                }
                continue;
            }

            let mut close = false;
            if let Some(frame) = frames.last_mut() {
                match frame {
                    Frame::Array { rest, .. } => {
                        if let Some(value) = rest.next() {
                            pending = Some(value);
                        } else {
                            close = true;
                        }
                    }
                    Frame::Object { key, rest, .. } => {
                        if let Some((next_key, value)) = rest.next() {
                            *key = Some(next_key.clone());
                            pending = Some(value);
                        } else {
                            close = true;
                        }
                    }
                }
            }

            if close && let Some(frame) = frames.pop() {
                completed = Some(match frame {
                    Frame::Array { built, .. } => Self::Array(built),
                    Frame::Object { built, .. } => Self::Object(built),
                });
            }
        }
    }
}

impl PartialEq for JsonValue {
    fn eq(&self, other: &Self) -> bool {
        let mut work: Vec<(&JsonValue, &JsonValue)> = vec![(self, other)];
        while let Some((left, right)) = work.pop() {
            match (left, right) {
                (Self::Null, Self::Null) => {}
                (Self::Bool(left), Self::Bool(right)) if left == right => {}
                (Self::Number(left), Self::Number(right)) if left == right => {}
                (Self::String(left), Self::String(right)) if left == right => {}
                (Self::Array(left), Self::Array(right)) => {
                    if left.len() != right.len() {
                        return false;
                    }
                    work.extend(left.iter().zip(right.iter()));
                }
                (Self::Object(left), Self::Object(right)) => {
                    if left.len() != right.len() {
                        return false;
                    }
                    for ((left_key, left_value), (right_key, right_value)) in left.iter().zip(right.iter()) {
                        if left_key != right_key {
                            return false;
                        }
                        work.push((left_value, right_value));
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

impl JsonValue {
    /// Returns true only for [`JsonValue::Null`].
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Borrows a boolean value, if this is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Borrows a binary64 number, including a nonfinite payload.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// Borrows a canonical string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&JsString> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// Borrows an array, if this is one.
    #[must_use]
    pub fn as_array(&self) -> Option<&Vec<JsonValue>> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    /// Borrows an object, if this is one.
    #[must_use]
    pub fn as_object(&self) -> Option<&JsObject> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    /// Takes an owned string without destructuring a `Drop` type.
    #[must_use]
    pub fn into_string(mut self) -> Option<JsString> {
        match &mut self {
            Self::String(value) => Some(std::mem::take(value)),
            _ => None,
        }
    }

    /// Replaces this value with [`JsonValue::Null`] and returns the old value.
    #[must_use]
    pub fn take(&mut self) -> JsonValue {
        std::mem::replace(self, Self::Null)
    }
}

impl Drop for JsonValue {
    fn drop(&mut self) {
        // Moving every child into an explicit heap stack prevents recursive
        // Vec/BTreeMap drop glue from following a deeply nested path.
        let mut pending: Vec<JsonValue> = Vec::new();
        match self {
            Self::Array(items) => pending.append(items),
            Self::Object(map) => {
                pending.extend(map.values_mut().map(|value| std::mem::replace(value, Self::Null)));
            }
            _ => {}
        }
        while let Some(mut value) = pending.pop() {
            match &mut value {
                Self::Array(items) => pending.append(items),
                Self::Object(map) => {
                    pending.extend(map.values_mut().map(|slot| std::mem::replace(slot, Self::Null)));
                }
                _ => {}
            }
        }
    }
}

impl fmt::Debug for JsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MAX_DEPTH: usize = 8;
        const MAX_NODES: usize = 4_096;

        enum Item<'a> {
            Value(&'a JsonValue, usize),
            Key(&'a JsString),
            Text(&'static str),
        }

        let mut work = vec![Item::Value(self, 0)];
        let mut nodes = 0;
        while let Some(item) = work.pop() {
            match item {
                Item::Text(text) => formatter.write_str(text)?,
                Item::Key(key) => fmt::Debug::fmt(key, formatter)?,
                Item::Value(value, depth) => {
                    if nodes >= MAX_NODES {
                        formatter.write_str("…")?;
                        work.clear();
                        continue;
                    }
                    nodes += 1;
                    match value {
                        Self::Null => formatter.write_str("Null")?,
                        Self::Bool(value) => write!(formatter, "Bool({value})")?,
                        Self::Number(value) => {
                            formatter.write_str("Number(")?;
                            fmt::Debug::fmt(value, formatter)?;
                            formatter.write_char(')')?;
                        }
                        Self::String(value) => {
                            formatter.write_str("String(")?;
                            fmt::Debug::fmt(value, formatter)?;
                            formatter.write_char(')')?;
                        }
                        Self::Array(values) => {
                            if depth >= MAX_DEPTH {
                                formatter.write_str("Array(…)")?;
                            } else if values.is_empty() {
                                formatter.write_str("Array([])")?;
                            } else {
                                formatter.write_str("Array([")?;
                                work.push(Item::Text("]))"));
                                for (index, value) in values.iter().enumerate().rev() {
                                    if index + 1 < values.len() {
                                        work.push(Item::Text(", "));
                                    }
                                    work.push(Item::Value(value, depth + 1));
                                }
                            }
                        }
                        Self::Object(values) => {
                            if depth >= MAX_DEPTH {
                                formatter.write_str("Object(…)")?;
                            } else if values.is_empty() {
                                formatter.write_str("Object({})")?;
                            } else {
                                formatter.write_str("Object({")?;
                                work.push(Item::Text("})"));
                                for (index, (key, value)) in values.iter().enumerate().rev() {
                                    if index + 1 < values.len() {
                                        work.push(Item::Text(", "));
                                    }
                                    work.push(Item::Value(value, depth + 1));
                                    work.push(Item::Text(": "));
                                    work.push(Item::Key(key));
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// A value-domain conversion failure.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ValueError {
    /// The input is not a nonnegative finite integral binary64.
    #[error("not a nonnegative integral number: {0}")]
    NotInteger(f64),
    /// A nonfinite number cannot be represented by `serde_json`.
    #[error("number is not finite: {0}")]
    NonFinite(f64),
    /// A string or object key contains an unpaired UTF-16 code unit.
    #[error("unpaired surrogate at code unit {index}")]
    LoneSurrogate {
        /// Code-unit index reported by strict UTF-16 decoding.
        index: usize,
    },
}

/// Tests whether a canonical tree is admissible as a finite JSON value.
///
/// This predicate is separate from [`parse_json`], which accepts overflowing
/// numeric text and imposes no extra nesting cap, and from delta/service shape
/// validation, which has its own contracts. A node at root-based depth 512 is
/// valid; it cannot contain a child because that child would be depth 513.
#[must_use]
pub fn is_json_value(value: &JsonValue) -> bool {
    const MAX_DEPTH: usize = 512;
    let mut work: Vec<(&JsonValue, usize)> = vec![(value, 0)];
    while let Some((value, depth)) = work.pop() {
        match value {
            JsonValue::Number(number) if !number.is_finite() => return false,
            JsonValue::Array(values) => {
                for value in values {
                    if depth >= MAX_DEPTH {
                        return false;
                    }
                    work.push((value, depth + 1));
                }
            }
            JsonValue::Object(values) => {
                for value in values.values() {
                    if depth >= MAX_DEPTH {
                        return false;
                    }
                    work.push((value, depth + 1));
                }
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test fixtures and assertions use contextual failure messages")]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;

    use super::*;

    fn parse(text: &str) -> JsonValue {
        parse_json(text).expect("test JSON parses")
    }

    fn integer(value: f64) -> JsInteger {
        JsInteger::new(value).expect("test integer")
    }

    #[test]
    fn scalar_and_container_json_round_trip() {
        let value = parse(r#"{"null":null,"bool":true,"number":1.25,"text":"é😀","items":[false,2]}"#);
        assert_eq!(stringify_json(&value), r#"{"bool":true,"items":[false,2],"null":null,"number":1.25,"text":"é😀"}"#);
        assert_eq!(parse(&stringify_json(&value)), value);
    }

    #[test]
    fn duplicate_decoded_keys_are_last_wins() {
        let value = parse(r#"{"a":1,"\u0061":2}"#);
        let object = value.as_object().expect("object");
        assert_eq!(object.len(), 1);
        assert_eq!(object.get(&JsString::from("a")).and_then(JsonValue::as_f64), Some(2.0));
    }

    #[test]
    fn surrogate_units_are_lossless_and_strictly_decoded() {
        let lone = parse(r#""\uDE03""#);
        assert_eq!(lone.as_str().expect("string").as_utf16(), &[0xDE03]);
        assert_eq!(lone.as_str().expect("string").try_to_utf8(), Err(UnpairedSurrogate { index: 0 }));
        assert_eq!(stringify_json(&lone), r#""\ude03""#);
        let reparsed = parse(&stringify_json(&lone));
        assert_eq!(reparsed.as_str().expect("string").as_utf16(), &[0xDE03]);

        let pair = parse(r#""\uD83D\uDE00""#);
        assert_eq!(pair.as_str().expect("string").try_to_utf8().as_deref(), Ok("😀"));
        assert_eq!(stringify_json(&pair), "\"😀\"");
    }

    #[test]
    fn negative_zero_overflow_and_json_admission_stay_separate() {
        let negative_zero = parse("-0");
        assert!(matches!(negative_zero, JsonValue::Number(number) if number == 0.0 && number.is_sign_negative()));
        assert_eq!(stringify_json(&negative_zero), "0");
        let reparsed = parse(&stringify_json(&negative_zero));
        assert!(matches!(reparsed, JsonValue::Number(number) if !number.is_sign_negative()));

        let overflow = parse("1e9999");
        assert!(matches!(overflow, JsonValue::Number(number) if number.is_infinite() && number.is_sign_positive()));
        assert!(!is_json_value(&overflow));
        assert_eq!(stringify_json(&overflow), "null");
    }

    #[test]
    fn javascript_number_spelling_uses_shortest_digits_and_es_placement() {
        let cases = [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-1.5, "-1.5"),
            (0.5, "0.5"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (18_446_744_073_709_551_616.0, "18446744073709552000"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for (number, expected) in cases {
            assert_eq!(js_number_to_string(number), expected);
        }
    }

    #[test]
    fn js_integer_keeps_zero_sign_but_normalizes_identity() {
        assert!(JsInteger::new(-1.0).is_err());
        assert!(JsInteger::new(0.5).is_err());
        assert!(JsInteger::new(f64::NAN).is_err());
        assert!(JsInteger::new(f64::INFINITY).is_err());

        let negative_zero = JsInteger::new(-0.0).expect("negative zero is a nonnegative integer");
        assert!(negative_zero.as_f64().is_sign_negative());
        assert_eq!(negative_zero, JsInteger::zero());
        let mut left = DefaultHasher::new();
        negative_zero.hash(&mut left);
        let mut right = DefaultHasher::new();
        JsInteger::zero().hash(&mut right);
        assert_eq!(left.finish(), right.finish());

        assert_eq!(JsInteger::zero().next(), JsInteger::one());
        let precision_edge = integer(9_007_199_254_740_992.0);
        assert_eq!(precision_edge.next(), precision_edge);
        assert_eq!(integer(3.0).clamp_to_len(10), 3);
        assert_eq!(integer(1e100).clamp_to_len(10), 10);
    }

    #[test]
    fn deep_json_is_iterative_and_admission_has_a_512_level_boundary() {
        let depth = 10_000;
        let text = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let value = parse(&text);
        assert!(!is_json_value(&value));
        assert_eq!(stringify_json(&value), text);

        let cloned = value.clone();
        assert_eq!(cloned, value);
        drop(cloned);
        drop(value);

        let mut at_limit = JsonValue::Null;
        for _ in 0..512 {
            at_limit = JsonValue::Array(vec![at_limit]);
        }
        assert!(is_json_value(&at_limit));
        at_limit = JsonValue::Array(vec![at_limit]);
        assert!(!is_json_value(&at_limit));
    }

    #[test]
    fn serde_bridge_is_explicit_and_nonlossy_in_reverse() {
        let wide = serde_json::json!({"n": 9_007_199_254_740_993_u64});
        let canonical = from_serde_json(wide);
        let number = canonical
            .as_object()
            .and_then(|object| object.get(&JsString::from("n")))
            .and_then(JsonValue::as_f64);
        assert_eq!(number, Some(9_007_199_254_740_992.0));

        let lone = JsonValue::String(JsString::from_utf16(vec![0xD800]));
        assert_eq!(try_into_serde_json(lone), Err(ValueError::LoneSurrogate { index: 0 }));
        assert_eq!(
            try_into_serde_json(JsonValue::Number(f64::INFINITY)),
            Err(ValueError::NonFinite(f64::INFINITY))
        );
        let valid = parse(r#"{"a":[true,null,"ok"]}"#);
        assert!(try_into_serde_json(valid).is_ok());
    }

    #[test]
    fn string_comparison_and_borrow_use_code_units() {
        let music = JsString::from("𝄞");
        assert_eq!(music.as_utf16(), &[0xD834, 0xDD1E]);
        assert_eq!(music, "𝄞");
        assert_eq!(music.try_to_utf8().as_deref(), Ok("𝄞"));

        let mut object = JsObject::new();
        object.insert(JsString::from("a"), JsonValue::Null);
        assert!(object.contains_key(&[0x61_u16][..]));
    }

    #[test]
    fn nan_equality_matches_binary64() {
        assert_ne!(JsonValue::Number(f64::NAN), JsonValue::Number(f64::NAN));
        assert_eq!(JsonValue::Number(0.0), JsonValue::Number(-0.0));
    }
}
