//! Overlay sizing and positioning primitives.
//!
//! Ports `SizeValue`, `OverlayAnchor`, `OverlayMargin`, `OverlayOptions` layout
//! fields, and `resolveOverlayLayout` from
//! `.references/pi/packages/tui/src/tui.ts`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Absolute cell count or percentage of a reference dimension.
///
/// Wire form matches TypeScript: a JSON number for cells, or a string `"N%"`
/// for percent (integer percent in `[0, 100]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeValue {
    /// Absolute size in terminal cells.
    Cells(u16),
    /// Percentage of the reference size (`0..=100`).
    Percent(u8),
}

impl SizeValue {
    /// Absolute cells.
    #[must_use]
    pub const fn cells(n: u16) -> Self {
        Self::Cells(n)
    }

    /// Percentage of the reference size, clamped to `0..=100`.
    #[must_use]
    pub const fn percent(n: u8) -> Self {
        Self::Percent(if n > 100 { 100 } else { n })
    }

    /// Resolve to an absolute cell count against `reference`.
    ///
    /// Percent uses floor division: `floor(reference * n / 100)`.
    #[must_use]
    pub fn resolve(self, reference: u16) -> u16 {
        match self {
            Self::Cells(n) => n,
            Self::Percent(n) => {
                let percentage = u32::from(n.min(100));
                u16::try_from(u32::from(reference) * percentage / 100).unwrap_or(u16::MAX)
            }
        }
    }

    /// Resolve an optional size value.
    #[must_use]
    pub fn resolve_opt(value: Option<Self>, reference: u16) -> Option<u16> {
        value.map(|v| v.resolve(reference))
    }
}

impl Serialize for SizeValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match *self {
            Self::Cells(n) => serializer.serialize_u16(n),
            Self::Percent(n) => serializer.serialize_str(&format!("{n}%")),
        }
    }
}

impl<'de> Deserialize<'de> for SizeValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = SizeValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a cell count number or a percent string like \"50%\"")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u16::try_from(v)
                    .map(SizeValue::Cells)
                    .map_err(|_| E::custom("size value exceeds u16"))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom("size value must be non-negative"));
                }
                self.visit_u64(v.cast_unsigned())
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_percent_str(v)
                    .map(SizeValue::Percent)
                    .ok_or_else(|| E::custom(format!("invalid percent size: {v}")))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

fn parse_percent_str(value: &str) -> Option<u8> {
    let stripped = value.strip_suffix('%')?;
    if stripped.is_empty() || !stripped.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = stripped.parse().ok()?;
    u8::try_from(n.min(100)).ok()
}

/// Anchor point for overlay placement (default: center).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlayAnchor {
    /// Center of the available area.
    #[default]
    Center,
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
    /// Top edge, horizontally centered.
    TopCenter,
    /// Bottom edge, horizontally centered.
    BottomCenter,
    /// Left edge, vertically centered.
    LeftCenter,
    /// Right edge, vertically centered.
    RightCenter,
}

/// Per-side overlay margin from terminal edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayMargin {
    /// Top margin in rows.
    pub top: u16,
    /// Right margin in columns.
    pub right: u16,
    /// Bottom margin in rows.
    pub bottom: u16,
    /// Left margin in columns.
    pub left: u16,
}

impl OverlayMargin {
    /// Zero margin on every side.
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            top: 0,
            right: 0,
            bottom: 0,
            left: 0,
        }
    }

    /// Same margin on every side.
    #[must_use]
    pub const fn uniform(n: u16) -> Self {
        Self {
            top: n,
            right: n,
            bottom: n,
            left: n,
        }
    }

    /// Clamp each side to non-negative (identity for `u16`).
    #[must_use]
    pub const fn clamped(self) -> Self {
        self
    }
}

impl<'de> Deserialize<'de> for OverlayMargin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OverlayMargin;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a margin number or an object with optional top/right/bottom/left sides",
                )
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u16::try_from(v)
                    .map(OverlayMargin::uniform)
                    .map_err(|_| E::custom("margin value exceeds u16"))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom("margin must be non-negative"));
                }
                self.visit_u64(v.cast_unsigned())
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut top = 0u16;
                let mut right = 0u16;
                let mut bottom = 0u16;
                let mut left = 0u16;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "top" => top = map.next_value()?,
                        "right" => right = map.next_value()?,
                        "bottom" => bottom = map.next_value()?,
                        "left" => left = map.next_value()?,
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(OverlayMargin {
                    top,
                    right,
                    bottom,
                    left,
                })
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Serializable overlay layout specification (Phase 6 `uiSlot` value type).
///
/// Field names are camelCase on the wire. The host-side `visible` callback is
/// not part of this type; hosts decide visibility before sending a slot.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OverlaySpec {
    /// Width in columns, or percentage of terminal width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<SizeValue>,
    /// Minimum width in columns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_width: Option<u16>,
    /// Maximum height in rows, or percentage of terminal height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<SizeValue>,
    /// Anchor point when `row`/`col` are unset (default center).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<OverlayAnchor>,
    /// Horizontal offset from the resolved position (positive = right).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_x: Option<i16>,
    /// Vertical offset from the resolved position (positive = down).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_y: Option<i16>,
    /// Absolute or percent row position (overrides vertical anchor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<SizeValue>,
    /// Absolute or percent column position (overrides horizontal anchor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<SizeValue>,
    /// Margin from terminal edges.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin: Option<OverlayMargin>,
    /// When true, showing the overlay does not capture keyboard focus.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub non_capturing: bool,
}

/// Resolved overlay rectangle and optional height clamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedOverlayLayout {
    /// Overlay width in columns.
    pub width: u16,
    /// Top row (0-based, within the terminal).
    pub row: u16,
    /// Left column (0-based, within the terminal).
    pub col: u16,
    /// Optional max height clamp applied to measured content.
    pub max_height: Option<u16>,
}

/// Resolve overlay layout from a serializable spec.
///
/// Ports `TUI.resolveOverlayLayout` exactly:
/// - default width = `min(80, avail_width)`
/// - margins clamped non-negative and subtracted from available space
/// - percent sizes floor-resolve against full terminal dimensions
/// - anchor math for the nine anchors
/// - final position clamped into the margin-respecting bounds
#[must_use]
pub fn resolve_overlay_layout(
    spec: &OverlaySpec,
    overlay_height: u16,
    term_width: u16,
    term_height: u16,
) -> ResolvedOverlayLayout {
    let margin = spec.margin.unwrap_or_default().clamped();
    let margin_top = margin.top;
    let margin_right = margin.right;
    let margin_bottom = margin.bottom;
    let margin_left = margin.left;

    let avail_width = term_width
        .saturating_sub(margin_left)
        .saturating_sub(margin_right)
        .max(1);
    let avail_height = term_height
        .saturating_sub(margin_top)
        .saturating_sub(margin_bottom)
        .max(1);

    // Width: default min(80, avail), then minWidth, then clamp to avail.
    let mut width = SizeValue::resolve_opt(spec.width, term_width).unwrap_or(80.min(avail_width));
    if let Some(min_width) = spec.min_width {
        width = width.max(min_width);
    }
    width = width.clamp(1, avail_width);

    // maxHeight: optional, clamped to avail.
    let mut max_height = SizeValue::resolve_opt(spec.max_height, term_height);
    if let Some(mh) = max_height.as_mut() {
        *mh = (*mh).clamp(1, avail_height);
    }

    let effective_height = match max_height {
        Some(mh) => overlay_height.min(mh),
        None => overlay_height,
    };

    let anchor = spec.anchor.unwrap_or_default();

    let mut row = match spec.row {
        Some(SizeValue::Percent(p)) => {
            margin_top + resolve_percentage(avail_height.saturating_sub(effective_height), p)
        }
        Some(SizeValue::Cells(r)) => r,
        None => resolve_anchor_row(anchor, effective_height, avail_height, margin_top),
    };

    let mut col = match spec.col {
        Some(SizeValue::Percent(p)) => {
            margin_left + resolve_percentage(avail_width.saturating_sub(width), p)
        }
        Some(SizeValue::Cells(c)) => c,
        None => resolve_anchor_col(anchor, width, avail_width, margin_left),
    };

    if let Some(offset_y) = spec.offset_y {
        row = add_i16(row, offset_y);
    }
    if let Some(offset_x) = spec.offset_x {
        col = add_i16(col, offset_x);
    }

    // Clamp into margin-respecting bounds.
    let max_row = term_height
        .saturating_sub(margin_bottom)
        .saturating_sub(effective_height);
    let max_col = term_width
        .saturating_sub(margin_right)
        .saturating_sub(width);
    row = row.clamp(margin_top, max_row.max(margin_top));
    col = col.clamp(margin_left, max_col.max(margin_left));

    ResolvedOverlayLayout {
        width,
        row,
        col,
        max_height,
    }
}

fn resolve_anchor_row(
    anchor: OverlayAnchor,
    height: u16,
    avail_height: u16,
    margin_top: u16,
) -> u16 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => margin_top,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomCenter | OverlayAnchor::BottomRight => {
            margin_top + avail_height.saturating_sub(height)
        }
        OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
            margin_top + avail_height.saturating_sub(height) / 2
        }
    }
}

fn resolve_anchor_col(
    anchor: OverlayAnchor,
    width: u16,
    avail_width: u16,
    margin_left: u16,
) -> u16 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => {
            margin_left
        }
        OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
            margin_left + avail_width.saturating_sub(width)
        }
        OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
            margin_left + avail_width.saturating_sub(width) / 2
        }
    }
}

fn resolve_percentage(reference: u16, percentage: u8) -> u16 {
    u16::try_from(u32::from(reference) * u32::from(percentage.min(100)) / 100).unwrap_or(u16::MAX)
}

fn add_i16(base: u16, delta: i16) -> u16 {
    let value = i32::from(base) + i32::from(delta);
    u16::try_from(value.clamp(0, i32::from(u16::MAX))).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_value_percent_floor() {
        assert_eq!(SizeValue::percent(50).resolve(81), 40);
        assert_eq!(SizeValue::percent(100).resolve(80), 80);
        assert_eq!(SizeValue::percent(0).resolve(80), 0);
        assert_eq!(SizeValue::cells(12).resolve(999), 12);
    }

    #[test]
    fn size_value_serde_number_and_percent() -> Result<(), serde_json::Error> {
        let cells: SizeValue = serde_json::from_str("42")?;
        assert_eq!(cells, SizeValue::Cells(42));
        let pct: SizeValue = serde_json::from_str("\"50%\"")?;
        assert_eq!(pct, SizeValue::Percent(50));
        assert_eq!(serde_json::to_string(&SizeValue::Cells(7))?, "7");
        assert_eq!(serde_json::to_string(&SizeValue::Percent(25))?, "\"25%\"");
        Ok(())
    }

    #[test]
    fn overlay_spec_camel_case_roundtrip() -> Result<(), serde_json::Error> {
        let json = r#"{
            "width": "50%",
            "minWidth": 20,
            "maxHeight": 10,
            "anchor": "top-right",
            "offsetX": 1,
            "offsetY": -2,
            "row": 3,
            "col": "25%",
            "margin": {"top": 1, "right": 2, "bottom": 3, "left": 4},
            "nonCapturing": true
        }"#;
        let spec: OverlaySpec = serde_json::from_str(json)?;
        assert_eq!(spec.width, Some(SizeValue::Percent(50)));
        assert_eq!(spec.min_width, Some(20));
        assert_eq!(spec.max_height, Some(SizeValue::Cells(10)));
        assert_eq!(spec.anchor, Some(OverlayAnchor::TopRight));
        assert_eq!(spec.offset_x, Some(1));
        assert_eq!(spec.offset_y, Some(-2));
        assert_eq!(spec.row, Some(SizeValue::Cells(3)));
        assert_eq!(spec.col, Some(SizeValue::Percent(25)));
        assert_eq!(
            spec.margin,
            Some(OverlayMargin {
                top: 1,
                right: 2,
                bottom: 3,
                left: 4
            })
        );
        assert!(spec.non_capturing);
        let out = serde_json::to_value(&spec)?;
        assert_eq!(out["minWidth"], 20);
        assert_eq!(out["nonCapturing"], true);
        assert_eq!(out["anchor"], "top-right");
        Ok(())
    }

    #[test]
    fn overlay_margin_deserializes_bare_scalar_as_uniform() -> Result<(), serde_json::Error> {
        let m: OverlayMargin = serde_json::from_str("3")?;
        assert_eq!(m, OverlayMargin::uniform(3));
        let m2: OverlayMargin = serde_json::from_value(serde_json::Value::Number(7.into()))?;
        assert_eq!(m2, OverlayMargin::uniform(7));
        Ok(())
    }

    #[test]
    fn overlay_margin_deserializes_partial_object_defaults_zero() -> Result<(), serde_json::Error> {
        let m: OverlayMargin = serde_json::from_str(r#"{"top":1}"#)?;
        assert_eq!(
            m,
            OverlayMargin {
                top: 1,
                right: 0,
                bottom: 0,
                left: 0
            }
        );
        Ok(())
    }

    #[test]
    fn overlay_margin_object_roundtrip_keeps_shape() -> Result<(), serde_json::Error> {
        let m = OverlayMargin {
            top: 1,
            right: 2,
            bottom: 3,
            left: 4,
        };
        let out = serde_json::to_value(m)?;
        assert_eq!(out["top"], 1);
        assert_eq!(out["right"], 2);
        assert_eq!(out["bottom"], 3);
        assert_eq!(out["left"], 4);
        let back: OverlayMargin = serde_json::from_value(out)?;
        assert_eq!(back, m);
        Ok(())
    }

    #[test]
    fn default_width_min_80_avail() {
        let layout = resolve_overlay_layout(&OverlaySpec::default(), 3, 100, 40);
        assert_eq!(layout.width, 80);
        // center: row = (40-3)/2 = 18, col = (100-80)/2 = 10
        assert_eq!(layout.row, 18);
        assert_eq!(layout.col, 10);
    }

    #[test]
    fn default_width_clamps_to_narrow_terminal() {
        let layout = resolve_overlay_layout(&OverlaySpec::default(), 1, 50, 20);
        assert_eq!(layout.width, 50);
        assert_eq!(layout.col, 0);
    }

    #[test]
    fn min_width_and_percent_width() {
        let spec = OverlaySpec {
            width: Some(SizeValue::percent(10)),
            min_width: Some(30),
            ..OverlaySpec::default()
        };
        // 10% of 100 = 10, minWidth 30 → 30
        let layout = resolve_overlay_layout(&spec, 2, 100, 40);
        assert_eq!(layout.width, 30);
    }

    #[test]
    fn margin_reduces_available_and_clamps() {
        let spec = OverlaySpec {
            width: Some(SizeValue::cells(100)),
            margin: Some(OverlayMargin::uniform(5)),
            anchor: Some(OverlayAnchor::TopLeft),
            ..OverlaySpec::default()
        };
        // avail = 80-10 = 70 → width clamped to 70, at margin 5,5
        let layout = resolve_overlay_layout(&spec, 4, 80, 24);
        assert_eq!(layout.width, 70);
        assert_eq!(layout.row, 5);
        assert_eq!(layout.col, 5);
    }

    #[test]
    fn anchors_table() {
        let cases = [
            (OverlayAnchor::TopLeft, 0, 0),
            (OverlayAnchor::TopCenter, 0, 35), // (80-10)/2
            (OverlayAnchor::TopRight, 0, 70),
            (OverlayAnchor::BottomLeft, 17, 0), // 20-3
            (OverlayAnchor::BottomCenter, 17, 35),
            (OverlayAnchor::BottomRight, 17, 70),
            (OverlayAnchor::LeftCenter, 8, 0), // (20-3)/2
            (OverlayAnchor::Center, 8, 35),
            (OverlayAnchor::RightCenter, 8, 70),
        ];
        for (anchor, row, col) in cases {
            let spec = OverlaySpec {
                width: Some(SizeValue::cells(10)),
                anchor: Some(anchor),
                ..OverlaySpec::default()
            };
            let layout = resolve_overlay_layout(&spec, 3, 80, 20);
            assert_eq!(layout.row, row, "row for {anchor:?}");
            assert_eq!(layout.col, col, "col for {anchor:?}");
            assert_eq!(layout.width, 10);
        }
    }

    #[test]
    fn percent_row_col_within_bounds() {
        let spec = OverlaySpec {
            width: Some(SizeValue::cells(10)),
            row: Some(SizeValue::percent(100)),
            col: Some(SizeValue::percent(100)),
            ..OverlaySpec::default()
        };
        // maxRow = 20-3 = 17, maxCol = 80-10 = 70
        let layout = resolve_overlay_layout(&spec, 3, 80, 20);
        assert_eq!(layout.row, 17);
        assert_eq!(layout.col, 70);
    }

    #[test]
    fn percent_zero_row_col_at_origin() {
        let spec = OverlaySpec {
            width: Some(SizeValue::cells(4)),
            row: Some(SizeValue::percent(0)),
            col: Some(SizeValue::percent(0)),
            ..OverlaySpec::default()
        };
        let layout = resolve_overlay_layout(&spec, 2, 40, 10);
        assert_eq!(layout.row, 0);
        assert_eq!(layout.col, 0);
    }

    #[test]
    fn offsets_then_clamp() {
        let spec = OverlaySpec {
            width: Some(SizeValue::cells(10)),
            anchor: Some(OverlayAnchor::TopLeft),
            offset_x: Some(-5),
            offset_y: Some(100),
            ..OverlaySpec::default()
        };
        let layout = resolve_overlay_layout(&spec, 3, 80, 20);
        // row 0+100 → clamp to 20-3 = 17; col 0-5 → 0
        assert_eq!(layout.row, 17);
        assert_eq!(layout.col, 0);
    }

    #[test]
    fn max_height_clamped_to_avail() {
        let spec = OverlaySpec {
            max_height: Some(SizeValue::percent(200)),
            margin: Some(OverlayMargin::uniform(2)),
            ..OverlaySpec::default()
        };
        // 200% of 24 = 48, clamp to availHeight = 20
        let layout = resolve_overlay_layout(&spec, 50, 80, 24);
        assert_eq!(layout.max_height, Some(20));
    }

    #[test]
    fn absolute_row_col_respected_then_clamped() {
        let spec = OverlaySpec {
            width: Some(SizeValue::cells(10)),
            row: Some(SizeValue::cells(5)),
            col: Some(SizeValue::cells(5)),
            ..OverlaySpec::default()
        };
        let layout = resolve_overlay_layout(&spec, 3, 80, 20);
        assert_eq!(layout.row, 5);
        assert_eq!(layout.col, 5);
    }
}
