//! Success/error HTML pages shown by the OAuth loopback callback server.

const LOGO_SVG: &str = concat!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800" aria-hidden="true">"##,
    r##"<path fill="#fff" fill-rule="evenodd" d="M165.29 165.29 H517.36 V400 H400 V517.36 H282.65 V634.72 H165.29 Z M282.65 282.65 V400 H400 V282.65 Z"/>"##,
    r##"<path fill="#fff" d="M517.36 400 H634.72 V634.72 H517.36 Z"/></svg>"##
);

/// Escape text for safe inclusion in HTML element content and attributes.
#[must_use]
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let title = escape_html(title);
    let heading = escape_html(heading);
    let message = escape_html(message);
    let details_html = details.map_or_else(String::new, |details| {
        format!(r#"    <div class="details">{}</div>"#, escape_html(details))
    });

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{title}</title>
  <style>
    :root {{
      --text: #fafafa;
      --text-dim: #a1a1aa;
      --page-bg: #09090b;
      --font-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
    }}
    * {{ box-sizing: border-box; }}
    html {{ color-scheme: dark; }}
    body {{
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 24px;
      background: var(--page-bg);
      color: var(--text);
      font-family: var(--font-sans);
      text-align: center;
    }}
    main {{
      width: 100%;
      max-width: 560px;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
    }}
    .logo {{
      width: 72px;
      height: 72px;
      display: block;
      margin-bottom: 24px;
    }}
    h1 {{
      margin: 0 0 10px;
      font-size: 28px;
      line-height: 1.15;
      font-weight: 650;
      color: var(--text);
    }}
    p {{
      margin: 0;
      line-height: 1.7;
      color: var(--text-dim);
      font-size: 15px;
    }}
    .details {{
      margin-top: 16px;
      font-family: var(--font-mono);
      font-size: 13px;
      color: var(--text-dim);
      white-space: pre-wrap;
      word-break: break-word;
    }}
  </style>
</head>
<body>
  <main>
    <div class="logo">{LOGO_SVG}</div>
    <h1>{heading}</h1>
    <p>{message}</p>
{details_html}
  </main>
</body>
</html>"#
    )
}

/// HTML page shown after a successful OAuth callback.
#[must_use]
pub fn oauth_success_html(message: &str) -> String {
    render_page(
        "Authentication successful",
        "Authentication successful",
        message,
        None,
    )
}

/// HTML page shown after a failed OAuth callback.
#[must_use]
pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
    render_page(
        "Authentication failed",
        "Authentication failed",
        message,
        details,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_covers_all_special_chars() {
        assert_eq!(escape_html(r#"&<>"'x"#), "&amp;&lt;&gt;&quot;&#39;x");
    }

    #[test]
    fn success_page_escapes_message_and_sets_title() {
        let html = oauth_success_html(r#"Done <script>alert("x")</script>"#);
        assert!(html.contains("<title>Authentication successful</title>"));
        assert!(html.contains("Done &lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert"));
        assert!(html.contains(LOGO_SVG));
    }

    #[test]
    fn error_page_escapes_details() {
        let html = oauth_error_html("nope", Some(r#"err <b>&'""#));
        assert!(html.contains("<title>Authentication failed</title>"));
        assert!(html.contains("err &lt;b&gt;&amp;&#39;&quot;"));
        assert!(html.contains(r#"class="details""#));
    }
}
