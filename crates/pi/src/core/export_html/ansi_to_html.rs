//! ANSI SGR to safe inline HTML conversion used by session export.

const ANSI_COLORS: [&str; 16] = [
    "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
    "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
];

#[derive(Default)]
struct TextModifiers(u8);

impl TextModifiers {
    const BOLD: u8 = 1 << 0;
    const DIM: u8 = 1 << 1;
    const ITALIC: u8 = 1 << 2;
    const UNDERLINE: u8 = 1 << 3;

    fn contains(&self, modifier: u8) -> bool {
        self.0 & modifier != 0
    }

    fn insert(&mut self, modifier: u8) {
        self.0 |= modifier;
    }

    fn remove(&mut self, modifier: u8) {
        self.0 &= !modifier;
    }

    fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

#[derive(Default)]
struct TextStyle {
    foreground: Option<String>,
    background: Option<String>,
    modifiers: TextModifiers,
}

impl TextStyle {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn has_style(&self) -> bool {
        self.foreground.is_some() || self.background.is_some() || !self.modifiers.is_empty()
    }

    fn inline_css(&self) -> String {
        let mut parts = Vec::with_capacity(6);
        if let Some(color) = &self.foreground {
            parts.push(format!("color:{color}"));
        }
        if let Some(color) = &self.background {
            parts.push(format!("background-color:{color}"));
        }
        if self.modifiers.contains(TextModifiers::BOLD) {
            parts.push("font-weight:bold".to_owned());
        }
        if self.modifiers.contains(TextModifiers::DIM) {
            parts.push("opacity:0.6".to_owned());
        }
        if self.modifiers.contains(TextModifiers::ITALIC) {
            parts.push("font-style:italic".to_owned());
        }
        if self.modifiers.contains(TextModifiers::UNDERLINE) {
            parts.push("text-decoration:underline".to_owned());
        }
        parts.join(";")
    }
}

fn escape_html(text: &str, output: &mut String) {
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#039;"),
            _ => output.push(character),
        }
    }
}

fn color_256(index: u16) -> String {
    let index = index.min(255);
    if index < 16 {
        return ANSI_COLORS[usize::from(index)].to_owned();
    }
    if index < 232 {
        let cube = index - 16;
        let component = |value: u16| if value == 0 { 0 } else { 55 + value * 40 };
        return format!(
            "#{:02x}{:02x}{:02x}",
            component(cube / 36),
            component((cube % 36) / 6),
            component(cube % 6)
        );
    }
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

fn apply_sgr(parameters: &[u16], style: &mut TextStyle) {
    let mut index = 0;
    while index < parameters.len() {
        let code = parameters[index];
        match code {
            0 => style.reset(),
            1 => style.modifiers.insert(TextModifiers::BOLD),
            2 => style.modifiers.insert(TextModifiers::DIM),
            3 => style.modifiers.insert(TextModifiers::ITALIC),
            4 => style.modifiers.insert(TextModifiers::UNDERLINE),
            22 => style
                .modifiers
                .remove(TextModifiers::BOLD | TextModifiers::DIM),
            23 => style.modifiers.remove(TextModifiers::ITALIC),
            24 => style.modifiers.remove(TextModifiers::UNDERLINE),
            30..=37 => style.foreground = Some(ANSI_COLORS[usize::from(code - 30)].to_owned()),
            38 | 48 => {
                let color = match parameters.get(index + 1) {
                    Some(5) => parameters
                        .get(index + 2)
                        .map(|value| (color_256(*value), 2)),
                    Some(2) if parameters.len() > index + 4 => Some((
                        format!(
                            "rgb({},{},{})",
                            parameters[index + 2],
                            parameters[index + 3],
                            parameters[index + 4]
                        ),
                        4,
                    )),
                    _ => None,
                };
                if let Some((color, consumed)) = color {
                    if code == 38 {
                        style.foreground = Some(color);
                    } else {
                        style.background = Some(color);
                    }
                    index += consumed;
                }
            }
            39 => style.foreground = None,
            40..=47 => style.background = Some(ANSI_COLORS[usize::from(code - 40)].to_owned()),
            49 => style.background = None,
            90..=97 => {
                style.foreground = Some(ANSI_COLORS[usize::from(code - 90 + 8)].to_owned());
            }
            100..=107 => {
                style.background = Some(ANSI_COLORS[usize::from(code - 100 + 8)].to_owned());
            }
            _ => {}
        }
        index += 1;
    }
}

fn sgr_at(text: &str, start: usize) -> Option<(usize, Vec<u16>)> {
    let bytes = text.as_bytes();
    if bytes.get(start..start.saturating_add(2)) != Some(b"\x1b[") {
        return None;
    }
    let mut end = start + 2;
    while let Some(byte) = bytes.get(end) {
        if *byte == b'm' {
            let raw = &text[start + 2..end];
            if !raw
                .bytes()
                .all(|value| value.is_ascii_digit() || value == b';')
            {
                return None;
            }
            let parameters = if raw.is_empty() {
                vec![0]
            } else {
                raw.split(';')
                    .map(|part| part.parse::<u16>().unwrap_or(0))
                    .collect()
            };
            return Some((end + 1, parameters));
        }
        if !byte.is_ascii_digit() && *byte != b';' {
            return None;
        }
        end += 1;
    }
    None
}

/// Convert ANSI SGR escapes to safe HTML spans with inline styles.
#[must_use]
pub fn ansi_to_html(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut style = TextStyle::default();
    let mut cursor = 0;
    let mut span_open = false;

    while let Some(relative) = text[cursor..].find("\x1b[") {
        let start = cursor + relative;
        let Some((end, parameters)) = sgr_at(text, start) else {
            escape_html(&text[cursor..=start], &mut output);
            cursor = start + 1;
            continue;
        };
        escape_html(&text[cursor..start], &mut output);
        if span_open {
            output.push_str("</span>");
            span_open = false;
        }
        apply_sgr(&parameters, &mut style);
        if style.has_style() {
            output.push_str("<span style=\"");
            output.push_str(&style.inline_css());
            output.push_str("\">");
            span_open = true;
        }
        cursor = end;
    }

    escape_html(&text[cursor..], &mut output);
    if span_open {
        output.push_str("</span>");
    }
    output
}

/// Convert ANSI lines to adjacent `ansi-line` divs, preserving empty lines.
#[must_use]
pub fn ansi_lines_to_html(lines: &[String]) -> String {
    let mut output = String::new();
    for line in lines {
        output.push_str("<div class=\"ansi-line\">");
        let converted = ansi_to_html(line);
        if converted.is_empty() {
            output.push_str("&nbsp;");
        } else {
            output.push_str(&converted);
        }
        output.push_str("</div>");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{ansi_lines_to_html, ansi_to_html};

    #[test]
    fn escapes_plain_html_and_quotes() {
        assert_eq!(ansi_to_html("<&\"'>"), "&lt;&amp;&quot;&#039;&gt;");
    }

    #[test]
    fn renders_standard_extended_and_reset_styles() {
        assert_eq!(
            ansi_to_html("\x1b[1;31mred\x1b[22;38;5;214morange\x1b[0m!"),
            concat!(
                "<span style=\"color:#800000;font-weight:bold\">red</span>",
                "<span style=\"color:#ffaf00\">orange</span>!"
            )
        );
        assert_eq!(
            ansi_to_html("\x1b[48;2;1;2;3m x "),
            "<span style=\"background-color:rgb(1,2,3)\"> x </span>"
        );
    }

    #[test]
    fn wraps_lines_and_preserves_empty_lines() {
        assert_eq!(
            ansi_lines_to_html(&[String::new(), "x".to_owned()]),
            "<div class=\"ansi-line\">&nbsp;</div><div class=\"ansi-line\">x</div>"
        );
    }
}
