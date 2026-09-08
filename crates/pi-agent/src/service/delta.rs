//! Typed native implementation of Chord's JSON delta primitive.
//!
//! The tuple vocabulary intentionally stays identical to the TypeScript
//! implementation.  Values are the canonical [`JsonValue`] tree — UTF-16
//! strings, binary64 numbers, ordered-map objects — shared with the
//! surrounding native remote envelope.  The tracker is explicit: callers
//! mutate [`DeltaTracker::state_mut`] (or use its path helpers), and the
//! tracker derives one coalesced batch from the last published baseline.

use std::collections::{HashMap, HashSet};
use std::fmt;

use thiserror::Error;

use super::value::{JsInteger, JsObject, JsString, JsonValue, js_number_to_string};

const DEFAULT_MAX_OVERLAP_SCAN: usize = 65_536;
const OVERLAP_PROBE: usize = 64;
const OVERLAP_MAX_CANDIDATES: usize = 8;

/// A key or an array index in a state path.
///
/// Numeric segments remain numeric throughout the codec.  In particular,
/// `Index(0)` is not interchangeable with `Key("0")`: arrays reject the latter
/// at the applier boundary, just as JavaScript arrays do.  Indices use the
/// complete binary64 integer domain — a path segment parsed from `1e30` is
/// admitted and fails only when it is applied to a concrete collection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum PathSegment {
    /// An object property name.
    Key(JsString),
    /// A non-negative array index (or a numeric object property when the
    /// addressed container is an object).
    Index(JsInteger),
}

impl From<JsString> for PathSegment {
    fn from(key: JsString) -> Self {
        Self::Key(key)
    }
}

impl From<String> for PathSegment {
    fn from(key: String) -> Self {
        Self::Key(JsString::from(key))
    }
}

impl From<&str> for PathSegment {
    fn from(key: &str) -> Self {
        Self::Key(JsString::from(key))
    }
}

impl From<JsInteger> for PathSegment {
    fn from(index: JsInteger) -> Self {
        Self::Index(index)
    }
}

impl From<u64> for PathSegment {
    fn from(index: u64) -> Self {
        Self::Index(integer(index))
    }
}

impl PathSegment {
    /// Creates an object-key segment.
    #[must_use]
    pub fn key(key: impl Into<JsString>) -> Self {
        Self::Key(key.into())
    }

    /// Creates an array-index segment.
    ///
    /// The index is a canonical binary64 integer; native callers holding a
    /// `u64` can use `PathSegment::from(index)` for the same coercion.
    #[must_use]
    pub const fn index(index: JsInteger) -> Self {
        Self::Index(index)
    }

    /// Returns the key when this is an object segment.
    #[must_use]
    pub fn as_key(&self) -> Option<&JsString> {
        match self {
            Self::Key(key) => Some(key),
            Self::Index(_) => None,
        }
    }

    /// Returns the numeric index when this is an index segment.
    #[must_use]
    pub const fn as_index(&self) -> Option<JsInteger> {
        match self {
            Self::Key(_) => None,
            Self::Index(index) => Some(*index),
        }
    }
}

impl fmt::Display for PathSegment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Diagnostic only: a key holding unpaired surrogates renders with
            // replacement characters because a Rust `String` cannot hold them.
            Self::Key(key) => formatter.write_str(&String::from_utf16_lossy(key.as_utf16())),
            Self::Index(index) => formatter.write_str(&js_number_to_string(index.as_f64())),
        }
    }
}

/// A complete state path.  The empty path addresses the root and is legal
/// only for a splice operation.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct StatePath(Vec<PathSegment>);

impl StatePath {
    /// Returns the root path.
    #[must_use]
    pub const fn root() -> Self {
        Self(Vec::new())
    }

    /// Builds and validates a path from any segment-like iterator.
    ///
    /// # Errors
    /// Returns `DeltaError::UnsafePath` when a key segment matches a reserved
    /// prototype name.
    pub fn new<I, S>(segments: I) -> Result<Self, DeltaError>
    where
        I: IntoIterator<Item = S>,
        S: Into<PathSegment>,
    {
        let path = Self(segments.into_iter().map(Into::into).collect());
        path.validate()?;
        Ok(path)
    }

    /// Builds a path and validates it as a non-empty path.
    ///
    /// # Errors
    /// Returns `DeltaError::UnsafePath` when a key segment matches a reserved
    /// prototype name, or `DeltaError::InvalidTuple` when the path is empty.
    pub fn non_empty<I, S>(segments: I) -> Result<Self, DeltaError>
    where
        I: IntoIterator<Item = S>,
        S: Into<PathSegment>,
    {
        let path = Self::new(segments)?;
        if path.is_empty() {
            return Err(DeltaError::InvalidTuple {
                kind: "path",
                reason: "path is empty".to_owned(),
            });
        }
        Ok(path)
    }

    /// Returns the path segments.
    #[must_use]
    pub fn as_slice(&self) -> &[PathSegment] {
        &self.0
    }

    /// Consumes the path into its segments.
    #[must_use]
    pub fn into_vec(self) -> Vec<PathSegment> {
        self.0
    }

    /// Returns whether this is the root path.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns a path with one segment appended.
    #[must_use]
    pub fn joined(&self, segment: PathSegment) -> Self {
        let mut segments = self.0.clone();
        segments.push(segment);
        Self(segments)
    }

    /// Validates that no key segment matches a reserved prototype name.
    ///
    /// # Errors
    /// Returns `DeltaError::UnsafePath` when a key segment is reserved.
    pub fn validate(&self) -> Result<(), DeltaError> {
        validate_path(self)
    }

    fn from_segments_unchecked(segments: Vec<PathSegment>) -> Self {
        Self(segments)
    }
}

/// Converts a segment vector into a path.  Call [`StatePath::validate`] before
/// using paths built through this infallible conversion.
impl<S> From<Vec<S>> for StatePath
where
    S: Into<PathSegment>,
{
    fn from(segments: Vec<S>) -> Self {
        Self(segments.into_iter().map(Into::into).collect())
    }
}

impl<const N: usize> From<[PathSegment; N]> for StatePath {
    fn from(segments: [PathSegment; N]) -> Self {
        Self(segments.into_iter().collect())
    }
}

impl AsRef<[PathSegment]> for StatePath {
    fn as_ref(&self) -> &[PathSegment] {
        self.as_slice()
    }
}

/// Errors raised while validating, encoding, decoding, tracking, or applying
/// a delta stream.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DeltaError {
    /// Tuple syntax or a tuple payload has the wrong shape.
    #[error("invalid {kind} tuple: {reason}")]
    InvalidTuple {
        /// Tuple vocabulary (`op` or `wire`).
        kind: &'static str,
        /// Shape detail.
        reason: String,
    },
    /// A path attempts to use a prototype-chain name or another unsafe segment.
    #[error("unsafe path segment: {segment}")]
    UnsafePath {
        /// Rejected path segment.
        segment: PathSegment,
    },
    /// A path cannot be resolved against the current value.
    #[error("unresolvable path: {path:?}")]
    Path {
        /// Complete failing path.
        path: StatePath,
    },
    /// A numeric wire path id was not defined on this stream.
    #[error("unresolvable path id: {0:?}")]
    PathId(JsInteger),
    /// The tracked root cannot be deleted.
    #[error("the tracked root cannot be deleted")]
    RootDelete,
    /// A mutation would create a sparse array.
    #[error("array index is outside the existing range: {index:?}")]
    SparseArray {
        /// Rejected index.
        index: JsInteger,
    },
    /// The operation requires a string at its target path.
    #[error("path does not contain a string: {path:?}")]
    NotString {
        /// Complete failing path.
        path: StatePath,
    },
    /// The operation requires an array at its target path.
    #[error("path does not contain an array: {path:?}")]
    NotArray {
        /// Complete failing path.
        path: StatePath,
    },
}

/// Reserved object names remain forbidden in paths even though Rust values do
/// not have JavaScript prototype objects.  This preserves wire compatibility
/// and prevents a native producer from generating a batch a JavaScript replica
/// would reject.
pub const RESERVED_SEGMENTS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// Returns whether a UTF-16 segment is one of Chord's reserved path names.
///
/// The reserved names are ASCII, so a unit-wise comparison is exact: no
/// non-ASCII code unit can equal an ASCII byte widened to `u16`.
#[must_use]
pub fn is_reserved_segment(segment: &[u16]) -> bool {
    RESERVED_SEGMENTS.iter().any(|name| {
        name.len() == segment.len() && name.bytes().map(u16::from).eq(segment.iter().copied())
    })
}

/// Validates a path against the cross-language path grammar.
///
/// # Errors
/// Returns `DeltaError::UnsafePath` when a key segment matches a reserved
/// prototype name.
pub fn validate_path(path: &StatePath) -> Result<(), DeltaError> {
    for segment in path.as_slice() {
        if matches!(segment, PathSegment::Key(key) if is_reserved_segment(key.as_utf16())) {
            return Err(DeltaError::UnsafePath {
                segment: segment.clone(),
            });
        }
    }
    Ok(())
}

/// A decoded operation with a complete path.
#[derive(Clone, Debug, PartialEq)]
pub enum DeltaOp {
    /// Replace the complete root value.
    Replace(JsonValue),
    /// Set one object property or dense array element.
    Set(StatePath, JsonValue),
    /// Delete one object property or array element.
    Delete(StatePath),
    /// Append text to a string.
    Append(StatePath, JsString),
    /// Remove a number of UTF-16 code units from the front of a string.
    Truncate(StatePath, JsInteger),
    /// Splice an array at a path.
    Splice(StatePath, JsInteger, JsInteger, Vec<JsonValue>),
}

impl DeltaOp {
    /// Validates the decoded operation grammar and its path.
    ///
    /// # Errors
    /// Returns `DeltaError::InvalidTuple` if the operation requires a
    /// non-empty path and the path is empty, or `DeltaError::UnsafePath` if a
    /// key segment is reserved.  A splice path may be empty and is only checked
    /// for reserved segments.
    pub fn validate(&self) -> Result<(), DeltaError> {
        match self {
            Self::Replace(_) => Ok(()),
            Self::Set(path, _)
            | Self::Delete(path)
            | Self::Append(path, _)
            | Self::Truncate(path, _) => validate_non_empty(path),
            Self::Splice(path, _, _, _) => validate_path(path),
        }
    }

    /// Returns the operation path, or `None` for a root replacement.
    #[must_use]
    pub fn path(&self) -> Option<&StatePath> {
        match self {
            Self::Replace(_) => None,
            Self::Set(path, _)
            | Self::Delete(path)
            | Self::Append(path, _)
            | Self::Truncate(path, _)
            | Self::Splice(path, _, _, _) => Some(path),
        }
    }

    /// Constructs a replacement operation.
    #[must_use]
    pub fn replace(value: JsonValue) -> Self {
        Self::Replace(value)
    }

    /// Constructs a set operation.
    #[must_use]
    pub fn set(path: StatePath, value: JsonValue) -> Self {
        Self::Set(path, value)
    }

    /// Constructs a delete operation.
    #[must_use]
    pub fn delete(path: StatePath) -> Self {
        Self::Delete(path)
    }

    /// Constructs a string append operation.
    #[must_use]
    pub fn append(path: StatePath, value: impl Into<JsString>) -> Self {
        Self::Append(path, value.into())
    }

    /// Constructs a UTF-16 string truncation operation.
    #[must_use]
    pub const fn truncate(path: StatePath, count: JsInteger) -> Self {
        Self::Truncate(path, count)
    }

    /// Constructs an array splice operation.
    #[must_use]
    pub fn splice(
        path: StatePath,
        index: JsInteger,
        delete_count: JsInteger,
        values: Vec<JsonValue>,
    ) -> Self {
        Self::Splice(path, index, delete_count, values)
    }

    /// Parses one decoded-vocabulary tuple from a canonical JSON tree.
    ///
    /// Payload values are adopted as-is: the remote `is_json_value`
    /// finite/depth admission check is a boundary concern and is not
    /// re-imposed on local operation parsing.  Counts and indices still obey
    /// the operation grammar — finite, integral, non-negative binary64 with
    /// no u64 or 2^53 cap — and are clamped at application, not here.
    ///
    /// # Errors
    /// Returns `DeltaError::InvalidTuple` when the tuple has the wrong verb,
    /// arity, or shape, or `DeltaError::UnsafePath` when an inline path
    /// contains a reserved segment.
    pub fn from_json(value: &JsonValue) -> Result<Self, DeltaError> {
        let raw = value
            .as_array()
            .ok_or_else(|| invalid_tuple("op", "op is not a tuple"))?;
        let op = parse_delta_op(raw)?;
        op.validate()?;
        Ok(op)
    }

    /// Serializes the operation into its canonical tuple tree.
    ///
    /// This is the pure encoding direction and does not re-validate, matching
    /// the source's `JSON.stringify(op)`.  [`DeltaOp::from_json`] is the
    /// validating direction.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::Replace(value) => JsonValue::Array(vec![verb_value("r"), value]),
            Self::Set(path, value) => {
                JsonValue::Array(vec![verb_value("s"), path_value(path), value])
            }
            Self::Delete(path) => JsonValue::Array(vec![verb_value("d"), path_value(path)]),
            Self::Append(path, value) => JsonValue::Array(vec![
                verb_value("a"),
                path_value(path),
                JsonValue::String(value),
            ]),
            Self::Truncate(path, count) => {
                JsonValue::Array(vec![verb_value("t"), path_value(path), number_value(count)])
            }
            Self::Splice(path, index, delete_count, values) => JsonValue::Array(vec![
                verb_value("p"),
                path_value(path),
                number_value(index),
                number_value(delete_count),
                JsonValue::Array(values),
            ]),
        }
    }
}

/// Returns whether an operation replaces the complete root.
#[must_use]
pub fn is_replace(op: &DeltaOp) -> bool {
    matches!(op, DeltaOp::Replace(_))
}

/// Returns whether a decoded batch begins with a complete replacement.
#[must_use]
pub fn is_base(ops: &[DeltaOp]) -> bool {
    matches!(ops.first(), Some(DeltaOp::Replace(_)))
}

/// Returns whether a wire batch begins with a complete replacement.
#[must_use]
pub fn is_wire_base(ops: &[WireOp]) -> bool {
    matches!(ops.first(), Some(WireOp::Replace(_)))
}

/// A path reference in the wire vocabulary: an inline path or a dictionary id.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum PathRef {
    /// An inline complete path.
    Inline(StatePath),
    /// A previously defined path id.
    Id(JsInteger),
}

/// A wire operation.  Short variants omit the path and reuse the previous
/// operation's path in the same batch; `Define` is the `#` dictionary record.
#[derive(Clone, Debug, PartialEq)]
pub enum WireOp {
    /// Complete root replacement.
    Replace(JsonValue),
    /// Set with an explicit path reference.
    Set(PathRef, JsonValue),
    /// Short set using the previous path.
    SetShort(JsonValue),
    /// Delete with an explicit path reference.
    Delete(PathRef),
    /// Short delete using the previous path.
    DeleteShort,
    /// String append with an explicit path reference.
    Append(PathRef, JsString),
    /// Short string append using the previous path.
    AppendShort(JsString),
    /// UTF-16 truncation with an explicit path reference.
    Truncate(PathRef, JsInteger),
    /// Short UTF-16 truncation using the previous path.
    TruncateShort(JsInteger),
    /// Array splice with an explicit path reference.
    Splice(PathRef, JsInteger, JsInteger, Vec<JsonValue>),
    /// Short array splice using the previous path.
    SpliceShort(JsInteger, JsInteger, Vec<JsonValue>),
    /// Define a dictionary id (`#`).
    Define(JsInteger, StatePath),
}

impl WireOp {
    /// Validates wire tuple shape and all inline path segments.
    ///
    /// # Errors
    /// Returns `DeltaError::UnsafePath` when an inline path contains a reserved
    /// segment.
    pub fn validate(&self) -> Result<(), DeltaError> {
        match self {
            Self::Set(path, _)
            | Self::Delete(path)
            | Self::Append(path, _)
            | Self::Truncate(path, _)
            | Self::Splice(path, _, _, _) => validate_path_ref(path),
            Self::Define(_, path) => validate_path(path),
            Self::Replace(_)
            | Self::SetShort(_)
            | Self::DeleteShort
            | Self::AppendShort(_)
            | Self::TruncateShort(_)
            | Self::SpliceShort(_, _, _) => Ok(()),
        }
    }

    /// Parses one wire-vocabulary tuple from a canonical JSON tree.
    ///
    /// Like [`DeltaOp::from_json`], payload values are adopted without the
    /// remote finite/depth admission check.  Grammar validation of the
    /// decoded result happens in [`DeltaDecoder::decode`], matching the
    /// source's `assertValidWireOp` placement.
    ///
    /// # Errors
    /// Returns `DeltaError::InvalidTuple` when the tuple has the wrong verb,
    /// arity, or shape, or `DeltaError::UnsafePath` when an inline path
    /// contains a reserved segment.
    pub fn from_json(value: &JsonValue) -> Result<Self, DeltaError> {
        let raw = value
            .as_array()
            .ok_or_else(|| invalid_tuple("wire", "op is not a tuple"))?;
        parse_wire_op(raw)
    }

    /// Serializes the wire operation into its canonical tuple tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::Replace(value) => JsonValue::Array(vec![verb_value("r"), value]),
            Self::Set(path, value) => {
                JsonValue::Array(vec![verb_value("s"), path_ref_value(path), value])
            }
            Self::SetShort(value) => JsonValue::Array(vec![verb_value("s"), value]),
            Self::Delete(path) => JsonValue::Array(vec![verb_value("d"), path_ref_value(path)]),
            Self::DeleteShort => JsonValue::Array(vec![verb_value("d")]),
            Self::Append(path, value) => JsonValue::Array(vec![
                verb_value("a"),
                path_ref_value(path),
                JsonValue::String(value),
            ]),
            Self::AppendShort(value) => {
                JsonValue::Array(vec![verb_value("a"), JsonValue::String(value)])
            }
            Self::Truncate(path, count) => JsonValue::Array(vec![
                verb_value("t"),
                path_ref_value(path),
                number_value(count),
            ]),
            Self::TruncateShort(count) => {
                JsonValue::Array(vec![verb_value("t"), number_value(count)])
            }
            Self::Splice(path, index, delete_count, values) => JsonValue::Array(vec![
                verb_value("p"),
                path_ref_value(path),
                number_value(index),
                number_value(delete_count),
                JsonValue::Array(values),
            ]),
            Self::SpliceShort(index, delete_count, values) => JsonValue::Array(vec![
                verb_value("p"),
                number_value(index),
                number_value(delete_count),
                JsonValue::Array(values),
            ]),
            Self::Define(id, path) => {
                JsonValue::Array(vec![verb_value("#"), number_value(id), path_value(path)])
            }
        }
    }
}

/// Finds a UTF-16 suffix of `a` that is a prefix of `b`, for stream-overlap
/// detection.
///
/// The search is bounded, not exhaustive: it scans at most the last `scan`
/// code units of `a` and verifies at most eight candidate positions per probe
/// head (a 64-unit head, then a 1-unit head), so it can return 0 even when a
/// real overlap exists outside that window or budget.  `scan == 0` disables
/// probing.  A returned count is a UTF-16 code-unit count, matching
/// JavaScript's `String.length` and `slice` semantics.
#[must_use]
pub fn overlap(a: &[u16], b: &[u16], scan: usize) -> usize {
    if a.is_empty() || b.is_empty() || scan == 0 {
        return 0;
    }
    let tail_start = if a.len() > scan { a.len() - scan } else { 0 };
    let tail = &a[tail_start..];
    let probe_len = OVERLAP_PROBE.min(b.len());
    for head_len in [probe_len, 1] {
        if head_len == 0 {
            continue;
        }
        let head = &b[..head_len];
        let mut tried = 0_usize;
        let mut offset = 0_usize;
        while offset + head_len <= tail.len() {
            if tail[offset..offset + head_len] == *head {
                tried += 1;
                if tried > OVERLAP_MAX_CANDIDATES {
                    break;
                }
                let count = tail.len() - offset;
                if count <= b.len() && tail[offset..] == b[..count] {
                    return count;
                }
            }
            offset += 1;
        }
        if head_len == 1 {
            break;
        }
    }
    0
}

/// Options controlling native delta tracking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeltaTrackerOptions {
    /// Maximum UTF-16 suffix scanned by string-overlap detection.
    pub max_overlap_scan: usize,
}

impl Default for DeltaTrackerOptions {
    fn default() -> Self {
        Self {
            max_overlap_scan: DEFAULT_MAX_OVERLAP_SCAN,
        }
    }
}

/// Explicit native mutation tracker.
///
/// Use [`DeltaTracker::state_mut`] for arbitrary in-place edits, or the path
/// helpers when a caller wants validation at the mutation boundary.  The
/// tracker remembers only the last published baseline; repeated edits to one
/// path therefore coalesce at [`DeltaTracker::flush`].
#[derive(Clone, Debug)]
pub struct DeltaTracker {
    root: JsonValue,
    baseline: Option<JsonValue>,
    force_base: bool,
    dirty: bool,
    options: DeltaTrackerOptions,
}

impl DeltaTracker {
    /// Starts tracking a JSON root.  The first flush is always a replacement.
    #[must_use]
    pub fn new(root: JsonValue) -> Self {
        Self::with_options(root, DeltaTrackerOptions::default())
    }

    /// Starts tracking with explicit overlap options.
    #[must_use]
    pub fn with_options(root: JsonValue, options: DeltaTrackerOptions) -> Self {
        Self {
            root,
            baseline: None,
            force_base: true,
            dirty: false,
            options,
        }
    }

    /// Borrows the current tracked value.
    #[must_use]
    pub const fn state(&self) -> &JsonValue {
        &self.root
    }

    /// Borrows the untracked native root.  This is an alias of [`Self::state`]
    /// retained to make the source `target` distinction explicit to callers.
    #[must_use]
    pub const fn target(&self) -> &JsonValue {
        &self.root
    }

    /// Borrows the tracked value mutably and marks the tracker dirty.
    ///
    /// This is the native replacement for a JavaScript proxy.  The resulting
    /// batch is still derived from the published baseline, so no mutation log
    /// or path-specific bookkeeping leaks into the public API.
    pub fn state_mut(&mut self) -> &mut JsonValue {
        self.dirty = true;
        &mut self.root
    }

    /// Replaces the tracked root and makes the next flush a complete base.
    pub fn replace_state(&mut self, root: JsonValue) {
        self.root = root;
        self.baseline = None;
        self.force_base = true;
        self.dirty = false;
    }

    /// Replaces the tracked root; equivalent to [`Self::replace_state`].
    pub fn set_state(&mut self, root: JsonValue) {
        self.replace_state(root);
    }

    /// Returns whether a base or delta is owed to consumers.
    #[must_use]
    pub const fn dirty(&self) -> bool {
        self.force_base || self.dirty
    }

    /// Marks the current value as requiring a complete base on the next flush.
    pub fn rebase(&mut self) {
        self.force_base = true;
        self.dirty = false;
    }

    /// Accepts the current value into the local baseline without publishing it.
    ///
    /// A pending base obligation is consumed as well: the caller already holds
    /// the current value, so the next flush must not emit a duplicate
    /// replacement.
    pub fn discard(&mut self) {
        self.baseline = Some(self.root.clone());
        self.dirty = false;
        self.force_base = false;
    }

    /// Sets one path, rejecting reserved names and sparse array writes.
    ///
    /// # Errors
    /// Returns `DeltaError` when the path is empty, a key segment is reserved,
    /// the target is not the expected container, or the index would create a
    /// sparse array.
    pub fn set(&mut self, path: &StatePath, value: JsonValue) -> Result<(), DeltaError> {
        validate_non_empty(path)?;
        set_path(&mut self.root, path, value)?;
        self.dirty = true;
        Ok(())
    }

    /// Deletes one path, using array removal for dense arrays.
    ///
    /// # Errors
    /// Returns `DeltaError` when the path is empty, a key segment is reserved,
    /// the target is not the expected container, or the index is out of range.
    pub fn delete(&mut self, path: &StatePath) -> Result<(), DeltaError> {
        validate_non_empty(path)?;
        delete_path(&mut self.root, path)?;
        self.dirty = true;
        Ok(())
    }

    /// Splices an array.  `index` and `delete_count` use the non-negative wire
    /// grammar; negative JavaScript convenience indices should be normalized
    /// by the caller before crossing this explicit native API.
    ///
    /// # Errors
    /// Returns `DeltaError` when the path is empty, a key segment is reserved,
    /// the target is not an array, or the splice arguments are out of range.
    pub fn splice(
        &mut self,
        path: &StatePath,
        index: JsInteger,
        delete_count: JsInteger,
        values: Vec<JsonValue>,
    ) -> Result<(), DeltaError> {
        validate_path(path)?;
        splice_path(&mut self.root, path, index, delete_count, values)?;
        self.dirty = true;
        Ok(())
    }

    /// Splices with JavaScript-style signed start and optional delete count.
    ///
    /// A negative `start` counts from the end.  `delete_count == None` means
    /// delete through the end, matching a one-argument `splice` call.  The
    /// resulting operation still uses non-negative wire integers.
    ///
    /// # Errors
    /// Returns `DeltaError` when the path is empty, a key segment is reserved,
    /// the target is not an array, or the splice arguments are out of range.
    pub fn splice_js(
        &mut self,
        path: &StatePath,
        start: i64,
        delete_count: Option<i64>,
        values: Vec<JsonValue>,
    ) -> Result<(), DeltaError> {
        validate_path(path)?;
        let target = resolve_value(&self.root, path)?;
        let JsonValue::Array(items) = target else {
            return Err(DeltaError::NotArray { path: path.clone() });
        };
        let (index, remove) = normalize_splice(items.len(), start, delete_count);
        splice_path(&mut self.root, path, index, remove, values)?;
        self.dirty = true;
        Ok(())
    }

    /// Appends text to a string path.
    ///
    /// # Errors
    /// Returns `DeltaError` when the path is empty, a key segment is reserved,
    /// the target is not a string, or the path is unresolvable.
    pub fn append_string(
        &mut self,
        path: &StatePath,
        value: impl Into<JsString>,
    ) -> Result<(), DeltaError> {
        validate_non_empty(path)?;
        append_string_path(&mut self.root, path, &value.into())?;
        self.dirty = true;
        Ok(())
    }

    /// Removes UTF-16 code units from the front of a string path.
    ///
    /// # Errors
    /// Returns `DeltaError` when the path is empty, a key segment is reserved,
    /// the target is not a string, or the path is unresolvable.
    pub fn truncate_string(
        &mut self,
        path: &StatePath,
        count: JsInteger,
    ) -> Result<(), DeltaError> {
        validate_non_empty(path)?;
        truncate_string_path(&mut self.root, path, count)?;
        self.dirty = true;
        Ok(())
    }

    /// Pushes one or more values at the end of an array path.
    ///
    /// # Errors
    /// Returns `DeltaError` when the path is empty, a key segment is reserved,
    /// the target is not an array, or the index is out of range.
    pub fn push(&mut self, path: &StatePath, values: Vec<JsonValue>) -> Result<(), DeltaError> {
        validate_path(path)?;
        let length = match resolve_value(&self.root, path)? {
            JsonValue::Array(items) => items.len(),
            _ => return Err(DeltaError::NotArray { path: path.clone() }),
        };
        splice_path(
            &mut self.root,
            path,
            count(length),
            JsInteger::zero(),
            values,
        )?;
        self.dirty = true;
        Ok(())
    }

    /// Flushes the first base or the coalesced operations since the last flush.
    ///
    /// # Errors
    /// Returns `DeltaError` when the diff or an emitted operation is invalid.
    pub fn flush(&mut self) -> Result<Vec<DeltaOp>, DeltaError> {
        if self.force_base {
            let value = self.root.clone();
            self.baseline = Some(self.root.clone());
            self.force_base = false;
            self.dirty = false;
            return Ok(vec![DeltaOp::Replace(value)]);
        }
        if !self.dirty {
            return Ok(Vec::new());
        }
        let Some(baseline) = self.baseline.as_ref() else {
            // This is reachable only after a caller manually constructs a
            // tracker through future extensions.  A missing baseline is a base
            // obligation, never a reason to emit an unsafe partial batch.
            self.force_base = true;
            return self.flush();
        };
        let mut output = Vec::new();
        diff_value(
            Some(baseline),
            Some(&self.root),
            &StatePath::root(),
            self.options.max_overlap_scan,
            &mut output,
        )?;
        for op in &output {
            op.validate()?;
        }
        self.baseline = Some(self.root.clone());
        self.dirty = false;
        Ok(output)
    }
}

/// Applies decoded operations to a mutable JSON value.
///
/// `None` models the TypeScript `undefined` target.  A stream with no base
/// operation therefore remains `None`; a replacement produces `Some(value)`.
///
/// # Errors
/// Returns `DeltaError` when an operation is invalid or a path cannot be
/// resolved against the current value.
pub fn apply(target: Option<JsonValue>, ops: &[DeltaOp]) -> Result<Option<JsonValue>, DeltaError> {
    let mut root = target;
    for op in ops {
        op.validate()?;
        apply_one(&mut root, op)?;
    }
    Ok(root)
}

/// Applies decoded operations without mutating the input value.
///
/// [`JsonValue`] owns its children, so the returned tree is detached from the
/// input.  The operation order and path errors are identical to [`apply`].
///
/// # Errors
/// Returns `DeltaError` when an operation is invalid or a path cannot be
/// resolved against the current value.
pub fn apply_immutable(
    target: Option<&JsonValue>,
    ops: &[DeltaOp],
) -> Result<Option<JsonValue>, DeltaError> {
    let mut root = target.cloned();
    for op in ops {
        op.validate()?;
        apply_one(&mut root, op)?;
    }
    Ok(root)
}

/// A stateful path encoder.  Path ids persist across batches; previous-path
/// elision is reset for every batch.
#[derive(Clone, Debug)]
pub struct DeltaEncoder {
    seen: HashSet<StatePath>,
    ids: HashMap<StatePath, JsInteger>,
    next_id: JsInteger,
}

impl Default for DeltaEncoder {
    fn default() -> Self {
        Self {
            seen: HashSet::new(),
            ids: HashMap::new(),
            next_id: JsInteger::zero(),
        }
    }
}

impl DeltaEncoder {
    /// Creates an empty encoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes one ordered batch.
    ///
    /// # Errors
    /// Returns `DeltaError` when an operation is malformed or uses a reserved
    /// path.
    pub fn encode(&mut self, ops: &[DeltaOp]) -> Result<Vec<WireOp>, DeltaError> {
        for op in ops {
            op.validate()?;
        }
        let mut previous: Option<StatePath> = None;
        let mut output = Vec::with_capacity(ops.len());
        for op in ops {
            if let DeltaOp::Replace(value) = op {
                output.push(WireOp::Replace(value.clone()));
                self.seen.clear();
                self.ids.clear();
                self.next_id = JsInteger::zero();
                previous = None;
                continue;
            }
            let path = match op {
                DeltaOp::Set(path, _)
                | DeltaOp::Delete(path)
                | DeltaOp::Append(path, _)
                | DeltaOp::Truncate(path, _)
                | DeltaOp::Splice(path, _, _, _) => path,
                DeltaOp::Replace(_) => continue,
            };
            if previous.as_ref() == Some(path) {
                match op {
                    DeltaOp::Set(_, value) => output.push(WireOp::SetShort(value.clone())),
                    DeltaOp::Delete(_) => output.push(WireOp::DeleteShort),
                    DeltaOp::Append(_, value) => output.push(WireOp::AppendShort(value.clone())),
                    DeltaOp::Truncate(_, count) => output.push(WireOp::TruncateShort(*count)),
                    DeltaOp::Splice(_, index, delete_count, values) => {
                        output.push(WireOp::SpliceShort(*index, *delete_count, values.clone()));
                    }
                    DeltaOp::Replace(_) => continue,
                }
                continue;
            }
            let path_ref = if let Some(id) = self.ids.get(path) {
                PathRef::Id(*id)
            } else if self.seen.contains(path) {
                let id = self.next_id;
                // `nextId++` in the source is binary64 arithmetic: past 2^53
                // it stops increasing, and `JsInteger::next` reproduces that
                // exactly rather than failing on a u64 boundary.
                self.next_id = self.next_id.next();
                self.ids.insert(path.clone(), id);
                output.push(WireOp::Define(id, path.clone()));
                PathRef::Id(id)
            } else {
                self.seen.insert(path.clone());
                PathRef::Inline(path.clone())
            };
            match op {
                DeltaOp::Set(_, value) => output.push(WireOp::Set(path_ref, value.clone())),
                DeltaOp::Delete(_) => output.push(WireOp::Delete(path_ref)),
                DeltaOp::Append(_, value) => output.push(WireOp::Append(path_ref, value.clone())),
                DeltaOp::Truncate(_, count) => output.push(WireOp::Truncate(path_ref, *count)),
                DeltaOp::Splice(_, index, delete_count, values) => {
                    output.push(WireOp::Splice(
                        path_ref,
                        *index,
                        *delete_count,
                        values.clone(),
                    ));
                }
                DeltaOp::Replace(_) => continue,
            }
            previous = Some(path.clone());
        }
        Ok(output)
    }

    /// Clears dictionary state.  The next batch starts with an empty stream
    /// dictionary, while previous-path elision is already batch-local.
    pub fn reset(&mut self) {
        self.seen.clear();
        self.ids.clear();
        self.next_id = JsInteger::zero();
    }
}

/// A stateful path decoder.  Dictionary ids persist across batches while
/// previous-path elision does not.
#[derive(Clone, Debug, Default)]
pub struct DeltaDecoder {
    paths: HashMap<JsInteger, StatePath>,
}

impl DeltaDecoder {
    /// Creates an empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one ordered wire batch into complete operations.
    ///
    /// Batches are transactional: dictionary mutations from a rejected batch
    /// are rolled back, so dictionary ids defined only by the failed batch
    /// cannot be resolved by a later update.
    ///
    /// # Errors
    /// Returns `DeltaError` when a dictionary id is unresolved or a resolved
    /// path is reserved or empty.
    pub fn decode(&mut self, wire: &[WireOp]) -> Result<Vec<DeltaOp>, DeltaError> {
        let dictionary = self.paths.clone();
        let result = self.decode_batch(wire);
        if result.is_err() {
            self.paths = dictionary;
        }
        result
    }

    fn decode_batch(&mut self, wire: &[WireOp]) -> Result<Vec<DeltaOp>, DeltaError> {
        let mut previous: Option<StatePath> = None;
        let mut output = Vec::with_capacity(wire.len());
        for op in wire {
            op.validate()?;
            match op {
                WireOp::Define(id, path) => {
                    self.paths.insert(*id, path.clone());
                }
                WireOp::Replace(value) => {
                    output.push(DeltaOp::Replace(value.clone()));
                    self.paths.clear();
                    previous = None;
                }
                WireOp::SetShort(value) => {
                    let path = previous.clone().ok_or_else(|| DeltaError::Path {
                        path: StatePath::root(),
                    })?;
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Set(path, value.clone()));
                }
                WireOp::DeleteShort => {
                    let path = previous.clone().ok_or_else(|| DeltaError::Path {
                        path: StatePath::root(),
                    })?;
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Delete(path));
                }
                WireOp::AppendShort(value) => {
                    let path = previous.clone().ok_or_else(|| DeltaError::Path {
                        path: StatePath::root(),
                    })?;
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Append(path, value.clone()));
                }
                WireOp::TruncateShort(count) => {
                    let path = previous.clone().ok_or_else(|| DeltaError::Path {
                        path: StatePath::root(),
                    })?;
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Truncate(path, *count));
                }
                WireOp::SpliceShort(index, delete_count, values) => {
                    let path = previous.clone().ok_or_else(|| DeltaError::Path {
                        path: StatePath::root(),
                    })?;
                    output.push(DeltaOp::Splice(path, *index, *delete_count, values.clone()));
                }
                WireOp::Set(path_ref, value) => {
                    let path = resolve_path_ref(&self.paths, path_ref)?;
                    previous = Some(path.clone());
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Set(path, value.clone()));
                }
                WireOp::Delete(path_ref) => {
                    let path = resolve_path_ref(&self.paths, path_ref)?;
                    previous = Some(path.clone());
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Delete(path));
                }
                WireOp::Append(path_ref, value) => {
                    let path = resolve_path_ref(&self.paths, path_ref)?;
                    previous = Some(path.clone());
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Append(path, value.clone()));
                }
                WireOp::Truncate(path_ref, count) => {
                    let path = resolve_path_ref(&self.paths, path_ref)?;
                    previous = Some(path.clone());
                    require_decoded_non_empty(&path)?;
                    output.push(DeltaOp::Truncate(path, *count));
                }
                WireOp::Splice(path_ref, index, delete_count, values) => {
                    let path = resolve_path_ref(&self.paths, path_ref)?;
                    previous = Some(path.clone());
                    output.push(DeltaOp::Splice(path, *index, *delete_count, values.clone()));
                }
            }
        }
        Ok(output)
    }

    /// Clears dictionary state after abandoning a stream.
    pub fn reset(&mut self) {
        self.paths.clear();
    }
}

/// Validates a decoded operation, mirroring the TypeScript assertion helper.
///
/// # Errors
/// Returns `DeltaError` when the operation is invalid.
pub fn assert_valid_op(op: &DeltaOp) -> Result<(), DeltaError> {
    op.validate()
}

/// Validates a wire operation, mirroring the TypeScript assertion helper.
///
/// # Errors
/// Returns `DeltaError` when the operation is invalid.
pub fn assert_valid_wire_op(op: &WireOp) -> Result<(), DeltaError> {
    op.validate()
}

fn validate_non_empty(path: &StatePath) -> Result<(), DeltaError> {
    validate_path(path)?;
    if path.is_empty() {
        return Err(DeltaError::InvalidTuple {
            kind: "op",
            reason: "path is empty".to_owned(),
        });
    }
    Ok(())
}

fn require_decoded_non_empty(path: &StatePath) -> Result<(), DeltaError> {
    validate_path(path)?;
    if path.is_empty() {
        return Err(DeltaError::Path { path: path.clone() });
    }
    Ok(())
}

fn validate_path_ref(path_ref: &PathRef) -> Result<(), DeltaError> {
    match path_ref {
        PathRef::Inline(path) => validate_path(path),
        PathRef::Id(_) => Ok(()),
    }
}

fn resolve_path_ref(
    paths: &HashMap<JsInteger, StatePath>,
    path_ref: &PathRef,
) -> Result<StatePath, DeltaError> {
    match path_ref {
        PathRef::Inline(path) => Ok(path.clone()),
        PathRef::Id(id) => paths.get(id).cloned().ok_or(DeltaError::PathId(*id)),
    }
}

fn parse_delta_op(raw: &[JsonValue]) -> Result<DeltaOp, DeltaError> {
    let verb = raw
        .first()
        .and_then(JsonValue::as_str)
        .ok_or_else(|| invalid_tuple("op", "op is not a tuple"))?;
    match verb_code(verb) {
        Some(b'r') => {
            if raw.len() != 2 {
                return Err(invalid_tuple("op", "r arity"));
            }
            Ok(DeltaOp::Replace(raw[1].clone()))
        }
        Some(b's') => {
            if raw.len() != 3 {
                return Err(invalid_tuple("op", "s arity"));
            }
            Ok(DeltaOp::Set(
                parse_path_value(&raw[1], "op")?,
                raw[2].clone(),
            ))
        }
        Some(b'd') => {
            if raw.len() != 2 {
                return Err(invalid_tuple("op", "d arity"));
            }
            Ok(DeltaOp::Delete(parse_path_value(&raw[1], "op")?))
        }
        Some(b'a') => {
            if raw.len() != 3 {
                return Err(invalid_tuple("op", "a arity"));
            }
            Ok(DeltaOp::Append(
                parse_path_value(&raw[1], "op")?,
                parse_string_value(&raw[2], "op", "a value")?,
            ))
        }
        Some(b't') => {
            if raw.len() != 3 {
                return Err(invalid_tuple("op", "t arity"));
            }
            Ok(DeltaOp::Truncate(
                parse_path_value(&raw[1], "op")?,
                parse_nonnegative_integer(&raw[2], "op", "t count")?,
            ))
        }
        Some(b'p') => {
            if raw.len() != 5 {
                return Err(invalid_tuple("op", "p arity"));
            }
            Ok(DeltaOp::Splice(
                parse_path_value(&raw[1], "op")?,
                parse_nonnegative_integer(&raw[2], "op", "p index")?,
                parse_nonnegative_integer(&raw[3], "op", "p remove")?,
                parse_items(&raw[4], "op")?,
            ))
        }
        _ => Err(invalid_tuple("op", &unknown_verb(verb))),
    }
}

fn parse_wire_op(raw: &[JsonValue]) -> Result<WireOp, DeltaError> {
    let verb = raw
        .first()
        .and_then(JsonValue::as_str)
        .ok_or_else(|| invalid_tuple("wire", "op is not a tuple"))?;
    match verb_code(verb) {
        Some(b'r') => {
            if raw.len() != 2 {
                return Err(invalid_tuple("wire", "r arity"));
            }
            Ok(WireOp::Replace(raw[1].clone()))
        }
        Some(b's') => match raw.len() {
            2 => Ok(WireOp::SetShort(raw[1].clone())),
            3 => Ok(WireOp::Set(
                parse_path_ref(&raw[1], "wire")?,
                raw[2].clone(),
            )),
            _ => Err(invalid_tuple("wire", "s arity")),
        },
        Some(b'd') => match raw.len() {
            1 => Ok(WireOp::DeleteShort),
            2 => Ok(WireOp::Delete(parse_path_ref(&raw[1], "wire")?)),
            _ => Err(invalid_tuple("wire", "d arity")),
        },
        Some(b'a') => match raw.len() {
            2 => Ok(WireOp::AppendShort(parse_string_value(
                &raw[1], "wire", "a value",
            )?)),
            3 => Ok(WireOp::Append(
                parse_path_ref(&raw[1], "wire")?,
                parse_string_value(&raw[2], "wire", "a value")?,
            )),
            _ => Err(invalid_tuple("wire", "a arity")),
        },
        Some(b't') => match raw.len() {
            2 => Ok(WireOp::TruncateShort(parse_nonnegative_integer(
                &raw[1], "wire", "t count",
            )?)),
            3 => Ok(WireOp::Truncate(
                parse_path_ref(&raw[1], "wire")?,
                parse_nonnegative_integer(&raw[2], "wire", "t count")?,
            )),
            _ => Err(invalid_tuple("wire", "t arity")),
        },
        Some(b'p') => match raw.len() {
            4 => Ok(WireOp::SpliceShort(
                parse_nonnegative_integer(&raw[1], "wire", "p index")?,
                parse_nonnegative_integer(&raw[2], "wire", "p remove")?,
                parse_items(&raw[3], "wire")?,
            )),
            5 => Ok(WireOp::Splice(
                parse_path_ref(&raw[1], "wire")?,
                parse_nonnegative_integer(&raw[2], "wire", "p index")?,
                parse_nonnegative_integer(&raw[3], "wire", "p remove")?,
                parse_items(&raw[4], "wire")?,
            )),
            _ => Err(invalid_tuple("wire", "p arity")),
        },
        Some(b'#') => {
            if raw.len() != 3 {
                return Err(invalid_tuple("wire", "# shape"));
            }
            Ok(WireOp::Define(
                parse_nonnegative_integer(&raw[1], "wire", "# id")?,
                parse_path_value(&raw[2], "wire")?,
            ))
        }
        _ => Err(invalid_tuple("wire", &unknown_verb(verb))),
    }
}

fn parse_path_value(value: &JsonValue, kind: &'static str) -> Result<StatePath, DeltaError> {
    let segments = value
        .as_array()
        .ok_or_else(|| invalid_tuple(kind, "path is not an array"))?
        .iter()
        .map(|item| parse_path_segment(item, kind))
        .collect::<Result<Vec<_>, _>>()?;
    let path = StatePath::from_segments_unchecked(segments);
    path.validate()?;
    Ok(path)
}

fn parse_path_ref(value: &JsonValue, kind: &'static str) -> Result<PathRef, DeltaError> {
    if value.as_array().is_some() {
        Ok(PathRef::Inline(parse_path_value(value, kind)?))
    } else {
        Ok(PathRef::Id(parse_nonnegative_integer(
            value,
            kind,
            "bad path id",
        )?))
    }
}

fn parse_path_segment(value: &JsonValue, kind: &'static str) -> Result<PathSegment, DeltaError> {
    if let Some(key) = value.as_str() {
        return Ok(PathSegment::Key(key.clone()));
    }
    Ok(PathSegment::Index(parse_nonnegative_integer(
        value,
        kind,
        "path segment",
    )?))
}

fn parse_string_value(
    value: &JsonValue,
    kind: &'static str,
    reason: &'static str,
) -> Result<JsString, DeltaError> {
    value
        .as_str()
        .cloned()
        .ok_or_else(|| invalid_tuple(kind, reason))
}

fn parse_items(value: &JsonValue, kind: &'static str) -> Result<Vec<JsonValue>, DeltaError> {
    value
        .as_array()
        .cloned()
        .ok_or_else(|| invalid_tuple(kind, "p items"))
}

fn parse_nonnegative_integer(
    value: &JsonValue,
    kind: &'static str,
    reason: &'static str,
) -> Result<JsInteger, DeltaError> {
    let Some(number) = value.as_f64() else {
        return Err(invalid_tuple(kind, reason));
    };
    // The source domain is `Number.isInteger(value) && value >= 0` over
    // binary64: no u64 or 2^53 admission cap.  Magnitude meets a concrete
    // collection only at application, where it clamps or range-fails.
    JsInteger::new(number).map_err(|_| invalid_tuple(kind, reason))
}

/// Maps a verb string to its single ASCII byte.  Every verb in both
/// vocabularies is one ASCII code unit, so a multi-unit or non-ASCII verb is
/// simply unknown.
fn verb_code(verb: &JsString) -> Option<u8> {
    match verb.as_utf16() {
        [unit] => u8::try_from(*unit).ok(),
        _ => None,
    }
}

fn unknown_verb(verb: &JsString) -> String {
    // Diagnostic only: a verb holding unpaired surrogates renders with
    // replacement characters because a Rust `String` cannot hold them.
    format!(
        "unknown op verb: {}",
        String::from_utf16_lossy(verb.as_utf16())
    )
}

fn invalid_tuple(kind: &'static str, reason: &str) -> DeltaError {
    DeltaError::InvalidTuple {
        kind,
        reason: reason.to_owned(),
    }
}

fn verb_value(verb: &'static str) -> JsonValue {
    JsonValue::String(JsString::from_utf8(verb))
}

fn number_value(value: JsInteger) -> JsonValue {
    JsonValue::Number(value.as_f64())
}

fn segment_value(segment: PathSegment) -> JsonValue {
    match segment {
        PathSegment::Key(key) => JsonValue::String(key),
        PathSegment::Index(index) => number_value(index),
    }
}

fn path_value(path: StatePath) -> JsonValue {
    JsonValue::Array(path.into_vec().into_iter().map(segment_value).collect())
}

fn path_ref_value(path_ref: PathRef) -> JsonValue {
    match path_ref {
        PathRef::Inline(path) => path_value(path),
        PathRef::Id(id) => number_value(id),
    }
}

fn diff_value(
    before: Option<&JsonValue>,
    after: Option<&JsonValue>,
    path: &StatePath,
    scan: usize,
    output: &mut Vec<DeltaOp>,
) -> Result<(), DeltaError> {
    match (before, after) {
        (None, None) => {}
        (None, Some(value)) => emit_set(path, value.clone(), output),
        (Some(_), None) => emit_delete(path, output)?,
        (Some(before), Some(after)) => {
            if before == after {
                return Ok(());
            }
            match (before, after) {
                (JsonValue::String(before), JsonValue::String(after)) => {
                    diff_string(before.as_utf16(), after.as_utf16(), path, scan, output);
                }
                (JsonValue::Array(before), JsonValue::Array(after)) => {
                    diff_array(before, after, path, scan, output)?;
                }
                (JsonValue::Object(before), JsonValue::Object(after)) => {
                    diff_object(before, after, path, scan, output)?;
                }
                _ => emit_set(path, after.clone(), output),
            }
        }
    }
    Ok(())
}

fn diff_string(
    before: &[u16],
    after: &[u16],
    path: &StatePath,
    scan: usize,
    output: &mut Vec<DeltaOp>,
) {
    if before == after {
        return;
    }
    if path.is_empty() {
        emit_set(
            path,
            JsonValue::String(JsString::from_utf16(after.to_vec())),
            output,
        );
        return;
    }
    if after.len() > before.len() && after.starts_with(before) {
        output.push(DeltaOp::Append(
            path.clone(),
            JsString::from_utf16(after[before.len()..].to_vec()),
        ));
        return;
    }
    let shared = overlap(before, after, scan);
    if shared == 0 {
        emit_set(
            path,
            JsonValue::String(JsString::from_utf16(after.to_vec())),
            output,
        );
        return;
    }
    output.push(DeltaOp::Truncate(
        path.clone(),
        count(before.len() - shared),
    ));
    if after.len() > shared {
        // The append may begin inside what was a surrogate pair in `before`;
        // UTF-16 code units keep that split lossless.
        output.push(DeltaOp::Append(
            path.clone(),
            JsString::from_utf16(after[shared..].to_vec()),
        ));
    }
}

fn diff_object(
    before: &JsObject,
    after: &JsObject,
    path: &StatePath,
    scan: usize,
    output: &mut Vec<DeltaOp>,
) -> Result<(), DeltaError> {
    if before
        .keys()
        .chain(after.keys())
        .any(|key| is_reserved_segment(key.as_utf16()))
    {
        emit_set(path, JsonValue::Object(after.clone()), output);
        return Ok(());
    }
    for (key, value) in after {
        let child_path = path.joined(PathSegment::Key(key.clone()));
        diff_value(
            before.get(key.as_utf16()),
            Some(value),
            &child_path,
            scan,
            output,
        )?;
    }
    for key in before.keys() {
        if !after.contains_key(key.as_utf16()) {
            let child_path = path.joined(PathSegment::Key(key.clone()));
            emit_delete(&child_path, output)?;
        }
    }
    Ok(())
}

fn diff_array(
    before: &[JsonValue],
    after: &[JsonValue],
    path: &StatePath,
    scan: usize,
    output: &mut Vec<DeltaOp>,
) -> Result<(), DeltaError> {
    if before.len() == after.len() {
        for (index, (left, right)) in before.iter().zip(after).enumerate() {
            diff_value(
                Some(left),
                Some(right),
                &path.joined(PathSegment::Index(count(index))),
                scan,
                output,
            )?;
        }
        return Ok(());
    }
    let mut prefix = 0_usize;
    while prefix < before.len() && prefix < after.len() && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0_usize;
    while suffix < before.len() - prefix
        && suffix < after.len() - prefix
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let shorter = before.len().min(after.len());
    if prefix + suffix == shorter {
        let remove = before.len() - prefix - suffix;
        let items = after[prefix..after.len() - suffix].to_vec();
        if prefix == 0 && remove == before.len() {
            emit_set(path, JsonValue::Array(after.to_vec()), output);
        } else {
            output.push(DeltaOp::Splice(
                path.clone(),
                count(prefix),
                count(remove),
                items,
            ));
        }
        return Ok(());
    }
    for index in 0..shorter {
        diff_value(
            Some(&before[index]),
            Some(&after[index]),
            &path.joined(PathSegment::Index(count(index))),
            scan,
            output,
        )?;
    }
    if after.len() > before.len() {
        output.push(DeltaOp::Splice(
            path.clone(),
            count(before.len()),
            JsInteger::zero(),
            after[before.len()..].to_vec(),
        ));
    } else if before.len() > after.len() {
        if after.is_empty() {
            emit_set(path, JsonValue::Array(Vec::new()), output);
        } else {
            output.push(DeltaOp::Splice(
                path.clone(),
                count(after.len()),
                count(before.len() - after.len()),
                Vec::new(),
            ));
        }
    }
    Ok(())
}

fn emit_set(path: &StatePath, value: JsonValue, output: &mut Vec<DeltaOp>) {
    if path.is_empty() {
        output.push(DeltaOp::Replace(value));
    } else {
        output.push(DeltaOp::Set(path.clone(), value));
    }
}

fn emit_delete(path: &StatePath, output: &mut Vec<DeltaOp>) -> Result<(), DeltaError> {
    if path.is_empty() {
        return Err(DeltaError::RootDelete);
    }
    output.push(DeltaOp::Delete(path.clone()));
    Ok(())
}

fn apply_one(root: &mut Option<JsonValue>, op: &DeltaOp) -> Result<(), DeltaError> {
    match op {
        DeltaOp::Replace(value) => {
            *root = Some(value.clone());
            Ok(())
        }
        DeltaOp::Set(path, value) => {
            let target = root
                .as_mut()
                .ok_or_else(|| DeltaError::Path { path: path.clone() })?;
            set_path(target, path, value.clone())
        }
        DeltaOp::Delete(path) => {
            let target = root
                .as_mut()
                .ok_or_else(|| DeltaError::Path { path: path.clone() })?;
            delete_path(target, path)
        }
        DeltaOp::Append(path, value) => {
            let target = root
                .as_mut()
                .ok_or_else(|| DeltaError::Path { path: path.clone() })?;
            append_string_path(target, path, value)
        }
        DeltaOp::Truncate(path, count) => {
            let target = root
                .as_mut()
                .ok_or_else(|| DeltaError::Path { path: path.clone() })?;
            truncate_string_path(target, path, *count)
        }
        DeltaOp::Splice(path, index, delete_count, values) => {
            let target = root
                .as_mut()
                .ok_or_else(|| DeltaError::Path { path: path.clone() })?;
            splice_path(target, path, *index, *delete_count, values.clone())
        }
    }
}

fn resolve_value<'a>(root: &'a JsonValue, path: &StatePath) -> Result<&'a JsonValue, DeltaError> {
    let mut current = root;
    for segment in path.as_slice() {
        current = match current {
            JsonValue::Object(map) => {
                object_get(map, segment).ok_or_else(|| DeltaError::Path { path: path.clone() })?
            }
            JsonValue::Array(items) => {
                let Some(index) = segment.as_index() else {
                    return Err(DeltaError::UnsafePath {
                        segment: segment.clone(),
                    });
                };
                let index = existing_index(index, items.len(), path)?;
                items
                    .get(index)
                    .ok_or_else(|| DeltaError::Path { path: path.clone() })?
            }
            _ => return Err(DeltaError::Path { path: path.clone() }),
        };
    }
    Ok(current)
}

fn resolve_parent_mut<'a>(
    root: &'a mut JsonValue,
    path: &StatePath,
) -> Result<(&'a mut JsonValue, PathSegment), DeltaError> {
    let Some((last, parents)) = path.as_slice().split_last() else {
        return Err(DeltaError::Path { path: path.clone() });
    };
    let mut current = root;
    for segment in parents {
        current = match current {
            JsonValue::Object(map) => object_get_mut(map, segment)
                .ok_or_else(|| DeltaError::Path { path: path.clone() })?,
            JsonValue::Array(items) => {
                let Some(index) = segment.as_index() else {
                    return Err(DeltaError::UnsafePath {
                        segment: segment.clone(),
                    });
                };
                let index = existing_index(index, items.len(), path)?;
                items
                    .get_mut(index)
                    .ok_or_else(|| DeltaError::Path { path: path.clone() })?
            }
            _ => return Err(DeltaError::Path { path: path.clone() }),
        };
    }
    Ok((current, last.clone()))
}

fn set_path(root: &mut JsonValue, path: &StatePath, value: JsonValue) -> Result<(), DeltaError> {
    validate_non_empty(path)?;
    let (parent, segment) = resolve_parent_mut(root, path)?;
    match parent {
        JsonValue::Object(map) => {
            map.insert(object_key(&segment), value);
            Ok(())
        }
        JsonValue::Array(items) => {
            let Some(index) = segment.as_index() else {
                return Err(DeltaError::UnsafePath {
                    segment: segment.clone(),
                });
            };
            let index = appendable_index(index, items.len())?;
            if index == items.len() {
                items.push(value);
            } else if let Some(slot) = items.get_mut(index) {
                *slot = value;
            } else {
                return Err(DeltaError::Path { path: path.clone() });
            }
            Ok(())
        }
        _ => Err(DeltaError::Path { path: path.clone() }),
    }
}

fn delete_path(root: &mut JsonValue, path: &StatePath) -> Result<(), DeltaError> {
    validate_non_empty(path)?;
    let (parent, segment) = resolve_parent_mut(root, path)?;
    match parent {
        JsonValue::Object(map) => {
            drop(map.remove(object_key(&segment).as_utf16()));
            Ok(())
        }
        JsonValue::Array(items) => {
            let Some(index) = segment.as_index() else {
                return Err(DeltaError::UnsafePath {
                    segment: segment.clone(),
                });
            };
            let index = existing_index(index, items.len(), path)?;
            items.remove(index);
            Ok(())
        }
        _ => Err(DeltaError::Path { path: path.clone() }),
    }
}

fn normalize_splice(
    length: usize,
    start: i64,
    delete_count: Option<i64>,
) -> (JsInteger, JsInteger) {
    let length = u64::try_from(length).unwrap_or(u64::MAX);
    let index = if start < 0 {
        length.saturating_sub(start.unsigned_abs())
    } else {
        start.unsigned_abs().min(length)
    };
    let remove = match delete_count {
        None => length - index,
        Some(count) if count <= 0 => 0,
        Some(count) => count.unsigned_abs().min(length - index),
    };
    (integer(index), integer(remove))
}

fn splice_path(
    root: &mut JsonValue,
    path: &StatePath,
    index: JsInteger,
    delete_count: JsInteger,
    values: Vec<JsonValue>,
) -> Result<(), DeltaError> {
    validate_path(path)?;
    let target = if path.is_empty() {
        root
    } else {
        resolve_value_mut(root, path)?
    };
    let JsonValue::Array(items) = target else {
        return Err(DeltaError::NotArray { path: path.clone() });
    };
    // JavaScript splice clamps both arguments to the current length; the
    // admitted integer domain is unbounded, so the clamp — not the parser —
    // is where magnitude meets the collection.
    let index = index.clamp_to_len(items.len());
    let remove = delete_count.clamp_to_len(items.len() - index);
    drop(items.splice(index..index + remove, values));
    Ok(())
}

fn append_string_path(
    root: &mut JsonValue,
    path: &StatePath,
    value: &JsString,
) -> Result<(), DeltaError> {
    let target = resolve_value_mut(root, path)?;
    let JsonValue::String(current) = target else {
        return Err(DeltaError::NotString { path: path.clone() });
    };
    // JavaScript concatenates code units verbatim: appending a lone surrogate
    // never combines it with a trailing half of a pair.
    let mut units = Vec::with_capacity(current.as_utf16().len() + value.as_utf16().len());
    units.extend_from_slice(current.as_utf16());
    units.extend_from_slice(value.as_utf16());
    *current = JsString::from_utf16(units);
    Ok(())
}

fn truncate_string_path(
    root: &mut JsonValue,
    path: &StatePath,
    count: JsInteger,
) -> Result<(), DeltaError> {
    let target = resolve_value_mut(root, path)?;
    let JsonValue::String(current) = target else {
        return Err(DeltaError::NotString { path: path.clone() });
    };
    // `slice(count)` clamps past the end to the empty string, and a count
    // landing inside a surrogate pair keeps the trailing half — both are
    // exact UTF-16 code-unit behavior.
    let start = count.clamp_to_len(current.as_utf16().len());
    let truncated = JsString::from_utf16(current.as_utf16()[start..].to_vec());
    *current = truncated;
    Ok(())
}

fn resolve_value_mut<'a>(
    root: &'a mut JsonValue,
    path: &StatePath,
) -> Result<&'a mut JsonValue, DeltaError> {
    let mut current = root;
    for segment in path.as_slice() {
        current = match current {
            JsonValue::Object(map) => object_get_mut(map, segment)
                .ok_or_else(|| DeltaError::Path { path: path.clone() })?,
            JsonValue::Array(items) => {
                let Some(index) = segment.as_index() else {
                    return Err(DeltaError::UnsafePath {
                        segment: segment.clone(),
                    });
                };
                let index = existing_index(index, items.len(), path)?;
                items
                    .get_mut(index)
                    .ok_or_else(|| DeltaError::Path { path: path.clone() })?
            }
            _ => return Err(DeltaError::Path { path: path.clone() }),
        };
    }
    Ok(current)
}

fn object_get<'a>(map: &'a JsObject, segment: &PathSegment) -> Option<&'a JsonValue> {
    match segment {
        PathSegment::Key(key) => map.get(key.as_utf16()),
        PathSegment::Index(index) => map.get(index_key(*index).as_utf16()),
    }
}

fn object_get_mut<'a>(map: &'a mut JsObject, segment: &PathSegment) -> Option<&'a mut JsonValue> {
    match segment {
        PathSegment::Key(key) => map.get_mut(key.as_utf16()),
        PathSegment::Index(index) => map.get_mut(index_key(*index).as_utf16()),
    }
}

fn object_key(segment: &PathSegment) -> JsString {
    match segment {
        PathSegment::Key(key) => key.clone(),
        PathSegment::Index(index) => index_key(*index),
    }
}

/// The object-member spelling of an index segment: the ECMAScript
/// `Number::toString` form, matching `object[index]` key coercion.
fn index_key(index: JsInteger) -> JsString {
    JsString::from_utf8(&js_number_to_string(index.as_f64()))
}

fn existing_index(index: JsInteger, length: usize, path: &StatePath) -> Result<usize, DeltaError> {
    let index = index.clamp_to_len(length);
    if index >= length {
        return Err(DeltaError::Path { path: path.clone() });
    }
    Ok(index)
}

fn appendable_index(index: JsInteger, length: usize) -> Result<usize, DeltaError> {
    let clamped = index.clamp_to_len(length.saturating_add(1));
    if clamped > length {
        return Err(DeltaError::SparseArray { index });
    }
    Ok(clamped)
}

/// Converts a `usize` into the binary64 integer domain.  The cast rounds to
/// the nearest binary64, which stays finite, nonnegative, and integral, so
/// the checked constructor cannot fail here.
fn count(value: usize) -> JsInteger {
    #[expect(
        clippy::cast_precision_loss,
        reason = "binary64 rounding is the canonical integer coercion"
    )]
    let value = value as f64;
    JsInteger::new(value).unwrap_or_else(|_| JsInteger::zero())
}

/// Converts a `u64` into the binary64 integer domain, with the same rounding
/// guarantee as [`count`].
fn integer(value: u64) -> JsInteger {
    #[expect(
        clippy::cast_precision_loss,
        reason = "binary64 rounding is the canonical integer coercion"
    )]
    let value = value as f64;
    JsInteger::new(value).unwrap_or_else(|_| JsInteger::zero())
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "decoder tests use contextual fixture failures")]
mod tests {
    use super::*;
    use crate::service::value::{JsonError, ValueError, parse_json, stringify_json};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn path(segments: Vec<PathSegment>) -> Result<StatePath, DeltaError> {
        StatePath::new(segments)
    }

    fn json(text: &str) -> Result<JsonValue, JsonError> {
        parse_json(text)
    }

    fn js(text: &str) -> JsString {
        JsString::from_utf8(text)
    }

    fn int(value: f64) -> Result<JsInteger, ValueError> {
        JsInteger::new(value)
    }

    fn wire_json(ops: &[WireOp]) -> JsonValue {
        JsonValue::Array(ops.iter().cloned().map(WireOp::into_json).collect())
    }

    fn string_member<'a>(
        value: &'a JsonValue,
        key: &str,
    ) -> Result<&'a JsString, Box<dyn std::error::Error>> {
        let JsonValue::Object(map) = value else {
            return Err("expected object".into());
        };
        let Some(JsonValue::String(text)) = map.get(js(key).as_utf16()) else {
            return Err("expected string member".into());
        };
        Ok(text)
    }

    #[test]
    fn tuple_serialization_matches_chord() -> TestResult {
        let op = DeltaOp::Append(path(vec![PathSegment::key("output")])?, js("done\n"));
        let encoded = op.clone().into_json();
        assert_eq!(encoded, json(r#"["a",["output"],"done\n"]"#)?);
        assert_eq!(stringify_json(&encoded), r#"["a",["output"],"done\n"]"#);
        let decoded = DeltaOp::from_json(&encoded)?;
        assert_eq!(decoded, op);
        Ok(())
    }

    #[test]
    fn encoder_interns_on_second_use_and_decoder_keeps_ids_across_batches() -> TestResult {
        let p = path(vec![PathSegment::key("a"), PathSegment::key("deep")])?;
        let ops = vec![DeltaOp::Append(p.clone(), js("1"))];
        let mut encoder = DeltaEncoder::new();
        let first = encoder.encode(&ops)?;
        assert_eq!(wire_json(&first), json(r#"[["a",["a","deep"],"1"]]"#)?);
        let second = encoder.encode(&[DeltaOp::Append(p.clone(), js("2"))])?;
        assert_eq!(
            wire_json(&second),
            json(r##"[["#",0,["a","deep"]],["a",0,"2"]]"##)?
        );
        let mut decoder = DeltaDecoder::new();
        assert_eq!(decoder.decode(&first)?, ops);
        assert_eq!(decoder.decode(&second)?, vec![DeltaOp::Append(p, js("2"))]);
        Ok(())
    }

    #[test]
    fn previous_path_is_batch_local() -> TestResult {
        let p = path(vec![PathSegment::key("x")])?;
        let mut decoder = DeltaDecoder::new();
        let first = vec![WireOp::Set(PathRef::Inline(p.clone()), json("1")?)];
        decoder.decode(&first)?;
        assert!(decoder.decode(&[WireOp::SetShort(json("2")?)]).is_err());
        Ok(())
    }

    #[test]
    fn utf16_truncation_counts_code_units() -> TestResult {
        let p = path(vec![PathSegment::key("text")])?;
        let result = apply(
            Some(json(r#"{"text":"😀abc"}"#)?),
            &[DeltaOp::Truncate(p.clone(), int(2.0)?)],
        )?
        .ok_or("root")?;
        assert_eq!(result, json(r#"{"text":"abc"}"#)?);
        let append = apply(
            Some(json(r#"{"text":"😀"}"#)?),
            &[DeltaOp::Append(p, js("!"))],
        )?
        .ok_or("root")?;
        assert_eq!(append, json(r#"{"text":"😀!"}"#)?);
        Ok(())
    }

    #[test]
    fn utf16_edits_split_surrogate_pairs_losslessly() -> TestResult {
        let p = path(vec![PathSegment::key("text")])?;
        // Truncating one code unit off "😀" leaves the lone low surrogate,
        // exactly like JavaScript's "😀".slice(1).
        let result = apply(
            Some(json(r#"{"text":"😀"}"#)?),
            &[DeltaOp::Truncate(p.clone(), int(1.0)?)],
        )?
        .ok_or("root")?;
        assert_eq!(
            string_member(&result, "text")?.as_utf16(),
            [0xDE00].as_slice()
        );

        // Appending a lone high surrogate keeps the units verbatim —
        // JavaScript concatenates code units without combining them.
        let result = apply(
            Some(result),
            &[DeltaOp::Append(
                p.clone(),
                JsString::from_utf16(vec![0xD800]),
            )],
        )?
        .ok_or("root")?;
        assert_eq!(
            string_member(&result, "text")?.as_utf16(),
            [0xDE00, 0xD800].as_slice()
        );

        // A lone-surrogate append survives the tuple round trip: the writer
        // keeps it as a \udXXX escape and the parser restores the same units.
        let op = DeltaOp::Append(p, JsString::from_utf16(vec![0xD800]));
        let encoded = op.clone().into_json();
        assert_eq!(stringify_json(&encoded), r#"["a",["text"],"\ud800"]"#);
        assert_eq!(DeltaOp::from_json(&encoded)?, op);
        Ok(())
    }

    #[test]
    fn tracker_diff_may_truncate_into_a_surrogate_pair() -> TestResult {
        let mut tracker = DeltaTracker::new(json(r#"{"text":"a😀"}"#)?);
        tracker.flush()?;
        let text_path = path(vec![PathSegment::key("text")])?;
        tracker.set(
            &text_path,
            JsonValue::String(JsString::from_utf16(vec![0xDE00])),
        )?;
        let ops = tracker.flush()?;
        // The shared suffix is the lone low surrogate: a two-unit truncate
        // splits the pair, which the canonical string represents losslessly.
        assert_eq!(
            ops,
            vec![DeltaOp::Truncate(
                path(vec![PathSegment::key("text")])?,
                int(2.0)?
            )]
        );
        let applied = apply(Some(json(r#"{"text":"a😀"}"#)?), &ops)?.ok_or("root")?;
        assert_eq!(
            string_member(&applied, "text")?.as_utf16(),
            [0xDE00].as_slice()
        );
        Ok(())
    }

    #[test]
    fn lone_surrogate_keys_and_payloads_round_trip() -> TestResult {
        // A key that is not UTF-8 is still an exact path segment.
        let key = JsString::from_utf16(vec![0xD800]);
        let op = DeltaOp::from_json(&json(r#"["s",["\ud800"],1]"#)?)?;
        assert_eq!(
            op,
            DeltaOp::Set(
                StatePath::from(vec![PathSegment::Key(key.clone())]),
                json("1")?
            )
        );
        let result = apply(Some(json(r#"{"a":0}"#)?), &[op])?.ok_or("root")?;
        let expected = json("1")?;
        let JsonValue::Object(map) = &result else {
            return Err("expected object".into());
        };
        assert_eq!(map.get(key.as_utf16()), Some(&expected));
        // And the operation serializes back to the same escape.
        let encoded =
            DeltaOp::Set(StatePath::from(vec![PathSegment::Key(key)]), json("1")?).into_json();
        assert_eq!(stringify_json(&encoded), r#"["s",["\ud800"],1]"#);
        Ok(())
    }

    #[test]
    fn counts_beyond_u64_parse_and_clamp_at_application() -> TestResult {
        // The count domain is finite integral nonnegative binary64 — no u64
        // or 2^53 admission cap.
        let op = DeltaOp::from_json(&json(r#"["t",["s"],1e30]"#)?)?;
        let DeltaOp::Truncate(_, count) = op else {
            return Err("expected truncate".into());
        };
        assert_eq!(count.as_f64().to_bits(), 1e30_f64.to_bits());

        // Magnitude clamps to the current collection length at application,
        // as String.slice and Array.prototype.splice do.
        let t = DeltaOp::from_json(&json(r#"["t",["s"],1e30]"#)?)?;
        let s = DeltaOp::from_json(&json(r#"["p",["xs"],0,1e9,[]]"#)?)?;
        let result = apply(Some(json(r#"{"s":"abc","xs":[1,2]}"#)?), &[t, s])?.ok_or("root")?;
        assert_eq!(result, json(r#"{"s":"","xs":[]}"#)?);

        // A splice index beyond the end clamps to an append.
        let splice = DeltaOp::from_json(&json(r#"["p",["xs"],1e30,0,[2]]"#)?)?;
        let result = apply(Some(json(r#"{"xs":[1]}"#)?), &[splice])?.ok_or("root")?;
        assert_eq!(result, json(r#"{"xs":[1,2]}"#)?);

        // A set index beyond one-past-the-end is still a sparse-write
        // rejection at application.
        assert!(
            apply(
                Some(json(r#"{"xs":[1]}"#)?),
                &[DeltaOp::from_json(&json(r#"["s",["xs",1e30],9]"#)?)?],
            )
            .is_err()
        );

        // Negative, fractional, and non-numeric counts stay rejected at parse.
        assert!(DeltaOp::from_json(&json(r#"["t",["s"],-1]"#)?).is_err());
        assert!(DeltaOp::from_json(&json(r#"["t",["s"],1.5]"#)?).is_err());
        assert!(DeltaOp::from_json(&json(r#"["t",["s"],null]"#)?).is_err());
        Ok(())
    }

    #[test]
    fn dictionary_ids_use_the_full_binary64_domain() -> TestResult {
        let mut decoder = DeltaDecoder::new();
        let define = WireOp::from_json(&json(r##"["#",1e30,["a"]]"##)?)?;
        let set = WireOp::from_json(&json(r#"["s",1e30,1]"#)?)?;
        let wire = vec![define, set];
        let ops = decoder.decode(&wire)?;
        assert_eq!(
            ops,
            vec![DeltaOp::Set(path(vec![PathSegment::key("a")])?, json("1")?)]
        );
        Ok(())
    }

    #[test]
    fn failed_batch_does_not_leak_its_dictionary_entries() -> TestResult {
        let mut decoder = DeltaDecoder::new();
        let good = vec![
            WireOp::from_json(&json(r##"["#",0,["a"]]"##)?)?,
            WireOp::from_json(&json(r#"["s",0,1]"#)?)?,
        ];
        assert_eq!(
            decoder.decode(&good)?,
            vec![DeltaOp::Set(path(vec![PathSegment::key("a")])?, json("1")?)]
        );

        let rejected = vec![
            WireOp::from_json(&json(r##"["#",1,["b"]]"##)?)?,
            WireOp::from_json(&json(r#"["s",9,1]"#)?)?,
        ];
        assert_eq!(
            decoder.decode(&rejected).expect_err("unresolved id"),
            DeltaError::PathId(int(9.0)?)
        );

        // The rejected batch's Define must not survive into later updates.
        let later = vec![WireOp::from_json(&json(r#"["s",1,1]"#)?)?];
        assert_eq!(
            decoder.decode(&later).expect_err("rejected id"),
            DeltaError::PathId(int(1.0)?)
        );

        // Previously accepted dictionary entries still resolve.
        let again = vec![WireOp::from_json(&json(r#"["s",0,2]"#)?)?];
        assert_eq!(
            decoder.decode(&again)?,
            vec![DeltaOp::Set(path(vec![PathSegment::key("a")])?, json("2")?)]
        );
        Ok(())
    }

    #[test]
    fn local_parsing_does_not_impose_remote_finite_or_depth_validation() -> TestResult {
        // Payload values are adopted as-is: the remote `is_json_value`
        // finite/depth admission check is a boundary concern, not a local
        // op-grammar rule.
        let op = DeltaOp::from_json(&JsonValue::Array(vec![
            JsonValue::String(js("r")),
            JsonValue::Number(f64::NAN),
        ]))?;
        assert!(matches!(op, DeltaOp::Replace(JsonValue::Number(number)) if number.is_nan()));

        let mut deep = JsonValue::Null;
        for _ in 0..600 {
            deep = JsonValue::Array(vec![deep]);
        }
        let op = DeltaOp::from_json(&JsonValue::Array(vec![JsonValue::String(js("r")), deep]))?;
        assert!(matches!(op, DeltaOp::Replace(_)));
        Ok(())
    }

    #[test]
    fn decoded_parsing_rejects_wire_short_forms() -> TestResult {
        // Validating Op against the wire grammar would be laxer than the
        // type: each vocabulary gets the parser that matches it.
        assert!(DeltaOp::from_json(&json(r#"["s",1]"#)?).is_err());
        assert!(DeltaOp::from_json(&json(r#"["d"]"#)?).is_err());
        assert!(DeltaOp::from_json(&json(r#"["a","x"]"#)?).is_err());
        assert!(DeltaOp::from_json(&json(r#"["t",2]"#)?).is_err());
        assert!(DeltaOp::from_json(&json(r#"["p",0,0,[]]"#)?).is_err());
        assert!(DeltaOp::from_json(&json(r##"["#",0,["a"]]"##)?).is_err());
        Ok(())
    }

    #[test]
    fn wire_parsing_accepts_short_and_dictionary_forms() -> TestResult {
        for text in [
            r#"["r",{"a":1}]"#,
            r#"["s",["a"],1]"#,
            r#"["s",0,1]"#,
            r#"["s",1]"#,
            r#"["d",["a"]]"#,
            r#"["d",0]"#,
            r#"["d"]"#,
            r#"["a",["a"],"x"]"#,
            r#"["a",0,"x"]"#,
            r#"["a","x"]"#,
            r#"["t",["a"],2]"#,
            r#"["t",0,2]"#,
            r#"["t",2]"#,
            r#"["p",["a"],0,0,[]]"#,
            r#"["p",0,0,0,[]]"#,
            r#"["p",0,0,[]]"#,
            r##"["#",0,["a"]]"##,
        ] {
            WireOp::from_json(&json(text)?)?;
        }
        Ok(())
    }

    #[test]
    fn wire_tuples_round_trip_through_the_canonical_tree() -> TestResult {
        let p = path(vec![PathSegment::key("k"), PathSegment::index(int(3.0)?)])?;
        let wire = vec![
            WireOp::Replace(json(r#"{"a":1}"#)?),
            WireOp::Define(int(0.0)?, p.clone()),
            WireOp::Set(PathRef::Id(int(0.0)?), json("2")?),
            WireOp::SetShort(json("3")?),
            WireOp::Delete(PathRef::Inline(p.clone())),
            WireOp::DeleteShort,
            WireOp::Append(PathRef::Inline(p.clone()), js("x")),
            WireOp::AppendShort(js("y")),
            WireOp::Truncate(PathRef::Inline(p.clone()), int(2.0)?),
            WireOp::TruncateShort(int(1.0)?),
            WireOp::Splice(
                PathRef::Inline(p.clone()),
                int(0.0)?,
                int(1.0)?,
                vec![json("9")?],
            ),
            WireOp::SpliceShort(int(0.0)?, int(0.0)?, vec![json("8")?]),
        ];
        let encoded = wire_json(&wire);
        let JsonValue::Array(items) = &encoded else {
            return Err("expected array".into());
        };
        let decoded = items
            .iter()
            .map(WireOp::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(decoded, wire);
        Ok(())
    }

    #[test]
    fn splice_at_the_root_path_replaces_array_contents() -> TestResult {
        let splice = DeltaOp::from_json(&json(r#"["p",[],1,1,[9]]"#)?)?;
        let result = apply(Some(json("[1,2,3]")?), &[splice])?.ok_or("root")?;
        assert_eq!(result, json("[1,9,3]")?);
        Ok(())
    }

    #[test]
    fn tracker_coalesces_string_and_array_changes() -> TestResult {
        let mut tracker = DeltaTracker::new(json(r#"{"text":"","xs":[1]}"#)?);
        assert_eq!(
            tracker.flush()?,
            vec![DeltaOp::Replace(json(r#"{"text":"","xs":[1]}"#)?)]
        );
        let text_path = path(vec![PathSegment::key("text")])?;
        tracker.append_string(&text_path, "ab")?;
        tracker.append_string(&text_path, "cd")?;
        let xs_path = path(vec![PathSegment::key("xs")])?;
        tracker.push(&xs_path, vec![json("2")?, json("3")?])?;
        let ops = tracker.flush()?;
        assert_eq!(
            ops,
            vec![
                DeltaOp::Append(text_path, js("abcd")),
                DeltaOp::Splice(xs_path, int(1.0)?, int(0.0)?, vec![json("2")?, json("3")?],),
            ]
        );
        assert!(!tracker.dirty());
        Ok(())
    }

    #[test]
    fn reserved_paths_and_array_gaps_are_rejected() -> TestResult {
        let unsafe_path = StatePath::from(vec![PathSegment::key("constructor")]);
        assert!(DeltaOp::Delete(unsafe_path).validate().is_err());
        let gap = DeltaOp::Set(
            path(vec![PathSegment::key("xs"), PathSegment::index(int(5.0)?)])?,
            json("1")?,
        );
        assert!(apply(Some(json(r#"{"xs":[1]}"#)?), &[gap]).is_err());
        Ok(())
    }

    #[test]
    fn immutable_apply_does_not_mutate_previous_or_replacement_payload() -> TestResult {
        let previous = json(r#"{"nested":{"value":1},"untouched":[1]}"#)?;
        let replacement = json(r#"{"nested":{"value":2}}"#)?;
        let next = apply_immutable(
            Some(&previous),
            &[
                DeltaOp::Replace(replacement.clone()),
                DeltaOp::Set(
                    path(vec![PathSegment::key("nested"), PathSegment::key("value")])?,
                    json("3")?,
                ),
            ],
        )?
        .ok_or("root")?;
        assert_eq!(previous, json(r#"{"nested":{"value":1},"untouched":[1]}"#)?);
        assert_eq!(replacement, json(r#"{"nested":{"value":2}}"#)?);
        assert_eq!(next, json(r#"{"nested":{"value":3}}"#)?);
        Ok(())
    }
}
