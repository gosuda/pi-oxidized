/**
 * ANSI sanitizer: tokenizes a rendered ANSI string into allowlisted structured
 * {@link StyledRun} arrays, starting from a fresh ground-state parser on every
 * call so incomplete escape state never carries across pushes or generations.
 *
 * Mirrors the Rust validation boundary in `crates/pi-ext/src/protocol.rs`.
 * Plugin bytes never reach stdout: only validated structured runs cross the
 * wire. Everything not on the SGR/color/OSC-8/printable allowlist is silently
 * dropped, and the parser is always left in ground state when a line ends.
 */

import type {
	Hyperlink,
	NamedColor,
	Style,
	StyledRun,
	WireColor,
} from "./protocol.ts";

/** Maximum OSC 8 hyperlink id length in bytes. */
export const MAX_HYPERLINK_ID_BYTES = 128;
/** Maximum OSC 8 hyperlink URI length in bytes. */
export const MAX_HYPERLINK_URI_BYTES = 2048;
/** Named palette accepted on the wire, in stable order. */
const NAMED_COLORS: readonly NamedColor[] = [
	"black",
	"red",
	"green",
	"yellow",
	"blue",
	"magenta",
	"cyan",
	"white",
	"brightBlack",
	"brightRed",
	"brightGreen",
	"brightYellow",
	"brightBlue",
	"brightMagenta",
	"brightCyan",
	"brightWhite",
] as const;

const NAMED_SET: ReadonlySet<string> = new Set(NAMED_COLORS as readonly string[]);

/**
 * Parse one ANSI line into a list of structured runs.
 *
 * A brand-new parser is constructed on every invocation, so any truncated
 * escape sequence at the end of one line is discarded before the next line
 * begins. Embedded newlines split the input into separate lines.
 *
 * Accepted:
 * - Printable graphemes (and spaces).
 * - SGR sequences: reset (0), emphasis (1/2/3/4/7/9), 8/16/256/truecolor fg/bg
 *   (3x/4x/38;5;n/48;5;n/38;2;r;g;b/48;2;r;g;b), and foreground/background reset
 *   (39/49). Unknown/private SGR parameters are ignored without dropping text.
 * - OSC 8 hyperlinks (`ESC ] 8 ; <params> ; <uri> ST`) with http/https URIs.
 *
 * Rejected (silently dropped, parser resets to ground state):
 * - Non-SGR CSI finals (cursor movement, erase, clear, scrolling, etc.).
 * - DEC private modes (CSI ? ...), synchronized-output markers, clipboard/title
 *   OSC, DCS, APC, and all other C1 control sequences.
 * - Private/unknown SGR parameter values.
 * - `javascript:`/`file:`/non-http(s) OSC 8 URIs; oversized id/uri fields.
 * - All C0 control characters except the SGR/OSC introducers themselves.
 */
export function parseAnsiLine(input: string): StyledRun[] {
	const runs: StyledRun[] = [];
	const state = new ParserState();
	let text = "";
	let i = 0;
	const len = input.length;
	const flushText = () => {
		if (text.length > 0) {
			runs.push(makeRun(text, state.snapshot()));
			text = "";
		}
	};

	while (i < len) {
		const code = input.charCodeAt(i);

		if (code === CC.ESC) {
			flushText();
			const consumed = state.feedEscape(input, i);
			if (consumed > 0) {
				i += consumed;
				continue;
			}
			// Stray ESC with no recognized follower: drop the ESC byte, keep going.
			i += 1;
			continue;
		}

		if (code === CC.BEL) {
			// BEL terminates OSC; handled inside feedEscape when preceded by OSC.
			// A stray BEL outside an escape is a control char — drop it.
			i += 1;
			continue;
		}

		if (code < CC.SPACE || code === CC.DEL) {
			// C0 controls and DEL (0x7f) are dropped; they never reach stdout.
			// Tabs are expanded by parseAnsiLines before entering here.
			i += 1;
			continue;
		}

		// Printable (including DEL and above — grapheme clustering is the host's
		// concern; we pass through code points verbatim).
		text += input[i];
		i += 1;
	}

	flushText();
	return runs;
}

/**
 * Parse a multi-line ANSI string into one run-list per visual line.
 * Embedded `\n` and `\r\n` split lines. Tabs expand to a single space (the
 * Rust side re-expands to the configured tab stop if needed).
 */
export function parseAnsiLines(input: string): StyledRun[][] {
	if (input.length === 0) {
		return [[]];
	}
	// Normalize CRLF and CR to LF, then split.
	const normalized = input.replace(/\r\n?/g, "\n");
	const lines = normalized.split("\n");
	return lines.map((line) => parseAnsiLine(line.replace(/\t/g, " ")));
}

// ---------------------------------------------------------------------------
// Parser state machine
// ---------------------------------------------------------------------------

/** C0 control codes we care about. */
const enum CC {
	NUL = 0x00,
	ESC = 0x1b,
	BEL = 0x07,
	SPACE = 0x20,
	DEL = 0x7f,
}

/** Emphasis attribute SGR parameter -> style flag. */
const EMPHASIS_PARAM: Record<number, keyof Style> = {
	1: "bold",
	2: "dim",
	3: "italic",
	4: "underline",
	7: "reverse",
	9: "strikethrough",
};

class ParserState {
	bold = false;
	dim = false;
	italic = false;
	underline = false;
	reverse = false;
	strikethrough = false;
	fg: WireColor | undefined;
	bg: WireColor | undefined;
	link: Hyperlink | undefined;

	reset(): void {
		this.bold = false;
		this.dim = false;
		this.italic = false;
		this.underline = false;
		this.reverse = false;
		this.strikethrough = false;
		this.fg = undefined;
		this.bg = undefined;
		this.link = undefined;
	}

	snapshot(): Style {
		const style: Style = {};
		if (this.bold) style.bold = true;
		if (this.dim) style.dim = true;
		if (this.italic) style.italic = true;
		if (this.underline) style.underline = true;
		if (this.reverse) style.reverse = true;
		if (this.strikethrough) style.strikethrough = true;
		if (this.fg !== undefined) style.fg = this.fg;
		if (this.bg !== undefined) style.bg = this.bg;
		if (this.link !== undefined) style.link = this.link;
		return style;
	}

	/**
	 * Feed an escape sequence starting at `input[start]` (the ESC byte).
	 * Returns the number of characters consumed (including ESC), or 0 if the
	 * ESC has no recognized follower. Always leaves the parser in a consistent
	 * state.
	 */
	feedEscape(input: string, start: number): number {
		const next = start + 1;
		if (next >= input.length) {
			return 0;
		}
		const kind = input.charCodeAt(next);

		if (kind === CC_CODE("[") || kind === 0x9b) {
			// CSI: ESC [ params final   (also 8-bit CSI 0x9b)
			// feedCsi returns chars from `[` onward; add 1 for the ESC byte.
			return this.feedCsi(input, next) + 1;
		}
		if (kind === CC_CODE("]")) {
			// OSC: ESC ] ... ST
			return this.feedOsc(input, next) + 1;
		}
	// DCS (P), SOS (X), PM (^), APC (_) — string-type C1 controls.
	// Consume the entire string body up to ST (ESC \) or BEL so it never
	// leaks as text. These are always rejected.
	if (
		kind === CC_CODE("P") || kind === CC_CODE("X") ||
		kind === CC_CODE("^") || kind === CC_CODE("_")
	) {
		return this.feedDroppedString(input, next) + 1;
	}
	// All other ESC followers (SS2 N, SS3 O, RIS c, IND D, NEL E, etc.):
	// consume the introducer and its single-byte follower.
	return 2;
	}

	/** Feed a CSI sequence starting at the `[` (or 8-bit CSI byte) at `bracket`. */
	private feedCsi(input: string, bracket: number): number {
		let i = bracket + 1;
		const len = input.length;
		// Optional intermediate '?' (DEC private) — we reject all DEC private modes.
		let privateMarker = false;
		if (i < len && input.charCodeAt(i) === CC_CODE("?")) {
			privateMarker = true;
			i += 1;
		}
		// Parameter bytes: 0x30..0x3f  (0-9 : ; < = > ?)
		const paramStart = i;
		while (i < len) {
			const c = input.charCodeAt(i);
			if (c >= 0x30 && c <= 0x3f) {
				i += 1;
			} else {
				break;
			}
		}
	// Intermediate bytes: 0x20..0x2f (space ! " # $ % & ' ( ) * + , - . /)
	while (i < len) {
		const c = input.charCodeAt(i);
		if (c >= 0x20 && c <= 0x2f) {
			i += 1;
		} else {
			break;
		}
	}
	if (i >= len || input.charCodeAt(i) < 0x40 || input.charCodeAt(i) > 0x7e) {
		// Truncated or malformed CSI (no valid final byte 0x40..0x7e).
		// Drop consumed bytes but do NOT eat the invalid byte itself — it may
		// be the start of a new escape (e.g. an embedded ESC).
		return i - bracket;
	}
	const finalByte = input.charCodeAt(i);
	const consumed = i + 1 - bracket;

		// Only SGR (final 'm') with NO DEC private marker and NO intermediates is
		// accepted. Everything else (cursor, erase, scroll, DEC modes, sync output)
		// is dropped without touching state.
		if (privateMarker || finalByte !== CC_CODE("m")) {
			return consumed;
		}
		const paramText = input.slice(paramStart, i);
		this.applySgr(paramText);
		return consumed;
	}

	/** Feed an OSC string starting at the `]` at `bracket`. */
	private feedOsc(input: string, bracket: number): number {
		// OSC = ESC ] <data> (BEL | ST).  ST = ESC \.
		const len = input.length;
		let i = bracket + 1;
		let data = "";
		while (i < len) {
			const c = input.charCodeAt(i);
			if (c === CC.BEL) {
				// BEL terminator.
				this.applyOsc(data);
				return i + 1 - bracket;
			}
			if (c === CC.ESC && i + 1 < len && input.charCodeAt(i + 1) === CC_CODE("\\")) {
				// ST = ESC \
				this.applyOsc(data);
				return i + 2 - bracket;
			}
			if (c === CC.NUL) {
				// Embedded NUL — skip.
				i += 1;
				continue;
			}
			if (c < CC.SPACE) {
				// Other control char inside OSC — terminate the OSC early.
				this.applyOsc(data);
				return i - bracket;
			}
		data += input[i];
		i += 1;
	}
	// Unterminated OSC (ran off the end) — drop it; ground state.
	return len - bracket;
}
	/** Consume a DCS/SOS/PM/APC string body up to ST (ESC \) or BEL. */
	private feedDroppedString(input: string, bracket: number): number {
		const len = input.length;
		let i = bracket + 1;
		while (i < len) {
			const c = input.charCodeAt(i);
			if (c === CC.BEL) {
				return i + 1 - bracket;
			}
			if (c === CC.ESC && i + 1 < len && input.charCodeAt(i + 1) === CC_CODE("\\")) {
				return i + 2 - bracket;
			}
			if (c === CC.ESC) {
				// Lone ESC inside a string: stop here; the outer loop will
				// re-enter feedEscape for the new sequence.
				return i - bracket;
			}
			i += 1;
		}
		return len - bracket;
	}

	private applySgr(paramText: string): void {
		if (paramText.length === 0) {
			this.reset();
			return;
		}
		const parts = paramText.split(";");
		// Empty parameter string (just "m") already handled above.
		const params: number[] = [];
		for (const part of parts) {
			if (part === "") {
				params.push(0);
			} else {
				const n = Number.parseInt(part, 10);
				if (Number.isNaN(n) || n < 0 || n > 0xffff) {
					// Malformed/private parameter — abort this SGR, keep state.
					return;
				}
				params.push(n);
			}
		}
		let k = 0;
		while (k < params.length) {
			const p = params[k];
			if (p === undefined) {
				k += 1;
				continue;
			}
			if (p === 0) {
				this.reset();
			} else if (p === 39) {
				this.fg = undefined;
			} else if (p === 49) {
				this.bg = undefined;
			} else if (EMPHASIS_PARAM[p] !== undefined) {
				const flag = EMPHASIS_PARAM[p];
				if (flag !== undefined) (this as Record<string, unknown>)[flag] = true;
			} else if (p >= 30 && p <= 37) {
				this.fg = namedColor(p - 30);
			} else if (p >= 40 && p <= 47) {
				this.bg = namedColor(p - 40);
			} else if (p >= 90 && p <= 97) {
				this.fg = namedColor(p - 90 + 8);
			} else if (p >= 100 && p <= 107) {
				this.bg = namedColor(p - 100 + 8);
			} else if (p === 38 || p === 48) {
				const color = this.parseExtendedColor(params, k);
				if (color !== undefined) {
					if (p === 38) this.fg = color.value;
					else this.bg = color.value;
					k = color.next;
					continue;
				} else {
					// Malformed extended color — stop processing this SGR.
					return;
				}
			}
			// All other parameters (5, 8, hidden, font select, framed, etc.)
			// are silently ignored without dropping subsequent parameters.
			k += 1;
		}
	}

	private parseExtendedColor(
		params: number[],
		k: number,
	): { value: WireColor; next: number } | undefined {
		const mode = params[k + 1];
		if (mode === 5) {
			// 256-color: 38;5;n  /  48;5;n
			const idx = params[k + 2];
			if (idx === undefined || idx < 0 || idx > 255) {
				return undefined;
			}
			return { value: { type: "indexed", index: idx }, next: k + 3 };
		}
		if (mode === 2) {
			// truecolor: 38;2;r;g;b  /  48;2;r;g;b
			const r = params[k + 2];
			const g = params[k + 3];
			const b = params[k + 4];
			if (
				r === undefined || g === undefined || b === undefined ||
				r < 0 || r > 255 || g < 0 || g > 255 || b < 0 || b > 255
			) {
				return undefined;
			}
			return { value: { type: "rgb", r, g, b }, next: k + 5 };
		}
		return undefined;
	}

	private applyOsc(data: string): void {
		// OSC 8 hyperlink: 8;<params>;<uri>
		// Any other OSC (0/1/2 title, 4 palette, 10 color, 52 clipboard, etc.)
		// is dropped — only hyperlinks are allowlisted.
		if (!data.startsWith("8;")) {
			return;
		}
		const rest = data.slice(2);
		// params and uri are separated by ';'. The params may themselves contain
		// '=' separated key=value pairs; we only extract the optional id.
		const semi = rest.indexOf(";");
		if (semi < 0) {
			// OSC 8 with no URI terminator: treat the whole remainder as params,
			// no link target — ignore.
			return;
		}
		const paramsField = rest.slice(0, semi);
		const uri = rest.slice(semi + 1);

		// Extract optional id=ID from params.
		let id: string | undefined;
		for (const piece of paramsField.split(":")) {
			if (piece.startsWith("id=")) {
				id = piece.slice(3);
				break;
			}
		}
		if (id !== undefined) {
			if (byteLength(id) > MAX_HYPERLINK_ID_BYTES) {
				return;
			}
		}
		if (byteLength(uri) > MAX_HYPERLINK_URI_BYTES) {
			return;
		}
		// Only http/https URIs are accepted.
		if (!/^https?:\/\//i.test(uri)) {
			return;
		}
		const link: Hyperlink = { uri };
		if (id !== undefined && id.length > 0) link.id = id;
		if (uri.length === 0) {
			// OSC 8 close: id=;  uri empty -> clear link.
			this.link = undefined;
			return;
		}
		this.link = link;
	}
}

function CC_CODE(ch: string): number {
	return ch.charCodeAt(0);
}

function namedColor(index: number): WireColor {
	const name = NAMED_COLORS[index];
	if (name !== undefined && NAMED_SET.has(name)) {
		return { type: "named", name };
	}
	// Unreachable for 0..15.
	return { type: "indexed", index };
}

function makeRun(text: string, style: Style): StyledRun {
	if (Object.keys(style).length === 0) {
		return { text };
	}
	return { text, style };
}

function byteLength(value: string): number {
	if (typeof TextEncoder !== "undefined") {
		return new TextEncoder().encode(value).byteLength;
	}
	// Fallback: approximate (caller already validated ASCII-ish paths).
	return value.length;
}
