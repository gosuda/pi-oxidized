/**
 * Hostile fixture: emits forbidden CSI/OSC/DCS/APC sequences, private SGR,
 * split escapes, oversized hyperlinks, and non-http OSC 8 URIs.
 *
 * The sanitizer must drop ALL of these, leaving only allowlisted text and
 * structured styles. Plugin bytes never reach stdout.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function hostileExtension(pi: ExtensionAPI): void {
	pi.on("session_start", (_event, ctx) => {
		if (!ctx.hasUI) return;
		ctx.ui.setWidget("widget.hostile", [
			// CSI cursor movement (should be dropped).
			"\x1b[2J\x1b[H\x1b[5;10Hsafe",
			// DCS string (should be consumed entirely).
			"\x1bP1q#1!2\x1b\\clean",
			// APC string (should be consumed entirely).
			"\x1b_Gs=1\x1b\\ok",
			// OSC clipboard/title (should be dropped).
			"\x1b]52;c;Zm9v\x07\x1b]0;title\x07text",
			// OSC 8 with javascript: URI (should be dropped).
			"\x1b]8;;javascript:alert(1)\x07bad\x1b]8;;\x07",
			// OSC 8 with file: URI (should be dropped).
			"\x1b]8;;file:///etc/passwd\x07bad\x1b]8;;\x07",
			// Private SGR parameter (font select 10, should be ignored, text kept).
			"\x1b[10m\x1b[31mred\x1b[0m",
			// C0 controls embedded in text.
			"a\x00\x01\x07\x08b",
			// DEC private mode (synchronized output, should be dropped).
			"\x1b[?2026h\x1b[?2026lvisible",
			// Valid SGR that SHOULD pass through.
			"\x1b[1;32mgreen-bold\x1b[0m",
			// Valid OSC 8 http link.
			"\x1b]8;id=abc;https://example.com\x07link\x1b]8;;\x07",
		]);
	});
}
