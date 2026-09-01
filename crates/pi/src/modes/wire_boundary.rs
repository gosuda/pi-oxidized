//! ARC12 boundary guard: modes consume the product projection
//! (`core::extension_host::ExtensionUiEvent`), never raw pi-ext event types.
//!
//! `LEDGER` below is the complete set of `pi_ext` symbols each mode file may
//! name, with a written reason per group. The scan fails on any symbol not
//! listed AND on any listed symbol that no longer appears, so the ledger
//! cannot rot into a rubber stamp. `FORBIDDEN` names the raw inbound event
//! types that must never appear in mode code, each with a remedy string.

use std::fs;
use std::path::{Path, PathBuf};

const MODES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/modes");

/// (relative file path, &[(pi_ext module, symbol, reason)]) — the complete,
/// bidirectionally-checked allowance per mode file. Built by scanning the
/// real tree; every entry must be observed and every observation must be here.
const LEDGER: &[(&str, &[(&str, &str, &str)])] = &[
    (
        "interactive/runtime.rs",
        &[
            // dialog lifecycle — ARC10 typed correlated path, owned by pi_ext::client
            ("client", "DialogEnd", "dialog lifecycle"),
            ("client", "DialogOutcome", "dialog lifecycle"),
            ("client", "HostUiRequest", "dialog lifecycle"),
            ("client", "HostUiResponse", "dialog lifecycle"),
            // dialog request builders used inline when constructing host prompts
            ("protocol", "ConfirmRequest", "dialog request builder"),
            ("protocol", "DialogOptions", "dialog request builder"),
            ("protocol", "EditorRequest", "dialog request builder"),
            ("protocol", "InputRequest", "dialog request builder"),
            // layout value adapters — structurally identical to StyledRun/Style
            ("protocol", "Hyperlink", "layout value adapter"),
            ("protocol", "OverlaySpec", "layout value adapter"),
            ("protocol", "SlotPlacement", "layout value adapter"),
            ("protocol", "Style", "layout value adapter"),
            ("protocol", "StyledRun", "layout value adapter"),
            ("protocol", "UiSlot", "layout value adapter"),
            // outbound theme framing — built from mode-owned ResolvedTheme; core
            // cannot construct it without depending on modes/interactive/theme.rs
            ("protocol", "ThemeCatalogEntry", "outbound theme framing"),
            ("protocol", "ThemeColorValue", "outbound theme framing"),
            ("protocol", "ThemeUpdate", "outbound theme framing"),
            ("protocol", "ThemeWire", "outbound theme framing"),
            // outbound terminal-input framing — built from crossterm events; core
            // has no crossterm/pi-tui dependency
            (
                "protocol",
                "KeyEventKindWire",
                "outbound terminal-input framing",
            ),
            (
                "protocol",
                "KeyModifiersWire",
                "outbound terminal-input framing",
            ),
            (
                "protocol",
                "UiEventRequest",
                "outbound terminal-input framing",
            ),
            ("protocol", "UiEventWire", "outbound terminal-input framing"),
            // shortcut registration — mode-local keybinding adapter
            ("adapters", "ShortcutRegistration", "keybinding adapter"),
            // sanitize boundary — SanitizedSlot is the declared layout-adapter currency
            ("sanitize", "SanitizedSlot", "sanitize boundary"),
            ("sanitize", "contains_control_bytes", "sanitize boundary"),
            ("sanitize", "sanitize_slot", "sanitize boundary"),
        ],
    ),
    (
        "interactive/state.rs",
        &[("sanitize", "SanitizedSlot", "sanitize boundary")],
    ),
    (
        "interactive/tests.rs",
        &[
            ("protocol", "OverlayAnchor", "layout value adapter (test)"),
            (
                "protocol",
                "OverlayMarginWire",
                "layout value adapter (test)",
            ),
            ("protocol", "OverlaySpec", "layout value adapter (test)"),
            ("protocol", "SizeValue", "layout value adapter (test)"),
            ("protocol", "SlotPlacement", "layout value adapter (test)"),
            ("protocol", "Style", "layout value adapter (test)"),
            ("protocol", "StyledRun", "layout value adapter (test)"),
            ("protocol", "UiSlot", "layout value adapter (test)"),
            ("sanitize", "sanitize_slot", "sanitize boundary (test)"),
        ],
    ),
    (
        "interactive/view.rs",
        &[
            ("adapters", "SlotComponent", "view adapter"),
            ("adapters", "tui_overlay_spec", "view adapter"),
        ],
    ),
    (
        "rpc/server.rs",
        &[
            // dialog lifecycle — ARC10 typed correlated path, owned by pi_ext::client
            ("client", "DialogEnd", "dialog lifecycle"),
            ("client", "DialogOutcome", "dialog lifecycle"),
            ("client", "HostUiRequest", "dialog lifecycle"),
            ("client", "HostUiResponse", "dialog lifecycle"),
            // test module only — fake host client
            ("client", "HostClient", "test-only fake host"),
            // layout adapter
            ("protocol", "SlotPlacement", "layout adapter"),
            // dialog request builders — test module only
            (
                "protocol",
                "ConfirmRequest",
                "dialog request builder (test)",
            ),
            ("protocol", "DialogOptions", "dialog request builder (test)"),
            ("protocol", "EditorRequest", "dialog request builder (test)"),
            ("protocol", "InputRequest", "dialog request builder (test)"),
            ("protocol", "SelectRequest", "dialog request builder (test)"),
            // non-event framing/codec — test module only
            ("protocol", "Frame", "codec (test)"),
            ("protocol", "FrameKind", "codec (test)"),
            ("protocol", "HelloAck", "codec (test)"),
            ("protocol", "Method", "codec (test)"),
            ("protocol", "decode_frame_str", "codec (test)"),
            ("protocol", "encode_frame", "codec (test)"),
        ],
    ),
];

/// Names that must never appear in mode code, with the remedy shown on failure.
/// Checked as bare identifiers so both `use pi_ext::protocol::UiControl` and a
/// stray fully-qualified `pi_ext::protocol::UiControl` are caught; the product
/// enum variants `ExtensionUiEvent::UiControl` / `ExtensionUiEvent::ThemeSet`
/// are exempted (they ARE the product projection).
const FORBIDDEN: &[(&str, &str)] = &[
    ("NotifyRequest", "use core::extension_host::ExtensionNotice"),
    (
        "NotifyLevel",
        "use core::extension_host::ExtensionNoticeLevel",
    ),
    ("UiControl", "use core::extension_host::ExtensionUiControl"),
    (
        "ThemeSet",
        "use core::extension_host::ExtensionThemeRequest",
    ),
    (
        "UiStateWire",
        "use ExtensionRuntimeSet::push_ui_state(text, expanded)",
    ),
    (
        "HostNotification",
        "modes consume ExtensionUiEvent, not raw host notifications",
    ),
];

fn is_ident_byte(b: u8) -> bool {
    // Rust identifiers may hold Unicode chars; any non-ASCII byte is
    // conservatively identifier content so a Unicode name cannot slip past a
    // boundary check or truncate a symbol.
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Parse every `pi_ext::` occurrence in `text` and yield `(module, symbol)`
/// pairs. Handles `use pi_ext::module::{A, B, C}` (one brace level, multi-line)
/// and inline `pi_ext::module::Symbol::method` (only `Symbol` is kept). `self`
/// is skipped. Whitespace around path separators is tolerated —
/// `pi_ext :: protocol :: ThemeWire` is valid Rust and must not bypass the
/// ledger. De-duplicates.

fn strip_lexical_noise(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for b in &mut out[from..to] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    };
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                let end = source[i..]
                    .find('\n')
                    .map_or(bytes.len(), |rel| i + rel + 1);
                blank(&mut out, i, end);
                i = end;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let mut depth = 1usize;
                let mut j = i + 2;
                while j < bytes.len() && depth > 0 {
                    if bytes[j] == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'*' {
                        depth += 1;
                        j += 2;
                    } else if bytes[j] == b'*' && j + 1 < bytes.len() && bytes[j + 1] == b'/' {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i, j);
                i = j;
            }
            b'r' if source[i..].starts_with("r\"") || source[i..].starts_with("r#") => {
                // raw string: r"..." or r#"..."# (hash count preserved)
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] == b'#' {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'"' {
                    let hashes = j - (i + 1);
                    let close = format!("\"{}", "#".repeat(hashes));
                    let end = source[j + 1..]
                        .find(&close)
                        .map_or(bytes.len(), |rel| j + 1 + rel + close.len());
                    blank(&mut out, i, end);
                    i = end;
                } else {
                    i += 1;
                }
            }
            b'"' => {
                let mut j = i + 1;
                while j < bytes.len() {
                    if bytes[j] == b'\\' {
                        j += 2;
                    } else if bytes[j] == b'"' {
                        j += 1;
                        break;
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i, j);
                i = j;
            }
            b'\'' => {
                // Char literal only when a closing quote appears within a
                // bounded window; a lifetime (`'a`, `'_`) has none nearby and
                // must not swallow the rest of the file.
                const MAX_CHAR_LEN: usize = 12;
                let limit = bytes.len().min(i + MAX_CHAR_LEN);
                let mut j = i + 1;
                let mut end = None;
                while j < limit {
                    if bytes[j] == b'\\' {
                        j += 2;
                    } else if bytes[j] == b'\'' {
                        end = Some(j + 1);
                        break;
                    } else {
                        j += 1;
                    }
                }
                if let Some(end) = end {
                    blank(&mut out, i, end);
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_owned())
}

fn parse_pi_ext_pairs(source: &str) -> Vec<(String, String)> {
    let text = &strip_lexical_noise(source);
    let bytes = text.as_bytes();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let needle = "pi_ext";
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(needle) {
        let start = from + rel;
        let mut j = start + needle.len();
        let skip_ws = |mut j: usize| -> usize {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            j
        };
        // `pi_ext` must be the path root. `my_pi_ext` (identifier before)
        // and `foo::pi_ext` / `foo :: pi_ext` (identifier before the `::`
        // separator, whitespace tolerated) are rejected; keywords like the
        // `use` in `use pi_ext` and the crate-root `::pi_ext` form are not
        let is_path_segment = |token: &str| {
            !token.is_empty()
                && !matches!(
                    token,
                    "as" | "async"
                        | "await"
                        | "break"
                        | "const"
                        | "continue"
                        | "crate"
                        | "dyn"
                        | "else"
                        | "enum"
                        | "extern"
                        | "false"
                        | "fn"
                        | "for"
                        | "if"
                        | "impl"
                        | "in"
                        | "let"
                        | "loop"
                        | "match"
                        | "mod"
                        | "move"
                        | "mut"
                        | "pub"
                        | "ref"
                        | "return"
                        | "self"
                        | "Self"
                        | "static"
                        | "struct"
                        | "super"
                        | "trait"
                        | "true"
                        | "try"
                        | "type"
                        | "union"
                        | "unsafe"
                        | "use"
                        | "where"
                        | "while"
                        | "yield"
                )
        };
        let mut qualified = false;
        if start > 0 {
            let mut k = start;
            while k > 0 && bytes[k - 1].is_ascii_whitespace() {
                k -= 1;
            }
            if k > 0 {
                let b = bytes[k - 1];
                if is_ident_byte(b) {
                    let mut lo = k;
                    while lo > 0 && is_ident_byte(bytes[lo - 1]) {
                        lo -= 1;
                    }
                    qualified = is_path_segment(&text[lo..k]);
                } else if b == b':' {
                    // walk the `::` run, then classify the token before it
                    let mut first = k - 1;
                    while first > 0 && bytes[first - 1].is_ascii_whitespace() {
                        first -= 1;
                    }
                    if first > 0 && bytes[first - 1] == b':' {
                        first -= 1;
                        let mut tok_end = first;
                        while tok_end > 0 && bytes[tok_end - 1].is_ascii_whitespace() {
                            tok_end -= 1;
                        }
                        let mut lo = tok_end;
                        if tok_end > 0 && is_ident_byte(bytes[tok_end - 1]) {
                            while lo > 0 && is_ident_byte(bytes[lo - 1]) {
                                lo -= 1;
                            }
                        }
                        qualified = is_path_segment(&text[lo..tok_end]);
                    }
                }
            }
        }
        if qualified || (j < bytes.len() && is_ident_byte(bytes[j])) {
            from = start + needle.len();
            continue;
        }
        // expect `::` with optional surrounding whitespace
        j = skip_ws(j);
        if !text[j..].starts_with("::") {
            from = start + needle.len();
            continue;
        }
        j = skip_ws(j + 2);
        // module identifier (raw `r#` prefix normalized away)
        if text[j..].starts_with("r#") {
            j += 2;
        }
        let mod_lo = j;
        while j < bytes.len() && is_ident_byte(bytes[j]) {
            j += 1;
        }
        if j == mod_lo {
            from = j;
            continue;
        }
        let module = text[mod_lo..j].to_owned();
        // expect `::` with optional surrounding whitespace
        let after_module = skip_ws(j);
        if !text[after_module..].starts_with("::") {
            from = j;
            continue;
        }
        j = skip_ws(after_module + 2);
        // symbol: brace group or single identifier (raw `r#` normalized)
        let read_ident = |mut j: usize| -> (usize, Option<&str>) {
            if text[j..].starts_with("r#") {
                j += 2;
            }
            let lo = j;
            while j < bytes.len() && is_ident_byte(bytes[j]) {
                j += 1;
            }
            (j, (j > lo).then(|| &text[lo..j]))
        };
        if j < bytes.len() && bytes[j] == b'{' {
            // find matching close brace
            let mut depth = 1usize;
            let mut k = j + 1;
            while k < bytes.len() && depth > 0 {
                match bytes[k] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                k += 1;
            }
            let inner = &text[j + 1..k.saturating_sub(1).max(j + 1)];
            for part in inner.split(',') {
                let part = part.trim_start();
                let base = part.as_ptr() as usize - text.as_ptr() as usize;
                let (_, name) = read_ident(base);
                if let Some(name) = name.filter(|name| *name != "self") {
                    let key = (module.clone(), name.to_string());
                    if seen.insert(key.clone()) {
                        out.push(key);
                    }
                }
            }
            from = k;
        } else {
            let (j_end, name) = read_ident(j);
            if let Some(name) = name.filter(|name| *name != "self") {
                let key = (module.clone(), name.to_string());
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            from = j_end;
        }
    }
    out
}

/// True if the identifier at `pos` is immediately preceded by
/// `ExtensionUiEvent ::` (the product enum variant), allowing optional
/// whitespace around `::`. Exempts `ExtensionUiEvent::UiControl` /
/// `ExtensionUiEvent::ThemeSet` from the forbidden-name check.
fn is_extension_ui_event_variant(text: &str, pos: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = pos;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i < 2 || bytes[i - 1] != b':' || bytes[i - 2] != b':' {
        return false;
    }
    i -= 2;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    let mut j = i;
    while j > 0 && is_ident_byte(bytes[j - 1]) {
        j -= 1;
    }
    if &text[j..i] != "ExtensionUiEvent" {
        return false;
    }
    // `ExtensionUiEvent` must be the path root: a preceding `::` separator
    // (whitespace tolerated) marks a qualified lookalike such as
    // `some_wrapper::ExtensionUiEvent::UiControl`, which is not the product
    // variant and must stay forbidden.
    if j > 0 {
        let mut k = j;
        while k > 0 && bytes[k - 1].is_ascii_whitespace() {
            k -= 1;
        }
        if k > 0 && bytes[k - 1] == b':' {
            k -= 1;
            while k > 0 && bytes[k - 1].is_ascii_whitespace() {
                k -= 1;
            }
            if k > 0 && bytes[k - 1] == b':' {
                return false;
            }
        }
    }
    true
}

/// Bare-identifier forbidden-name violations in `text`: word-boundary
/// occurrences not exempted as `ExtensionUiEvent` variants.
fn forbidden_violations(source: &str) -> Vec<(String, usize)> {
    let text = &strip_lexical_noise(source);
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for &(name, _remedy) in FORBIDDEN {
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(name) {
            let pos = from + rel;
            let end = pos + name.len();
            let before_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
            let after_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
            if before_ok && after_ok && !is_extension_ui_event_variant(text, pos) {
                out.push((name.to_string(), pos));
            }
            from = end;
        }
    }
    out
}

fn collect_rs_files(dir: &Path, base: &Path, out: &mut Vec<String>) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, base, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

fn ledger_for(rel: &str) -> Option<&'static [(&'static str, &'static str, &'static str)]> {
    LEDGER.iter().find(|(p, _)| *p == rel).map(|(_, v)| *v)
}

/// Find `pi_ext` followed by whitespace and the `as` keyword (an import
/// alias), tolerating any whitespace between tokens.
fn find_pi_ext_alias(source: &str) -> Option<usize> {
    let text = &strip_lexical_noise(source);
    let bytes = text.as_bytes();
    let needle = "pi_ext";
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(needle) {
        let start = from + rel;
        let mut j = start + needle.len();
        if j < bytes.len() && is_ident_byte(bytes[j]) {
            from = j;
            continue;
        }
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if text[j..].starts_with("as ")
            || (text[j..].starts_with("as")
                && !text[j + 2..].starts_with(|c: char| c.is_alphanumeric()))
        {
            return Some(start);
        }
        from = start + needle.len();
    }
    None
}

#[test]
fn modes_consume_only_ledgered_pi_ext_symbols() {
    let base = PathBuf::from(MODES_DIR);
    let mut files: Vec<String> = Vec::new();
    collect_rs_files(&base, &base, &mut files);
    files.sort();

    let mut failures: Vec<String> = Vec::new();

    for rel in &files {
        if rel == "wire_boundary.rs" {
            continue;
        }
        let path = base.join(rel);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));

        // forbidden bare-identifier check
        for (name, pos) in forbidden_violations(&text) {
            let line = text[..pos].matches('\n').count() + 1;
            let remedy = FORBIDDEN
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, r)| *r)
                .unwrap_or("");
            failures.push(format!(
                "{rel}:{line}: FORBIDDEN `{name}` in mode code — {remedy}"
            ));
        }

        // an alias would route every later use around the `pi_ext::` scan;
        // `use pi_ext\nas ext;` is valid Rust, so tolerate any whitespace
        if let Some(pos) = find_pi_ext_alias(&text) {
            let line = text[..pos].matches('\n').count() + 1;
            failures.push(format!(
                "{rel}:{line}: `pi_ext as ...` is forbidden — aliasing would bypass the ledger; import the specific ledgered symbols instead"
            ));
        }
        // ledger bidirectional check
        let observed: std::collections::BTreeSet<(String, String)> =
            parse_pi_ext_pairs(&text).into_iter().collect();
        let expected: std::collections::BTreeSet<(String, String)> = ledger_for(rel)
            .map(|entries| {
                entries
                    .iter()
                    .map(|(m, s, _)| (m.to_string(), s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        for (m, s) in observed.difference(&expected) {
            failures.push(format!(
                "{rel}: extra pi_ext::{m}::{s} not in LEDGER (add it with a reason or use the product projection)"
            ));
        }
        for (m, s) in expected.difference(&observed) {
            failures.push(format!(
                "{rel}: LEDGER lists pi_ext::{m}::{s} but it no longer appears (remove the stale entry)"
            ));
        }
    }

    // ledger files that vanished from the tree
    for (rel, _) in LEDGER {
        if !files.iter().any(|f| f == rel) {
            failures.push(format!(
                "{rel}: LEDGER references a file not found under src/modes"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "ARC12 wire boundary violated:\n{}\n",
        failures.join("\n")
    );
}

#[test]
fn scanner_flags_pi_ext_alias_regardless_of_whitespace() {
    for src in [
        "use pi_ext as ext;\nfn f() { ext::protocol::ThemeWire }\n",
        "use pi_ext\nas ext;\n",
        "use pi_ext\tas\n ext;\n",
    ] {
        assert!(
            find_pi_ext_alias(src).is_some(),
            "alias not detected in {src:?}"
        );
    }
    // plain imports must not trip the alias detector
    assert_eq!(
        find_pi_ext_alias("use pi_ext::protocol::ThemeWire;\n"),
        None
    );
    assert_eq!(find_pi_ext_alias("use other as pi_ext;\n"), None);
}

#[test]
fn scanner_flags_a_planted_violation() {
    let src = "use pi_ext::protocol::UiControl;\nfn f() { let _ = pi_ext::protocol::{ThemeSet, NotifyLevel}; }\nuse pi_ext :: protocol :: ThemeWire;\n";
    let pairs = parse_pi_ext_pairs(src);
    let got: std::collections::BTreeSet<(String, String)> = pairs.into_iter().collect();
    for expected in [
        ("protocol", "UiControl"),
        ("protocol", "ThemeSet"),
        ("protocol", "NotifyLevel"),
        ("protocol", "ThemeWire"),
    ] {
        assert!(
            got.contains(&(expected.0.to_string(), expected.1.to_string())),
            "scanner did not yield pi_ext::{}::{} from synthetic source; got {:?}",
            expected.0,
            expected.1,
            got
        );
    }
    // the forbidden bare-name check must also flag all three
    let fv = forbidden_violations(src);
    let fv_names: std::collections::BTreeSet<String> = fv.into_iter().map(|(n, _)| n).collect();
    for name in ["UiControl", "ThemeSet", "NotifyLevel"] {
        assert!(
            fv_names.contains(name),
            "forbidden_violations did not flag `{name}`"
        );
    }
}

#[test]
fn scanner_ignores_lexical_noise_and_non_root_paths() {
    // strings, comments, and doc text must not create observations
    let noisy = concat!(
        "let s = \"pi_ext::protocol::ThemeWire\";\n",
        "// pi_ext::protocol::Frame\n",
        "/* pi_ext::sanitize::sanitize_slot */\n",
        "fn g<'a>(x: &'a str) { let _ = x; }\n",
        "use my_pi_ext::protocol::Frame2;\n",
        "use foo::pi_ext::protocol::Frame3;\n",
    );
    assert!(
        parse_pi_ext_pairs(noisy).is_empty(),
        "noise/non-root paths must yield nothing: {:?}",
        parse_pi_ext_pairs(noisy)
    );
    // a lifetime early in the file must not erase a later real import
    let lifetime_then_import = "fn f<'a>() {} use pi_ext::protocol::Frame;\n";
    assert!(
        parse_pi_ext_pairs(lifetime_then_import)
            .contains(&("protocol".to_owned(), "Frame".to_owned()))
    );
    // char literals are blanked but adjacent imports survive
    let char_then_import = "let c = 'x'; use pi_ext::client::HostUiRequest;\n";
    assert!(
        parse_pi_ext_pairs(char_then_import)
            .contains(&("client".to_owned(), "HostUiRequest".to_owned()))
    );
}

#[test]
fn scanner_parses_raw_and_unicode_identifiers() {
    // raw-identifier module and symbol normalize to their plain names
    let src = "use pi_ext::r#protocol::r#ThemeWire;\n";
    assert_eq!(
        parse_pi_ext_pairs(src),
        vec![("protocol".to_owned(), "ThemeWire".to_owned())]
    );
    // raw symbol inside a brace group normalizes too
    let src = "use pi_ext::protocol::{r#ThemeWire, Frame};\n";
    assert!(parse_pi_ext_pairs(src).contains(&("protocol".to_owned(), "ThemeWire".to_owned())));
    assert!(parse_pi_ext_pairs(src).contains(&("protocol".to_owned(), "Frame".to_owned())));
    // a Unicode symbol is captured whole and must be ledgered to pass
    let src = "use pi_ext::protocol::ÜiControl;\n";
    assert_eq!(
        parse_pi_ext_pairs(src),
        vec![("protocol".to_owned(), "ÜiControl".to_owned())]
    );
    // a Unicode-prefixed root is not pi_ext
    assert!(parse_pi_ext_pairs("use Üpi_ext::protocol::Frame;\n").is_empty());
}

#[test]
fn scanner_distinguishes_crate_root_from_qualified_paths() {
    // Crate-root `::pi_ext` (and its spaced spelling) is a genuine root and
    // must be observed; qualified `foo::pi_ext` / `foo :: pi_ext` forms are
    // not roots and must not be observed.
    for src in [
        "use ::pi_ext::protocol::Frame;\n",
        "use :: pi_ext::protocol::Frame;\n",
    ] {
        assert_eq!(
            parse_pi_ext_pairs(src),
            vec![("protocol".to_owned(), "Frame".to_owned())],
            "crate-root form must be observed: {src:?}"
        );
    }
    for src in [
        "use foo::pi_ext::protocol::Frame;\n",
        "use foo :: pi_ext::protocol::Frame;\n",
    ] {
        assert!(
            parse_pi_ext_pairs(src).is_empty(),
            "qualified path must not be observed as pi_ext root: {src:?}"
        );
    }
}

#[test]
fn scanner_exemptions_require_the_product_path_root() {
    // qualified lookalikes are not the product variant, in adjacent and
    // spaced `::` spellings: the forbidden names must be flagged
    for lookalike in [
        "use some_wrapper::ExtensionUiEvent::UiControl;\n",
        "use other :: ExtensionUiEvent :: ThemeSet;\n",
    ] {
        assert!(
            !forbidden_violations(lookalike).is_empty(),
            "qualified ExtensionUiEvent lookalike must be flagged: {lookalike:?}"
        );
    }
    // the canonical product-variant spelling stays exempt
    let canonical = "let e = ExtensionUiEvent::UiControl;\n";
    assert!(
        forbidden_violations(canonical).is_empty(),
        "canonical ExtensionUiEvent variants must stay exempt"
    );
}
