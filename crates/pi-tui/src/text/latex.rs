//! Terminal LaTeX math rendering: a Rust-native port of the upstream
//! `.references/pi/packages/tui/src/latex.ts` layout engine (T4,
//! `docs/PAR-MATH-latex-strategy.md`).
//!
//! Entry-exact command tables (same names, same mappings, same entry counts,
//! asserted in `tests`) plus the same parser invariants: a render succeeds
//! only when the parser consumed the entire source and never flagged
//! unsupported syntax; failed nested renders propagate. Display mode stacks
//! fractions and operator limits vertically through PUA layout markers
//! (U+F0000..U+F0005) that never leak into output; unsupported input returns
//! `None` and callers fall back to the raw source.

use super::width::visible_width;

// PUA layout protocol (never present in returned output).
const LAYOUT_MARKER_START: char = '\u{f0000}';
const LAYOUT_MARKER_END: char = '\u{f0001}';
const PROTECTED_SPACE: char = '\u{f0002}';
/// Internal sentinel for negative-spacing commands; consumed by the parser,
/// never appended to output.
const NEGATIVE_SPACE_SENTINEL: char = '\u{f0003}';
const NAMED_OPERATOR_START: char = '\u{f0004}';
const NAMED_OPERATOR_END: char = '\u{f0005}';

// ---------------------------------------------------------------------------
// Command tables (entry-exact port of latex.ts:3-585)
// ---------------------------------------------------------------------------

static SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ϵ"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("varkappa", "ϰ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("pi", "π"),
    ("varpi", "ϖ"),
    ("rho", "ρ"),
    ("varrho", "ϱ"),
    ("sigma", "σ"),
    ("varsigma", "ς"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "ϕ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Upsilon", "Υ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
    ("pm", "±"),
    ("mp", "∓"),
    ("times", "×"),
    ("div", "÷"),
    ("cdot", "·"),
    ("ast", "∗"),
    ("star", "⋆"),
    ("circ", "∘"),
    ("bullet", "•"),
    ("oplus", "⊕"),
    ("ominus", "⊖"),
    ("otimes", "⊗"),
    ("oslash", "⊘"),
    ("odot", "⊙"),
    ("bigcirc", "○"),
    ("dagger", "†"),
    ("ddagger", "‡"),
    ("amalg", "⨿"),
    ("uplus", "⊎"),
    ("sqcap", "⊓"),
    ("sqcup", "⊔"),
    ("triangleleft", "◁"),
    ("triangleright", "▷"),
    ("wr", "≀"),
    ("cap", "∩"),
    ("cup", "∪"),
    ("bigcap", "⋂"),
    ("bigcup", "⋃"),
    ("bigwedge", "⋀"),
    ("bigvee", "⋁"),
    ("bigsqcup", "⨆"),
    ("biguplus", "⨄"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigodot", "⨀"),
    ("setminus", "∖"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("subset", "⊂"),
    ("supset", "⊃"),
    ("subseteq", "⊆"),
    ("supseteq", "⊇"),
    ("sqsubset", "⊏"),
    ("sqsupset", "⊐"),
    ("sqsubseteq", "⊑"),
    ("sqsupseteq", "⊒"),
    ("prec", "≺"),
    ("preceq", "≼"),
    ("succ", "≻"),
    ("succeq", "≽"),
    ("ll", "≪"),
    ("gg", "≫"),
    ("le", "≤"),
    ("leq", "≤"),
    ("leqslant", "≤"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("geqslant", "≥"),
    ("ne", "≠"),
    ("neq", "≠"),
    ("equiv", "≡"),
    ("approx", "≈"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("cong", "≅"),
    ("asymp", "≍"),
    ("doteq", "≐"),
    ("propto", "∝"),
    ("parallel", "∥"),
    ("perp", "⊥"),
    ("mid", "∣"),
    ("vdash", "⊢"),
    ("dashv", "⊣"),
    ("models", "⊨"),
    ("Vdash", "⊩"),
    ("Vvdash", "⊪"),
    ("nvdash", "⊬"),
    ("nvDash", "⊭"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("neg", "¬"),
    ("land", "∧"),
    ("wedge", "∧"),
    ("lor", "∨"),
    ("vee", "∨"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("longrightarrow", "→"),
    ("leftarrow", "←"),
    ("longleftarrow", "←"),
    ("gets", "←"),
    ("leftrightarrow", "↔"),
    ("longleftrightarrow", "↔"),
    ("hookleftarrow", "↩"),
    ("hookrightarrow", "↪"),
    ("twoheadleftarrow", "↞"),
    ("twoheadrightarrow", "↠"),
    ("leftharpoonup", "↼"),
    ("leftharpoondown", "↽"),
    ("rightharpoonup", "⇀"),
    ("rightharpoondown", "⇁"),
    ("rightleftharpoons", "⇌"),
    ("leftrightharpoons", "⇋"),
    ("nearrow", "↗"),
    ("searrow", "↘"),
    ("swarrow", "↙"),
    ("nwarrow", "↖"),
    ("rightsquigarrow", "⇝"),
    ("leadsto", "⇝"),
    ("Rightarrow", "⇒"),
    ("Longrightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("Longleftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("Longleftrightarrow", "⇔"),
    ("implies", "⇒"),
    ("iff", "⇔"),
    ("mapsto", "↦"),
    ("longmapsto", "↦"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("int", "∫"),
    ("iint", "∬"),
    ("iiint", "∭"),
    ("oint", "∮"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("coprod", "∐"),
    ("infty", "∞"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("angle", "∠"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("aleph", "ℵ"),
    ("beth", "ℶ"),
    ("gimel", "ℷ"),
    ("daleth", "ℸ"),
    ("top", "⊤"),
    ("bot", "⊥"),
    ("triangle", "△"),
    ("square", "□"),
    ("lozenge", "◊"),
    ("checkmark", "✓"),
    ("complement", "∁"),
    ("wp", "℘"),
    ("prime", "′"),
    ("ldots", "…"),
    ("dots", "…"),
    ("cdots", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("ell", "ℓ"),
    ("hbar", "ℏ"),
    ("Im", "ℑ"),
    ("Re", "ℜ"),
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("vert", "|"),
    ("lvert", "|"),
    ("rvert", "|"),
    ("Vert", "‖"),
    ("lVert", "‖"),
    ("rVert", "‖"),
    ("lbrace", "{"),
    ("rbrace", "}"),
    ("backslash", "\\"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("colon", ":"),
];

static NEGATED_SYMBOLS: &[(&str, &str)] = &[
    ("<", "≮"),
    (">", "≯"),
    ("=", "≠"),
    ("∈", "∉"),
    ("∋", "∌"),
    ("∣", "∤"),
    ("∥", "∦"),
    ("∼", "≁"),
    ("≃", "≄"),
    ("≅", "≇"),
    ("≈", "≉"),
    ("≡", "≢"),
    ("≤", "≰"),
    ("≥", "≱"),
    ("≺", "⊀"),
    ("≻", "⊁"),
    ("⊂", "⊄"),
    ("⊃", "⊅"),
    ("⊆", "⊈"),
    ("⊇", "⊉"),
    ("⊢", "⊬"),
    ("⊨", "⊭"),
    ("↔", "↮"),
    ("←", "↚"),
    ("→", "↛"),
    ("⇒", "⇏"),
    ("⇐", "⇍"),
    ("⇔", "⇎"),
    ("≼", "⋠"),
    ("≽", "⋡"),
];

static BLACKBOARD: &[(&str, &str)] = &[
    ("C", "ℂ"),
    ("H", "ℍ"),
    ("N", "ℕ"),
    ("P", "ℙ"),
    ("Q", "ℚ"),
    ("R", "ℝ"),
    ("Z", "ℤ"),
];

static SUPERSCRIPTS: &[(&str, &str)] = &[
    ("0", "⁰"),
    ("1", "¹"),
    ("2", "²"),
    ("3", "³"),
    ("4", "⁴"),
    ("5", "⁵"),
    ("6", "⁶"),
    ("7", "⁷"),
    ("8", "⁸"),
    ("9", "⁹"),
    ("+", "⁺"),
    ("-", "⁻"),
    ("=", "⁼"),
    ("(", "⁽"),
    (")", "⁾"),
    ("a", "ᵃ"),
    ("b", "ᵇ"),
    ("c", "ᶜ"),
    ("d", "ᵈ"),
    ("e", "ᵉ"),
    ("f", "ᶠ"),
    ("g", "ᵍ"),
    ("h", "ʰ"),
    ("i", "ⁱ"),
    ("j", "ʲ"),
    ("k", "ᵏ"),
    ("l", "ˡ"),
    ("m", "ᵐ"),
    ("n", "ⁿ"),
    ("o", "ᵒ"),
    ("p", "ᵖ"),
    ("r", "ʳ"),
    ("s", "ˢ"),
    ("t", "ᵗ"),
    ("u", "ᵘ"),
    ("v", "ᵛ"),
    ("w", "ʷ"),
    ("x", "ˣ"),
    ("y", "ʸ"),
    ("z", "ᶻ"),
];

static SUBSCRIPTS: &[(&str, &str)] = &[
    ("0", "₀"),
    ("1", "₁"),
    ("2", "₂"),
    ("3", "₃"),
    ("4", "₄"),
    ("5", "₅"),
    ("6", "₆"),
    ("7", "₇"),
    ("8", "₈"),
    ("9", "₉"),
    ("+", "₊"),
    ("-", "₋"),
    ("=", "₌"),
    ("(", "₍"),
    (")", "₎"),
    ("a", "ₐ"),
    ("e", "ₑ"),
    ("h", "ₕ"),
    ("i", "ᵢ"),
    ("j", "ⱼ"),
    ("k", "ₖ"),
    ("l", "ₗ"),
    ("m", "ₘ"),
    ("n", "ₙ"),
    ("o", "ₒ"),
    ("p", "ₚ"),
    ("r", "ᵣ"),
    ("s", "ₛ"),
    ("t", "ₜ"),
    ("u", "ᵤ"),
    ("v", "ᵥ"),
    ("x", "ₓ"),
];

static ACCENTS: &[(&str, &str)] = &[
    ("acute", "́"),
    ("bar", "̅"),
    ("breve", "̆"),
    ("check", "̌"),
    ("ddot", "̈"),
    ("dot", "̇"),
    ("grave", "̀"),
    ("hat", "̂"),
    ("mathring", "̊"),
    ("overleftarrow", "⃖"),
    ("overleftrightarrow", "⃡"),
    ("overline", "̅"),
    ("overrightarrow", "⃗"),
    ("tilde", "̃"),
    ("underline", "̲"),
    ("vec", "⃗"),
    ("widehat", "̂"),
    ("widetilde", "̃"),
];

static NAMED_OPERATORS: &[&str] = &[
    "arccos", "arcsin", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det", "dim",
    "exp", "gcd", "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log", "max", "min",
    "Pr", "sec", "sin", "sinh", "sup", "tan", "tanh",
];

static LIMIT_OPERATORS: &[&str] = &[
    "argmax", "argmin", "inf", "injlim", "lim", "liminf", "limsup", "max", "min", "projlim", "sup",
];

static DISPLAY_LIMIT_SYMBOLS: &[&str] = &[
    "bigcap",
    "bigcup",
    "bigodot",
    "bigoplus",
    "bigotimes",
    "bigsqcup",
    "biguplus",
    "bigvee",
    "bigwedge",
    "coprod",
    "int",
    "iint",
    "iiint",
    "oint",
    "prod",
    "sum",
];

static RELATION_COMMANDS: &[&str] = &[
    "Leftarrow",
    "Leftrightarrow",
    "Longleftarrow",
    "Longleftrightarrow",
    "Longrightarrow",
    "Rightarrow",
    "Vdash",
    "Vvdash",
    "approx",
    "asymp",
    "cong",
    "dashv",
    "doteq",
    "downarrow",
    "equiv",
    "ge",
    "geq",
    "geqslant",
    "gets",
    "gg",
    "hookleftarrow",
    "hookrightarrow",
    "iff",
    "implies",
    "in",
    "leadsto",
    "le",
    "leftarrow",
    "leftharpoondown",
    "leftharpoonup",
    "leftrightarrow",
    "leftrightharpoons",
    "leq",
    "leqslant",
    "ll",
    "longleftarrow",
    "longleftrightarrow",
    "longmapsto",
    "longrightarrow",
    "mapsto",
    "mid",
    "models",
    "ne",
    "nearrow",
    "neq",
    "ni",
    "notin",
    "nvdash",
    "nvDash",
    "nwarrow",
    "parallel",
    "perp",
    "prec",
    "preceq",
    "propto",
    "rightharpoondown",
    "rightharpoonup",
    "rightleftharpoons",
    "rightarrow",
    "rightsquigarrow",
    "searrow",
    "sim",
    "simeq",
    "sqsubset",
    "sqsubseteq",
    "sqsupset",
    "sqsupseteq",
    "subset",
    "subseteq",
    "succ",
    "succeq",
    "supset",
    "supseteq",
    "swarrow",
    "to",
    "triangleleft",
    "triangleright",
    "twoheadleftarrow",
    "twoheadrightarrow",
    "uparrow",
    "vdash",
];

static SPACING_COMMANDS: &[&str] = &[
    ",",
    ":",
    ";",
    " ",
    ">",
    "enspace",
    "enskip",
    "medspace",
    "quad",
    "qquad",
    "thickspace",
    "thinspace",
];

static NEGATIVE_SPACING_COMMANDS: &[&str] = &["!", "negmedspace", "negthickspace", "negthinspace"];

static IGNORED_COMMANDS: &[&str] = &[
    "displaystyle",
    "limits",
    "nolimits",
    "scriptstyle",
    "scriptscriptstyle",
    "textstyle",
];

static SIZE_COMMANDS: &[&str] = &[
    "big", "Big", "bigg", "Bigg", "bigl", "Bigl", "biggl", "Biggl", "bigr", "Bigr", "biggr",
    "Biggr",
];

static PLAIN_WRAPPERS: &[&str] = &[
    "emph",
    "mathcal",
    "mathbf",
    "mathfrak",
    "mathit",
    "mathrm",
    "mathnormal",
    "mathscr",
    "mathsf",
    "mathtt",
    "mathup",
    "mbox",
    "overbrace",
    "pmb",
    "smash",
    "substack",
    "text",
    "textbf",
    "textit",
    "textmd",
    "textnormal",
    "textrm",
    "textsc",
    "textsf",
    "textsl",
    "texttt",
    "textup",
    "underbrace",
    "bm",
    "boldsymbol",
];

fn table_lookup<'v>(table: &[(&str, &'v str)], key: &str) -> Option<&'v str> {
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
}

fn table_has(table: &[&str], key: &str) -> bool {
    table.iter().any(|name| *name == key)
}

/// `replaceCharacters`: map every character through `table`, or fail.
fn replace_characters(value: &str, table: &[(&str, &str)]) -> Option<String> {
    let mut result = String::new();
    for character in value.chars() {
        let key = character.to_string();
        result.push_str(table_lookup(table, &key)?);
    }
    Some(result)
}

/// Collapse `\s*([=+-])\s*` to the operator character, upstream
/// `formatScript` pre-normalization.
fn collapse_operator_spacing(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut result = String::new();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '=' || character == '+' || character == '-' {
            result.push(character);
            index += 1;
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            continue;
        }
        if character.is_whitespace() {
            let mut lookahead = index;
            while lookahead < chars.len() && chars[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if lookahead < chars.len()
                && (chars[lookahead] == '=' || chars[lookahead] == '+' || chars[lookahead] == '-')
            {
                index = lookahead;
                continue;
            }
        }
        result.push(character);
        index += 1;
    }
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
    Sub,
    Sup,
}

fn format_script(value: &str, kind: ScriptKind) -> String {
    let value = value.trim();
    let replacements = match kind {
        ScriptKind::Sub => SUBSCRIPTS,
        ScriptKind::Sup => SUPERSCRIPTS,
    };
    let normalized = collapse_operator_spacing(value);
    if let Some(unicode) = replace_characters(&normalized, replacements) {
        return unicode;
    }

    let prefix = match kind {
        ScriptKind::Sub => '_',
        ScriptKind::Sup => '^',
    };
    let characters: Vec<char> = value.chars().collect();
    if characters.len() == 1
        || (kind == ScriptKind::Sub && value.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return format!("{prefix}{value}");
    }
    format!("{prefix}({value})")
}

fn is_simple_expression(value: &str) -> bool {
    // `/^[\p{L}\p{N}.]+$/u`
    value
        .chars()
        .all(|c| c.is_alphabetic() || c.is_numeric() || c == '.')
}

fn is_simple_numeric(value: &str) -> bool {
    // `/^[\p{N}.]+$/u`
    value.chars().all(|c| c.is_numeric() || c == '.')
}

fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = numerator.trim();
    let denominator = denominator.trim();
    let simple_numerator = is_simple_expression(numerator);
    let simple_denominator = is_simple_numeric(denominator) || denominator.chars().count() == 1;
    let numerator_text = if simple_numerator {
        numerator.to_owned()
    } else {
        format!("({numerator})")
    };
    let denominator_text = if simple_denominator {
        denominator.to_owned()
    } else {
        format!("({denominator})")
    };
    format!("{numerator_text}/{denominator_text}")
}

fn format_root(value: &str, symbol: &str) -> String {
    let value = value.trim();
    if is_simple_expression(value) {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

// ---------------------------------------------------------------------------
// Output normalization: named-operator spacing and line hygiene
// ---------------------------------------------------------------------------

/// `normalizeOutput`: resolve named-operator PUA markers to real spacing,
/// collapse runs of spaces/tabs per line, trim line edges, and drop empty
/// first/last lines (interior empty lines survive).
fn normalize_output(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    let mut resolved = String::with_capacity(characters.len());
    for (index, &character) in characters.iter().enumerate() {
        match character {
            NAMED_OPERATOR_START => {
                let previous = if index == 0 {
                    None
                } else {
                    Some(characters[index - 1])
                };
                let spaced = previous.is_some_and(|p| {
                    p.is_alphanumeric() || matches!(p, ')' | ']' | '}' | LAYOUT_MARKER_END)
                });
                if spaced {
                    resolved.push(' ');
                }
            }
            NAMED_OPERATOR_END => {
                let next = characters.get(index + 1).copied();
                let spaced = next
                    .is_some_and(|n| n.is_alphanumeric() || matches!(n, '√' | LAYOUT_MARKER_START));
                if spaced {
                    resolved.push(' ');
                }
            }
            other => resolved.push(other),
        }
    }

    let collapsed = resolved
        .split('\n')
        .map(collapse_spaces)
        .collect::<Vec<_>>();
    let total = collapsed.len();
    let kept = collapsed
        .into_iter()
        .enumerate()
        .filter(|(index, line)| !line.is_empty() || ((index + 1) > 1 && (index + 1) < total))
        .map(|(_, line)| line)
        .collect::<Vec<_>>();
    kept.join("\n").trim().to_owned()
}

/// `line.replace(/[ \t]+/g, " ").trim()` — collapse internal runs, trim ends.
fn collapse_spaces(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut in_run = false;
    for character in line.chars() {
        if character == ' ' || character == '\t' {
            in_run = true;
            continue;
        }
        if in_run && !result.is_empty() {
            result.push(' ');
        } else if in_run {
            // Leading run: trim (nothing emitted before it).
        }
        in_run = false;
        result.push(character);
    }
    result
}

// ---------------------------------------------------------------------------
// Layout composition (display-mode fractions, operator limits, matrices)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum LayoutNode {
    Fraction {
        numerator: String,
        denominator: String,
    },
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
    Matrix {
        lines: Vec<String>,
        baseline: usize,
    },
}

#[derive(Debug, Clone)]
struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    let mut result = String::new();
    for _ in 0..left {
        result.push(' ');
    }
    result.push_str(line);
    for _ in 0..(padding - left) {
        result.push(' ');
    }
    result
}

fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len().saturating_sub(l.baseline + 1))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for row in 0..=baseline + below {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row as isize - baseline as isize + layout.baseline as isize;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                let content = &layout.lines[source_row as usize];
                line.push_str(&pad_layout_line(content, layout.width, false));
            } else {
                for _ in 0..layout.width {
                    line.push(' ');
                }
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    Layout {
        lines,
        width: layouts.iter().map(|l| l.width).sum(),
        baseline,
    }
}

/// Parse a ``F0000<digits>F0001`` marker starting at `start`; returns the node
/// index and the char index just past the marker.
fn parse_marker(characters: &[char], start: usize) -> Option<(usize, usize)> {
    let mut index = start + 1;
    let mut digits = String::new();
    while index < characters.len() && characters[index].is_ascii_digit() {
        digits.push(characters[index]);
        index += 1;
    }
    if digits.is_empty() || index >= characters.len() || characters[index] != LAYOUT_MARKER_END {
        return None;
    }
    let node_index = digits.parse().ok()?;
    Some((node_index, index + 1))
}

fn text_layout(text: &str) -> Layout {
    Layout {
        lines: vec![text.to_owned()],
        width: visible_width(text),
        baseline: 0,
    }
}

fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0usize;
    for source_line in source.split('\n') {
        let characters: Vec<char> = source_line.chars().collect();
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0usize;
        let mut previous_matrix = false;
        let mut previous_any = false;

        let mut index = 0usize;
        while index < characters.len() {
            if characters[index] == LAYOUT_MARKER_START
                && let Some((node_index, after)) = parse_marker(&characters, index)
                && let Some(node) = nodes.get(node_index)
            {
                if index > position {
                    let sliced: String = characters[position..index].iter().collect();
                    let trimmed = if previous_any {
                        sliced.trim_start()
                    } else {
                        sliced.as_str()
                    }
                    .trim_end();
                    let preserve_leading =
                        previous_matrix && sliced.starts_with(|c: char| c.is_whitespace());
                    let preserve_trailing = matches!(node, LayoutNode::Matrix { .. })
                        && sliced.ends_with(|c: char| c.is_whitespace());
                    let text = if !trimmed.is_empty() {
                        let mut text = String::new();
                        if preserve_leading {
                            text.push(' ');
                        }
                        text.push_str(trimmed);
                        if preserve_trailing {
                            text.push(' ');
                        }
                        text
                    } else if preserve_leading || preserve_trailing {
                        " ".to_owned()
                    } else {
                        String::new()
                    };
                    layouts.push(text_layout(&text));
                }
                match node {
                    LayoutNode::Fraction {
                        numerator,
                        denominator,
                    } => {
                        let numerator_layout = render_layout(numerator, nodes);
                        let denominator_layout = render_layout(denominator, nodes);
                        let content_width =
                            numerator_layout.width.max(denominator_layout.width).max(1);
                        let width = content_width + 2;
                        let mut lines = Vec::new();
                        for line in &numerator_layout.lines {
                            lines.push(pad_layout_line(line, width, true));
                        }
                        lines.push(format!(" {} ", "─".repeat(content_width)));
                        for line in &denominator_layout.lines {
                            lines.push(pad_layout_line(line, width, true));
                        }
                        layouts.push(Layout {
                            lines,
                            width,
                            baseline: numerator_layout.lines.len(),
                        });
                    }
                    LayoutNode::Operator {
                        operator,
                        lower,
                        upper,
                    } => {
                        let content_width = visible_width(operator)
                            .max(upper.as_deref().map_or(0, visible_width))
                            .max(lower.as_deref().map_or(0, visible_width));
                        let mut lines = Vec::new();
                        if let Some(upper) = upper {
                            lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
                        }
                        lines.push(format!(
                            "{} ",
                            pad_layout_line(operator, content_width, true)
                        ));
                        if let Some(lower) = lower {
                            lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
                        }
                        layouts.push(Layout {
                            lines,
                            width: content_width + 1,
                            baseline: usize::from(upper.is_some()),
                        });
                    }
                    LayoutNode::Matrix { lines, baseline } => {
                        let width = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
                        layouts.push(Layout {
                            lines: lines
                                .iter()
                                .map(|line| pad_layout_line(line, width, false))
                                .collect(),
                            width,
                            baseline: *baseline,
                        });
                    }
                }
                position = after;
                previous_matrix = matches!(node, LayoutNode::Matrix { .. });
                previous_any = true;
                index = after;
                continue;
            }
            index += 1;
        }

        if position < characters.len() {
            let sliced: String = characters[position..].iter().collect();
            let trimmed = if previous_any {
                sliced.trim_start()
            } else {
                sliced.as_str()
            };
            let text = if previous_matrix && sliced.starts_with(|c: char| c.is_whitespace()) {
                format!(" {trimmed}")
            } else {
                trimmed.to_owned()
            };
            layouts.push(text_layout(&text));
        }

        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }

    Layout {
        width: rendered_lines
            .iter()
            .map(|line| visible_width(line))
            .max()
            .unwrap_or(0),
        lines: rendered_lines,
        baseline: first_baseline,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperatorStyle {
    Bracket,
    Script,
}

struct LatexParser<'a> {
    source: &'a [char],
    nodes: &'a mut Vec<LayoutNode>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
}

impl<'a> LatexParser<'a> {
    fn new(source: &'a [char], nodes: &'a mut Vec<LayoutNode>, display: bool) -> Self {
        Self {
            source,
            nodes,
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
        }
    }

    fn peek(&self) -> Option<char> {
        self.source.get(self.position).copied()
    }

    fn render(mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    fn parse_sequence(&mut self, end_character: Option<char>) -> String {
        let mut result = String::new();
        while let Some(character) = self.peek() {
            if end_character == Some(character) {
                self.position += 1;
                return result;
            }

            match character {
                '}' => {
                    self.supported = false;
                    return result;
                }
                '{' => {
                    self.position += 1;
                    result.push_str(&self.parse_sequence(Some('}')));
                }
                '\\' => {
                    let command = self.parse_command();
                    if command == NEGATIVE_SPACE_SENTINEL.to_string() {
                        result = result.trim_end().to_owned();
                        if result.ends_with(NAMED_OPERATOR_END) {
                            result.pop();
                        }
                    } else {
                        result.push_str(&command);
                    }
                }
                '^' | '_' => {
                    self.position += 1;
                    result = result.trim_end().to_owned();
                    let script = format_script(
                        &self.parse_required_argument(false),
                        if character == '_' {
                            ScriptKind::Sub
                        } else {
                            ScriptKind::Sup
                        },
                    );
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.pop();
                        result.push_str(&script);
                        result.push(NAMED_OPERATOR_END);
                    } else {
                        result.push_str(&script);
                    }
                }
                c if c.is_whitespace() => {
                    result.push_str(&self.parse_whitespace());
                }
                '=' | '<' | '>' => {
                    result = format!("{} {character} ", result.trim_end());
                    self.position += 1;
                }
                '&' => {
                    self.position += 1;
                }
                '~' => {
                    self.position += 1;
                    result.push(' ');
                }
                '.' => {
                    if let Some(node_index) = trailing_marker_index(&result)
                        && let Some(LayoutNode::Matrix { lines, .. }) =
                            self.nodes.get_mut(node_index)
                    {
                        if let Some(last) = lines.last_mut() {
                            last.push('.');
                        }
                        self.position += 1;
                        continue;
                    }
                    result.push(character);
                    self.position += 1;
                }
                c => {
                    result.push(c);
                    self.position += 1;
                }
            }
        }

        if end_character.is_some() {
            self.supported = false;
        }
        result
    }

    fn parse_whitespace(&mut self) -> &'static str {
        while self.peek().is_some_and(|c| c.is_whitespace()) {
            self.position += 1;
        }
        " "
    }

    fn parse_command(&mut self) -> String {
        self.position += 1;
        let Some(first) = self.peek() else {
            self.supported = false;
            return String::new();
        };

        let command: String = if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.peek() == Some('\n') {
                self.position += 1;
            }
            return " ".to_owned();
        } else if first.is_ascii_alphabetic() {
            let start = self.position;
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.position += 1;
            }
            self.source[start..self.position].iter().collect()
        } else {
            self.position += 1;
            first.to_string()
        };

        if command == "\\" {
            return "\n".to_owned();
        }
        if table_has(SPACING_COMMANDS, &command) {
            return " ".to_owned();
        }
        if table_has(NEGATIVE_SPACING_COMMANDS, &command) {
            return NEGATIVE_SPACE_SENTINEL.to_string();
        }
        if table_has(IGNORED_COMMANDS, &command) {
            return String::new();
        }
        if matches!(command.as_str(), "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command;
        }
        if command == "|" {
            return "‖".to_owned();
        }
        if command == "not" {
            let value = self.parse_required_argument(false).trim().to_owned();
            if let Some(negated) = table_lookup(NEGATED_SYMBOLS, &value) {
                return format!(" {negated} ");
            }
            let mut characters = value.chars();
            let Some(first) = characters.next() else {
                self.supported = false;
                return String::new();
            };
            let rest: String = characters.collect();
            return format!(" {first}\u{0338}{rest} ");
        }
        if table_has(LIMIT_OPERATORS, &command) {
            return self.parse_operator(&command, OperatorStyle::Bracket, true, true);
        }

        if let Some(symbol) = table_lookup(SYMBOLS, &command) {
            if table_has(DISPLAY_LIMIT_SYMBOLS, &command) {
                return self.parse_operator(symbol, OperatorStyle::Script, true, false);
            }
            if command == "cdot" || command == "times" || table_has(RELATION_COMMANDS, &command) {
                return format!(" {symbol} ");
            }
            return symbol.to_owned();
        }
        if table_has(NAMED_OPERATORS, &command) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if table_has(SIZE_COMMANDS, &command) {
            return String::new();
        }
        if command == "left" || command == "middle" || command == "right" {
            if self.peek() == Some('.') {
                self.position += 1;
            }
            return String::new();
        }
        if command == "frac" || command == "dfrac" || command == "tfrac" {
            let should_stack = self.display && self.stack_fractions && command != "tfrac";
            let numerator = self.parse_required_argument(!should_stack);
            let denominator = self.parse_required_argument(!should_stack);
            if should_stack {
                let index = self.nodes.len();
                self.nodes.push(LayoutNode::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
                return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
            }
            return format_fraction(&numerator, &denominator);
        }
        if command == "sqrt" {
            let degree = self.parse_optional_argument().map(|d| d.trim().to_owned());
            let value = self.parse_required_argument(true);
            let symbol = match degree.as_deref() {
                None | Some("2") => format_root(&value, "√"),
                Some("3") => format_root(&value, "∛"),
                Some("4") => format_root(&value, "∜"),
                Some(other) => format!(
                    "{}{}",
                    format_script(other, ScriptKind::Sup),
                    format_root(&value, "√")
                ),
            };
            return symbol;
        }
        if command == "boxed" || command == "fbox" {
            return format!("[{}]", self.parse_required_argument(true).trim());
        }
        if command == "binom" || command == "dbinom" || command == "tbinom" {
            let first = self.parse_required_argument(true);
            let second = self.parse_required_argument(true);
            return format!("({first} choose {second})");
        }
        if let Some(accent) = table_lookup(ACCENTS, &command) {
            let value = self.parse_required_argument(true);
            return if value.chars().count() == 1 {
                format!("{value}{accent}")
            } else {
                format!("{command}({value})")
            };
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(true);
            let mut mapped = String::new();
            for c in value.chars() {
                match table_lookup(BLACKBOARD, &c.to_string()) {
                    Some(replacement) => mapped.push_str(replacement),
                    None => mapped.push(c),
                }
            }
            return mapped;
        }
        if command == "operatorname" {
            let starred = self.peek() == Some('*');
            if starred {
                self.position += 1;
            }
            let operator = normalize_output(&self.parse_required_argument(true))
                .trim()
                .to_owned();
            return self.parse_operator(&operator, OperatorStyle::Bracket, starred, true);
        }
        if command == "mod" || command == "bmod" {
            return " mod ".to_owned();
        }
        if command == "pmod" || command == "pod" {
            let value = self.parse_required_argument(true).trim().to_owned();
            return if command == "pmod" {
                format!(" (mod {value})")
            } else {
                format!(" ({value})")
            };
        }
        if command == "overset" || command == "stackrel" {
            let upper = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_owned();
            return format!("{value}{}", format_script(&upper, ScriptKind::Sup));
        }
        if command == "underset" {
            let lower = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_owned();
            return format!("{value}{}", format_script(&lower, ScriptKind::Sub));
        }
        if table_has(PLAIN_WRAPPERS, &command) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" {
                value
            } else {
                value.trim().to_owned()
            };
        }
        if command == "begin" {
            return self.parse_environment();
        }
        if command == "end" {
            self.supported = false;
            return String::new();
        }

        self.supported = false;
        format!("\\{command}")
    }

    fn parse_operator(
        &mut self,
        operator: &str,
        inline_lower_style: OperatorStyle,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        let mut modifier_position = self.position;
        while self
            .source
            .get(modifier_position)
            .is_some_and(|c| *c == ' ' || *c == '\t')
        {
            modifier_position += 1;
        }
        for modifier in ["\\limits", "\\nolimits"] {
            let candidate: Vec<char> = modifier.chars().collect();
            if self.source.len() >= modifier_position + candidate.len()
                && self.source[modifier_position..modifier_position + candidate.len()]
                    == candidate[..]
                && !self
                    .source
                    .get(modifier_position + candidate.len())
                    .is_some_and(|c| c.is_ascii_alphabetic())
            {
                use_display_limits = modifier == "\\limits";
                self.position = modifier_position + candidate.len();
                break;
            }
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while self
                .source
                .get(script_position)
                .is_some_and(|c| *c == ' ' || *c == '\t')
            {
                script_position += 1;
            }
            let kind = self.source.get(script_position).copied();
            if kind != Some('_') && kind != Some('^') {
                break;
            }
            self.position = script_position + 1;
            let value = normalize_output(&self.parse_required_argument(false)).replace(' ', "");
            if kind == Some('_') {
                if lower.is_some() {
                    self.supported = false;
                }
                lower = Some(value);
            } else {
                if upper.is_some() {
                    self.supported = false;
                }
                upper = Some(value);
            }
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            let index = self.nodes.len();
            self.nodes.push(LayoutNode::Operator {
                operator: operator.to_owned(),
                lower: lower.clone(),
                upper: upper.clone(),
            });
            return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
        }

        let mut rendered = operator.to_owned();
        if let Some(lower) = &lower {
            rendered.push_str(&match inline_lower_style {
                OperatorStyle::Bracket => format!("[{lower}]"),
                OperatorStyle::Script => format_script(lower, ScriptKind::Sub),
            });
        }
        if let Some(upper) = &upper {
            rendered.push_str(&format_script(upper, ScriptKind::Sup));
        }
        if spaced {
            format!(" {rendered} ")
        } else {
            rendered
        }
    }

    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    fn parse_required_argument_value(&mut self) -> String {
        while self.peek().is_some_and(|c| c.is_whitespace()) {
            self.position += 1;
        }
        let Some(character) = self.peek() else {
            self.supported = false;
            return String::new();
        };
        match character {
            '{' => {
                self.position += 1;
                self.parse_sequence(Some('}'))
            }
            '\\' => self.parse_command(),
            c => {
                self.position += 1;
                c.to_string()
            }
        }
    }

    fn parse_optional_argument(&mut self) -> Option<String> {
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.position += 1;
        }
        if self.peek() != Some('[') {
            return None;
        }
        let Some(end) = self.source[self.position + 1..]
            .iter()
            .position(|c| *c == ']')
            .map(|offset| self.position + 1 + offset)
        else {
            self.supported = false;
            return None;
        };
        let value: String = self.source[self.position + 1..end].iter().collect();
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    fn read_raw_group(&mut self) -> Option<String> {
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.position += 1;
        }
        if self.peek() != Some('{') {
            self.supported = false;
            return None;
        }

        self.position += 1;
        let start = self.position;
        let mut depth = 1usize;
        while self.position < self.source.len() {
            let character = self.source[self.position];
            if character == '\\' {
                self.position += 2;
                continue;
            }
            if character == '{' {
                depth += 1;
            }
            if character == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value: String = self.source[start..self.position].iter().collect();
                self.position += 1;
                return Some(value);
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    fn split_environment_rows(body: &str) -> Vec<String> {
        let characters: Vec<char> = body.chars().collect();
        let mut rows = Vec::new();
        let mut current = String::new();
        let mut index = 0;
        while index < characters.len() {
            if characters[index] == '\\' && characters.get(index + 1) == Some(&'\\') {
                index += 2;
                // Optional `[dimen]` (no `]` or newline inside).
                if characters.get(index) == Some(&'[') {
                    let mut scan = index + 1;
                    while scan < characters.len()
                        && characters[scan] != ']'
                        && characters[scan] != '\n'
                    {
                        scan += 1;
                    }
                    if scan < characters.len() && characters[scan] == ']' {
                        index = scan + 1;
                    }
                }
                rows.push(std::mem::take(&mut current));
                continue;
            }
            current.push(characters[index]);
            index += 1;
        }
        rows.push(current);
        rows
    }

    fn parse_environment(&mut self) -> String {
        let Some(environment) = self.read_raw_group() else {
            return String::new();
        };
        let end_marker: Vec<char> = format!("\\end{{{environment}}}").chars().collect();
        let Some(end) = self.source[self.position..]
            .windows(end_marker.len())
            .position(|window| window == &end_marker[..])
            .map(|offset| self.position + offset)
        else {
            self.supported = false;
            return String::new();
        };
        let body: String = self.source[self.position..end].iter().collect();
        self.position = end + end_marker.len();

        if matches!(
            environment.as_str(),
            "equation" | "equation*" | "displaymath"
        ) {
            return self.render_nested(&body, true).trim().to_owned();
        }

        if matches!(
            environment.as_str(),
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            let aligned_at = matches!(environment.as_str(), "alignedat" | "alignat" | "alignat*");
            let aligned_body = if aligned_at {
                strip_leading_group(&body)
            } else {
                body.clone()
            };
            return Self::split_environment_rows(&aligned_body)
                .into_iter()
                .map(|row| {
                    let cells: Vec<&str> = row.split('&').collect();
                    let source = if aligned_at {
                        let pairs = cells.len().div_ceil(2);
                        (0..pairs)
                            .map(|index| {
                                cells
                                    .get(index * 2..index * 2 + 2)
                                    .map_or(String::new(), |pair| pair.concat())
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    } else {
                        cells.concat()
                    };
                    self.render_nested(&source, true).trim().to_owned()
                })
                .filter(|row| !row.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
        }

        if environment == "cases" || environment == "cases*" {
            let rows: Vec<Vec<String>> = Self::split_environment_rows(&body)
                .into_iter()
                .map(|row| {
                    row.split('&')
                        .map(|cell| self.render_nested(cell, false).trim().to_owned())
                        .collect::<Vec<_>>()
                })
                .filter(|row| row.iter().any(|cell| !cell.is_empty()))
                .collect();
            return rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let value = strip_trailing_comma(row.first().map_or("", String::as_str));
                    let condition = row.get(1).map_or("", String::as_str);
                    let delimiter = if index == 0 {
                        '⎧'
                    } else if index + 1 == rows.len() {
                        '⎩'
                    } else {
                        '⎨'
                    };
                    let condition_prefix = if starts_with_condition_word(condition) {
                        " "
                    } else {
                        " if "
                    };
                    if condition.is_empty() {
                        format!("{delimiter} {value}")
                    } else {
                        format!("{delimiter} {value}{condition_prefix}{condition}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }

        if matches!(
            environment.as_str(),
            "array"
                | "matrix"
                | "smallmatrix"
                | "pmatrix"
                | "bmatrix"
                | "Bmatrix"
                | "vmatrix"
                | "Vmatrix"
        ) {
            let matrix_body = if environment == "array" {
                strip_leading_group(&body)
            } else {
                body
            };
            return self.render_matrix(&environment, &matrix_body);
        }

        self.supported = false;
        body
    }

    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix: Vec<Vec<String>> = Self::split_environment_rows(body)
            .into_iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| self.render_nested(cell, false).trim().to_owned())
                    .collect::<Vec<_>>()
            })
            .filter(|row| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        let column_count = matrix.iter().map(|row| row.len()).max().unwrap_or(0);
        let column_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                matrix
                    .iter()
                    .map(|row| row.get(column).map_or(0, |cell| visible_width(cell)))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..column_count)
                    .map(|column| {
                        let cell = row.get(column).map_or("", String::as_str);
                        let pad = column_widths[column].saturating_sub(visible_width(cell));
                        format!("{cell}{}", PROTECTED_SPACE.to_string().repeat(pad))
                    })
                    .collect::<Vec<_>>()
                    .join(" │ ")
            })
            .collect();

        let lines: Vec<String> =
            if environment == "array" || environment == "matrix" || environment == "smallmatrix" {
                rows
            } else {
                let delimiters: [char; 6] = match environment {
                    "pmatrix" => ['⎛', '⎞', '⎜', '⎟', '⎝', '⎠'],
                    "bmatrix" => ['⎡', '⎤', '⎢', '⎥', '⎣', '⎦'],
                    "Bmatrix" => ['⎧', '⎫', '⎨', '⎬', '⎩', '⎭'],
                    "vmatrix" => ['│', '│', '│', '│', '│', '│'],
                    _ => ['║', '║', '║', '║', '║', '║'],
                };
                let total = rows.len();
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let left = if index == 0 {
                            delimiters[0]
                        } else if index + 1 == total {
                            delimiters[4]
                        } else {
                            delimiters[2]
                        };
                        let right = if index == 0 {
                            delimiters[1]
                        } else if index + 1 == total {
                            delimiters[5]
                        } else {
                            delimiters[3]
                        };
                        format!("{left} {row} {right}")
                    })
                    .collect()
            };

        if lines.len() <= 1 {
            return lines.first().cloned().unwrap_or_default();
        }
        let index = self.nodes.len();
        self.nodes.push(LayoutNode::Matrix {
            lines: lines.clone(),
            baseline: 0,
        });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let characters: Vec<char> = source.chars().collect();
        let nested = LatexParser::new(&characters, self.nodes, self.display && stack_fractions);
        match nested.render() {
            Some(rendered) => rendered,
            None => {
                self.supported = false;
                source.to_owned()
            }
        }
    }
}

/// `trailing_layout_marker`: when `result` ends with a layout marker, the
/// index it references (the parser's `.`-appends-to-matrix seam).
fn trailing_marker_index(result: &str) -> Option<usize> {
    let characters: Vec<char> = result.chars().collect();
    if characters.len() < 3 {
        return None;
    }
    let mut index = characters.len() - 1;
    if characters[index] != LAYOUT_MARKER_END {
        return None;
    }
    index -= 1;
    let mut digits = String::new();
    while index > 0 && characters[index].is_ascii_digit() {
        digits.insert(0, characters[index]);
        index -= 1;
    }
    if characters[index] != LAYOUT_MARKER_START {
        return None;
    }
    digits.parse().ok()
}

/// `^\s*\{[^}]*\}` — drop an alignat/array column-spec group.
fn strip_leading_group(body: &str) -> String {
    let stripped = body.trim_start();
    let mut chars = stripped.chars();
    if chars.next() != Some('{') {
        return body.to_owned();
    }
    for (offset, character) in stripped.char_indices().skip(1) {
        if character == '}' {
            return stripped[offset + 1..].to_owned();
        }
        if character == '{' {
            return body.to_owned();
        }
    }
    body.to_owned()
}

/// `/,\s*$/`
fn strip_trailing_comma(value: &str) -> &str {
    let trimmed = value.trim_end();
    if let Some(without_comma) = trimmed.strip_suffix(',') {
        without_comma.trim_end()
    } else {
        trimmed
    }
}

/// `^(?:if|when|for|otherwise)\b` (case-insensitive).
fn starts_with_condition_word(condition: &str) -> bool {
    const WORDS: [&str; 4] = ["if", "when", "for", "otherwise"];
    let lower = condition.to_lowercase();
    for word in WORDS {
        if lower.starts_with(word) {
            let rest = &lower[word.len()..];
            let boundary = rest
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if boundary {
                return true;
            }
        }
    }
    false
}

/// Render a basic LaTeX math expression as terminal-friendly Unicode text.
///
/// Returns `None` when the expression contains unsupported or malformed
/// syntax; callers fall back to the raw source (T4 rendered-vs-fallback
/// contract, `docs/PAR-MATH-latex-strategy.md`).
#[must_use]
pub fn render_latex(source: &str, display: bool) -> Option<String> {
    let characters: Vec<char> = source.chars().collect();
    let mut nodes: Vec<LayoutNode> = Vec::new();
    let parser = LatexParser::new(&characters, &mut nodes, display);
    let rendered = parser.render()?;

    if nodes.is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }

    let layout = render_layout(&rendered, &nodes);
    let indentation = layout
        .lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);
    let joined = layout
        .lines
        .iter()
        .map(|line| {
            line.chars()
                .skip(indentation)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(joined.trim_end().replace(PROTECTED_SPACE, " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str) -> Option<String> {
        render_latex(source, false)
    }

    fn render_display(source: &str) -> Option<String> {
        render_latex(source, true)
    }

    #[test]
    fn command_tables_are_entry_exact() {
        assert_eq!(SYMBOLS.len(), 217);
        assert_eq!(NEGATED_SYMBOLS.len(), 30);
        assert_eq!(BLACKBOARD.len(), 7);
        assert_eq!(SUPERSCRIPTS.len(), 40);
        assert_eq!(SUBSCRIPTS.len(), 32);
        assert_eq!(ACCENTS.len(), 18);
        assert_eq!(NAMED_OPERATORS.len(), 32);
        assert_eq!(LIMIT_OPERATORS.len(), 11);
        assert_eq!(DISPLAY_LIMIT_SYMBOLS.len(), 16);
        assert_eq!(RELATION_COMMANDS.len(), 81);
        assert_eq!(SPACING_COMMANDS.len(), 12);
        assert_eq!(NEGATIVE_SPACING_COMMANDS.len(), 4);
        assert_eq!(IGNORED_COMMANDS.len(), 6);
        assert_eq!(SIZE_COMMANDS.len(), 12);
        assert_eq!(PLAIN_WRAPPERS.len(), 30);
    }

    #[test]
    fn symbols_scripts_and_relations() {
        assert_eq!(
            render(r"\mathbb{C}^3 \to \mathbb{C}^3").as_deref(),
            Some("ℂ³ → ℂ³")
        );
        assert_eq!(
            render(r"F_1 = -\frac{1}{4x^2}.").as_deref(),
            Some("F₁ = -1/(4x²).")
        );
        assert_eq!(render("x=0").as_deref(), Some("x = 0"));
        assert_eq!(render("x =y").as_deref(), Some("x = y"));
        assert_eq!(render("x\n=\ny").as_deref(), Some("x = y"));
        assert_eq!(render("x_{i=0}").as_deref(), Some("xᵢ₌₀"));
        assert_eq!(render(r"x\neq0").as_deref(), Some("x ≠ 0"));
        assert_eq!(
            render("G = u^2 z + y^2(4+3xy)").as_deref(),
            Some("G = u² z + y²(4+3xy)")
        );
        assert_eq!(
            render(r"\{3x+2y,\; x(x-1)(x+1)\} \Rightarrow x \in \{0, \pm 1\}").as_deref(),
            Some("{3x+2y, x(x-1)(x+1)} ⇒ x ∈ {0, ± 1}")
        );
        assert_eq!(
            render(r"\epsilon+\varepsilon+\varsigma+\varkappa+\oplus+\otimes+\therefore+\because")
                .as_deref(),
            Some("ϵ+ε+ς+ϰ+⊕+⊗+∴+∵")
        );
        assert_eq!(
            render(r"A\not\subseteq B,\quad x\not\in X").as_deref(),
            Some("A ⊈ B, x ∉ X")
        );
    }

    #[test]
    fn named_operators_and_spacing() {
        assert_eq!(render(r"\sin\theta").as_deref(), Some("sin θ"));
        assert_eq!(render(r"\sin^2 x").as_deref(), Some("sin² x"));
        assert_eq!(render(r"-\sin\theta").as_deref(), Some("-sin θ"));
        assert_eq!(render(r"i\sin\theta").as_deref(), Some("i sin θ"));
        assert_eq!(render(r"\det(A)").as_deref(), Some("det(A)"));
        assert_eq!(render(r"\pi\cdot\frac{1}{\pi}").as_deref(), Some("π · 1/π"));
        assert_eq!(
            render(r"\det\!\left(\frac{\partial(F_1,F_2)}{\partial(x,y)}\right)=-2.").as_deref(),
            Some("det((∂(F₁,F₂))/(∂(x,y))) = -2.")
        );
    }

    #[test]
    fn roots_scripts_and_accents() {
        assert_eq!(
            render(r"\sqrt[2]{x}+\sqrt[3]{x}+\sqrt[4]{x}+\sqrt[n]{x}+\sqrt[k]{x+1}").as_deref(),
            Some("√x+∛x+∜x+ⁿ√x+ᵏ√(x+1)")
        );
        assert_eq!(
            render(r"\acute{x}+\grave{y}+\widehat{xyz}+\overrightarrow{AB}").as_deref(),
            Some("x́+ỳ+widehat(xyz)+overrightarrow(AB)")
        );
        assert_eq!(
            render(r"\binom{n}{k}+\vec{x}+\hat{y}+\overline{AB}").as_deref(),
            Some("(n choose k)+x⃗+ŷ+overline(AB)")
        );
        assert_eq!(
            render(r"\textnormal{hello}+\mbox{world}+\boldsymbol{x}").as_deref(),
            Some("hello+world+x")
        );
        assert_eq!(render(r"e^{i\pi}+1=0").as_deref(), Some("e^(iπ)+1 = 0"));
    }

    #[test]
    fn operators_mod_and_overlays() {
        assert_eq!(
            render(r"\operatorname*{arg\,max}_{x\in X} f(x)").as_deref(),
            Some("arg max[x∈X] f(x)")
        );
        assert_eq!(
            render(r"a\bmod n,\quad a\equiv b\pmod n").as_deref(),
            Some("a mod n, a ≡ b (mod n)")
        );
        assert_eq!(
            render(r"\overset{!}{=}+\underset{n}{x}+\stackrel{def}{=}").as_deref(),
            Some("=^!+xₙ+=ᵈᵉᶠ")
        );
        assert_eq!(
            render(r"\int_0^\infty e^{-x^2}\,dx=\frac{\sqrt{\pi}}{2}").as_deref(),
            Some("∫₀^∞ e^(-x²) dx = (√π)/2")
        );
        assert_eq!(
            render(r"\sum_{n=1}^{\infty}\frac{1}{n^2}=\frac{\pi^2}{6}").as_deref(),
            Some("∑ₙ₌₁^∞1/(n²) = π²/6")
        );
    }

    #[test]
    fn delimiters_and_control_space() {
        assert_eq!(
            render(r"\lvert{x}\rvert+\lVert{v}\rVert+\left.\frac{dy}{dx}\right|_{x=0}").as_deref(),
            Some("|x|+‖v‖+dy/(dx)|ₓ₌₀")
        );
        assert_eq!(
            render(r"\left\lbrace x \middle| x>0 \right\rbrace").as_deref(),
            Some("{ x | x > 0 }")
        );
        assert_eq!(render("a\\\r\nb").as_deref(), Some("a b"));
        assert_eq!(
            render(r"5\ \text{km}^2 = 5{,}000{,}000\ \text{m}^2").as_deref(),
            Some("5 km² = 5,000,000 m²")
        );
    }

    #[test]
    fn environments() {
        assert_eq!(
            render(r"\begin{equation}\begin{split}a&=b\\&=c\end{split}\end{equation}").as_deref(),
            Some("a = b\n= c")
        );
        assert_eq!(
            render(r"\begin{alignedat}{2}a&=b&\quad c&=d\\e&=f&g&=h\end{alignedat}").as_deref(),
            Some("a = b c = d\ne = f g = h")
        );
        assert_eq!(
            render(r"\begin{cases}a & x<0 \\ b & \text{if }x=0 \\ c & \text{otherwise}\end{cases}")
                .as_deref(),
            Some("⎧ a if x < 0\n⎨ b if x = 0\n⎩ c otherwise")
        );
        assert_eq!(
            render(r"\begin{pmatrix}1&200\\3000&4\end{pmatrix}").as_deref(),
            Some("⎛ 1    │ 200 ⎞\n⎝ 3000 │ 4   ⎠")
        );
        assert_eq!(render(r"\begin{aligned}F(0,0,-\tfrac14)&=(-\tfrac14,0,0),\\F(1,-\tfrac32,\tfrac{13}2)&=(-\tfrac14,0,0).\end{aligned}").as_deref(),
            Some("F(0,0,-1/4) = (-1/4,0,0),\nF(1,-3/2,13/2) = (-1/4,0,0)."));
    }

    #[test]
    fn display_mode_stacks_fractions_and_limits() {
        assert_eq!(
            render_display(r"x=\frac{-b\pm\sqrt{b^2-4ac}}{2a}").as_deref(),
            Some("    -b±√(b²-4ac)\nx = ────────────\n         2a")
        );
        assert_eq!(
            render_display(r"\frac{x^2+1}{x-1}").as_deref(),
            Some("x²+1\n────\nx-1")
        );
        assert_eq!(render(r"\sum_{i=0}^n x_i").as_deref(), Some("∑ᵢ₌₀ⁿ xᵢ"));
        assert_eq!(
            render_display(r"\sum_{i=0}^n x_i").as_deref(),
            Some(" n\n ∑  xᵢ\ni=0")
        );
        assert_eq!(
            render_display(r"\min_{x\in X} f(x)").as_deref(),
            Some("min f(x)\nx∈X")
        );
        assert_eq!(
            render_display(r"\int\nolimits_0^1 f(x)\,dx").as_deref(),
            Some("∫₀¹ f(x) dx")
        );
        assert_eq!(
            render_display(r"\int\limits_0^1 f(x)\,dx").as_deref(),
            Some("1\n∫ f(x) dx\n0")
        );
    }

    #[test]
    fn nested_display_fractions_stay_linear() {
        assert_eq!(
            render_display(r"\frac{\frac{x^2+1}{x-1}-\frac{2x}{x+1}}{\frac{x}{x^2-1}}").as_deref(),
            Some("(x²+1)/(x-1)-2x/(x+1)\n─────────────────────\n      x/(x²-1)")
        );
        assert_eq!(
            render_display(r"e^{\frac{1}{2}}").as_deref(),
            Some("e^(1/2)")
        );
        assert_eq!(render_display(r"\tfrac{1}{2}").as_deref(), Some("1/2"));
    }

    #[test]
    fn display_matrices_compose_with_adjacent_layout() {
        assert_eq!(render_display(r"R\left(\frac{\pi}{4}\right)=\begin{pmatrix}\frac{\sqrt{2}}{2} & -\frac{\sqrt{2}}{2}\\\frac{\sqrt{2}}{2} & \frac{\sqrt{2}}{2}\end{pmatrix}.").as_deref(),
            Some("   π\nR( ─ ) = ⎛ (√2)/2 │ -(√2)/2 ⎞\n   4     ⎝ (√2)/2 │ (√2)/2  ⎠."));
        assert_eq!(render_display(r"A\mathbf e_1=\begin{pmatrix}\pi\\0\end{pmatrix},\qquad A\mathbf e_2=\begin{pmatrix}0\\\frac{1}{\pi}\end{pmatrix}.").as_deref(),
            Some("Ae₁ = ⎛ π ⎞, Ae₂ = ⎛ 0   ⎞\n      ⎝ 0 ⎠        ⎝ 1/π ⎠."));
    }

    #[test]
    fn unsupported_and_malformed_inputs_return_none() {
        assert_eq!(render(r"x + \unknown{y}"), None);
        for malformed in [r"\frac{1}{x", "x}", r"\begin{matrix}1 & 2", "x\\"] {
            assert_eq!(render(malformed), None, "expected None for {malformed:?}");
        }
    }
}
