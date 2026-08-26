import { describe, expect, test } from "bun:test";
import { parseAnsiLine, parseAnsiLines } from "../src/sanitize.ts";
import type { StyledRun, Style } from "../src/protocol.ts";

const ESC = "\x1b";
const ST = `${ESC}\\`;
const BEL = "\x07";

function text(runs: StyledRun[]): string {
	return runs.map((r) => r.text).join("");
}

function styleOf(runs: StyledRun[], idx: number): Style | undefined {
	return runs[idx]?.style;
}

describe("sanitize: printable passthrough", () => {
	test("plain text unchanged", () => {
		expect(text(parseAnsiLine("hello world"))).toBe("hello world");
	});

	test("empty string produces empty array", () => {
		expect(parseAnsiLine("")).toEqual([]);
	});

	test("unicode graphemes pass through", () => {
		expect(text(parseAnsiLine("héllo 日本 🎉"))).toBe("héllo 日本 🎉");
	});

	test("tabs expand to single space", () => {
		expect(text(parseAnsiLines("a\tb")[0]!)).toBe("a b");
	});

	test("CRLF and LF split lines", () => {
		const lines = parseAnsiLines("a\nb\r\nc\rd");
		expect(lines).toHaveLength(4);
		expect(text(lines[0]!)).toBe("a");
		expect(text(lines[3]!)).toBe("d");
	});
});

describe("sanitize: SGR emphasis", () => {
	test("bold reset clears emphasis", () => {
		const runs = parseAnsiLine(`${ESC}[1mbold${ESC}[0m plain`);
		expect(text(runs)).toBe("bold plain");
		expect(styleOf(runs, 0)).toMatchObject({ bold: true });
		expect(styleOf(runs, 1)).toBeUndefined();
	});

	test("all emphasis flags", () => {
		for (const [param, flag] of [
			["1", "bold"], ["2", "dim"], ["3", "italic"],
			["4", "underline"], ["7", "reverse"], ["9", "strikethrough"],
		] as const) {
			const runs = parseAnsiLine(`${ESC}[${param}mx${ESC}[0m`);
			expect(styleOf(runs, 0)).toMatchObject({ [flag]: true });
		}
	});

	test("multiple attributes in one SGR", () => {
		const runs = parseAnsiLine(`${ESC}[1;3;4mbold italic underline${ESC}[0m`);
		expect(styleOf(runs, 0)).toMatchObject({
			bold: true, italic: true, underline: true,
		});
	});
});

describe("sanitize: colors", () => {
	test("named foreground 30-37", () => {
		const runs = parseAnsiLine(`${ESC}[31mred${ESC}[0m`);
		expect(styleOf(runs, 0)).toMatchObject({ fg: { type: "named", name: "red" } });
	});

	test("bright named 90-97", () => {
		const runs = parseAnsiLine(`${ESC}[91mbrightRed${ESC}[0m`);
		expect(styleOf(runs, 0)).toMatchObject({ fg: { type: "named", name: "brightRed" } });
	});

	test("256-color indexed", () => {
		const runs = parseAnsiLine(`${ESC}[38;5;196mx${ESC}[0m`);
		expect(styleOf(runs, 0)).toMatchObject({ fg: { type: "indexed", index: 196 } });
	});

	test("truecolor rgb", () => {
		const runs = parseAnsiLine(`${ESC}[38;2;10;20;30mx${ESC}[0m`);
		expect(styleOf(runs, 0)).toMatchObject({ fg: { type: "rgb", r: 10, g: 20, b: 30 } });
	});

	test("background color", () => {
		const runs = parseAnsiLine(`${ESC}[42;30mbg green fg black${ESC}[0m`);
		expect(styleOf(runs, 0)).toMatchObject({
			fg: { type: "named", name: "black" },
			bg: { type: "named", name: "green" },
		});
	});

	test("fg/bg reset 39/49", () => {
		const runs = parseAnsiLine(`${ESC}[31m${ESC}[39mreset-fg`);
		expect(runs[0]?.style?.fg).toBeUndefined();
	});
});

describe("sanitize: OSC 8 hyperlinks", () => {
	test("basic hyperlink with BEL terminator", () => {
		const runs = parseAnsiLine(`${ESC}]8;;https://example.com${BEL}link${ESC}]8;;${BEL}`);
		expect(text(runs)).toBe("link");
		expect(styleOf(runs, 0)).toMatchObject({ link: { uri: "https://example.com" } });
	});

	test("hyperlink with ST terminator", () => {
		const runs = parseAnsiLine(`${ESC}]8;;https://x.com${ST}x${ESC}]8;;${ST}`);
		expect(styleOf(runs, 0)).toMatchObject({ link: { uri: "https://x.com" } });
	});

	test("hyperlink with id", () => {
		const runs = parseAnsiLine(`${ESC}]8;id=abc;https://y.com${ST}x${ESC}]8;;${ST}`);
		expect(styleOf(runs, 0)).toMatchObject({ link: { id: "abc", uri: "https://y.com" } });
	});

	test("javascript URI rejected", () => {
		const runs = parseAnsiLine(`${ESC}]8;;javascript:alert(1)${BEL}x${ESC}]8;;${BEL}`);
		expect(text(runs)).toBe("x");
		expect(runs[0]?.style?.link).toBeUndefined();
	});

	test("file URI rejected", () => {
		const runs = parseAnsiLine(`${ESC}]8;;file:///etc/passwd${BEL}x${ESC}]8;;${BEL}`);
		expect(runs[0]?.style?.link).toBeUndefined();
	});

	test("non-http scheme rejected", () => {
		const runs = parseAnsiLine(`${ESC}]8;;ftp://evil${BEL}x${ESC}]8;;${BEL}`);
		expect(runs[0]?.style?.link).toBeUndefined();
	});
});

describe("sanitize: hostile sequences dropped", () => {
	test("CSI 2J (clear screen) dropped", () => {
		const runs = parseAnsiLine(`${ESC}[2Jtext`);
		expect(text(runs)).toBe("text");
	});

	test("CSI 3J (clear scrollback) dropped", () => {
		const runs = parseAnsiLine(`${ESC}[3Jtext`);
		expect(text(runs)).toBe("text");
	});

	test("CSI H (cursor home) dropped", () => {
		const runs = parseAnsiLine(`${ESC}[1;1Htext`);
		expect(text(runs)).toBe("text");
	});

	test("CSI K (erase line) dropped", () => {
		const runs = parseAnsiLine(`${ESC}[2Ktext`);
		expect(text(runs)).toBe("text");
	});

	test("DEC private mode (?...h/l) dropped", () => {
		const runs = parseAnsiLine(`${ESC}[?2026htext${ESC}[?2026l`);
		expect(text(runs)).toBe("text");
		expect(runs[0]?.style).toBeUndefined();
	});

	test("synchronized-output markers have no effect", () => {
		const runs = parseAnsiLine(`${ESC}[?2026h${ESC}[1mbold${ESC}[?2026l`);
		expect(styleOf(runs, 0)).toMatchObject({ bold: true });
	});

	test("DCS string body consumed to ST", () => {
		const runs = parseAnsiLine(`${ESC}Pmalicious${ST}clean`);
		expect(text(runs)).toBe("clean");
	});

	test("APC string body consumed", () => {
		const runs = parseAnsiLine(`${ESC}_hidden${ST}visible`);
		expect(text(runs)).toBe("visible");
	});

	test("SOS string body consumed", () => {
		const runs = parseAnsiLine(`${ESC}Xbad${BEL}good`);
		expect(text(runs)).toBe("good");
	});

	test("PM string body consumed", () => {
		const runs = parseAnsiLine(`${ESC}^nope${ST}yes`);
		expect(text(runs)).toBe("yes");
	});

	test("OSC title (0/1/2) dropped", () => {
		const runs = parseAnsiLine(`${ESC}]0;title${BEL}text`);
		expect(text(runs)).toBe("text");
	});

	test("OSC clipboard (52) dropped", () => {
		const runs = parseAnsiLine(`${ESC}]52;c;Zm9v${BEL}text`);
		expect(text(runs)).toBe("text");
	});

	test("C0 control characters dropped", () => {
		const runs = parseAnsiLine(`a\x00\x01\x02\x08\x0b\x0c\x7fb`);
		expect(text(runs)).toBe("ab");
	});

	test("embedded NUL in text dropped", () => {
		const runs = parseAnsiLine("a\x00b");
		expect(text(runs)).toBe("ab");
	});

	test("private SGR parameter (e.g. font select) ignored, text preserved", () => {
		const runs = parseAnsiLine(`${ESC}[10mtext`);
		expect(text(runs)).toBe("text");
		// Font select (10) is not a recognized emphasis/color — silently ignored.
		expect(runs[0]?.style?.bold).toBeUndefined();
	});
});

describe("sanitize: ground-state isolation", () => {
	test("truncated CSI at end of line does not carry over", () => {
		const line1 = `${ESC}[1`;
		const line2 = `mbold`;
		// Each line parsed independently from fresh state.
		const r1 = parseAnsiLine(line1);
		const r2 = parseAnsiLine(line2);
		expect(text(r1)).toBe("");
		expect(text(r2)).toBe("mbold");
		expect(r2[0]?.style).toBeUndefined();
	});

	test("truncated OSC at end does not carry link to next line", () => {
		const r1 = parseAnsiLine(`${ESC}]8;;https://evil.com`);
		const r2 = parseAnsiLine(`${BEL}text`);
		expect(text(r1)).toBe("");
		expect(text(r2)).toBe("text");
		expect(r2[0]?.style?.link).toBeUndefined();
	});

	test("truncated CSI at end of line does not carry to next line", () => {
		// `\x1b[1;` has params but no valid final byte; the line ends there.
		// The next line starts from a fresh ground state.
		const lines = parseAnsiLines(`${ESC}[1;\nbold text`);
		expect(text(lines[0]!)).toBe("");
		expect(text(lines[1]!)).toBe("bold text");
		expect(lines[1]?.[0]?.style).toBeUndefined();
	});

	test("incomplete SGR params do not corrupt subsequent valid SGR", () => {
		// `\x1b[1;` is a broken CSI (no final byte). The embedded ESC starts a
		// new sequence: `\x1b[32m` is valid SGR green.
		const r = parseAnsiLine(`${ESC}[1;${ESC}[32mgreen`);
		expect(text(r)).toBe("green");
		expect(styleOf(r, 0)).toMatchObject({ fg: { type: "named", name: "green" } });
		// bold (1) from the broken sequence was NOT applied.
		expect(r[0]?.style?.bold).toBeUndefined();
	});
});

describe("sanitize: multi-line", () => {
	test("parseAnsiLines returns one array per line", () => {
		const lines = parseAnsiLines(`${ESC}[1mhello${ESC}[0m\nworld`);
		expect(lines).toHaveLength(2);
		expect(text(lines[0]!)).toBe("hello");
		expect(text(lines[1]!)).toBe("world");
		expect(styleOf(lines[0]!, 0)).toMatchObject({ bold: true });
	});

	test("empty input produces one empty line", () => {
		expect(parseAnsiLines("")).toEqual([[]]);
	});

	test("style resets per line (fresh parser each line)", () => {
		const lines = parseAnsiLines(`${ESC}[1mbold\nplain`);
		expect(styleOf(lines[0]!, 0)).toMatchObject({ bold: true });
		expect(lines[1]?.[0]?.style?.bold).toBeUndefined();
	});
});

describe("sanitize: complex real-world sequences", () => {
	test("nested hyperlink with color", () => {
		const input = `${ESC}]8;id=lnk;https://click.dev${ST}${ESC}[4;34mclick here${ESC}[0m${ESC}]8;;${ST}`;
		const runs = parseAnsiLine(input);
		expect(text(runs)).toBe("click here");
		expect(styleOf(runs, 0)).toMatchObject({
			underline: true,
			fg: { type: "named", name: "blue" },
			link: { id: "lnk", uri: "https://click.dev" },
		});
	});

	test("rapid style changes coalesce into runs", () => {
		const runs = parseAnsiLine(`${ESC}[31mred ${ESC}[32mgreen ${ESC}[33myellow`);
		expect(runs).toHaveLength(3);
		expect(runs[0]?.text).toBe("red ");
		expect(runs[1]?.text).toBe("green ");
		expect(runs[2]?.text).toBe("yellow");
	});
});

describe("sanitize: oversize and boundary limits", () => {
	test("OSC 8 URI at exactly 2048 bytes is accepted", () => {
		const uri = "https://x.com/" + "a".repeat(2048 - "https://x.com/".length);
		const runs = parseAnsiLine(`${ESC}]8;;${uri}${BEL}link${ESC}]8;;${BEL}`);
		expect(text(runs)).toBe("link");
		expect(styleOf(runs, 0)).toMatchObject({ link: { uri } });
	});

	test("OSC 8 URI over 2048 bytes is dropped", () => {
		const uri = "https://x.com/" + "a".repeat(2048);
		const runs = parseAnsiLine(`${ESC}]8;;${uri}${BEL}link${ESC}]8;;${BEL}`);
		expect(text(runs)).toBe("link");
		expect(runs[0]?.style?.link).toBeUndefined();
	});

	test("OSC 8 id over 128 bytes is dropped", () => {
		const id = "a".repeat(129);
		const runs = parseAnsiLine(`${ESC}]8;id=${id};https://ok.com${BEL}x${ESC}]8;;${BEL}`);
		expect(runs[0]?.style?.link).toBeUndefined();
	});

	test("256-color boundary: index 0 and 255 accepted", () => {
		expect(styleOf(parseAnsiLine(`${ESC}[38;5;0mx`), 0)).toMatchObject({ fg: { type: "indexed", index: 0 } });
		expect(styleOf(parseAnsiLine(`${ESC}[38;5;255mx`), 0)).toMatchObject({ fg: { type: "indexed", index: 255 } });
	});

	test("256-color index 256 rejected", () => {
		const runs = parseAnsiLine(`${ESC}[38;5;256mx`);
		expect(runs[0]?.style?.fg).toBeUndefined();
	});

	test("RGB component over 255 rejected", () => {
		const runs = parseAnsiLine(`${ESC}[38;2;256;0;0mx`);
		expect(runs[0]?.style?.fg).toBeUndefined();
	});
});

describe("sanitize: stale-generation reset", () => {
	test("each parseAnsiLine call starts from fresh ground state", () => {
		// First call leaves parser in "bold" state internally, but the next
		// call must NOT inherit it.
		const r1 = parseAnsiLine(`${ESC}[1mbold`);
		expect(styleOf(r1, 0)).toMatchObject({ bold: true });

		const r2 = parseAnsiLine("plain");
		expect(r2[0]?.style?.bold).toBeUndefined();
	});

	test("OSC 8 link does not carry across parseAnsiLine calls", () => {
		const r1 = parseAnsiLine(`${ESC}]8;;https://a.com${BEL}first`);
		expect(styleOf(r1, 0)?.link?.uri).toBe("https://a.com");

		const r2 = parseAnsiLine("second");
		expect(r2[0]?.style?.link).toBeUndefined();
	});

	test("color does not carry across parseAnsiLines calls", () => {
		parseAnsiLine(`${ESC}[31mred`);
		const lines = parseAnsiLines("clean");
		expect(lines[0]?.[0]?.style?.fg).toBeUndefined();
	});
});

// ---------------------------------------------------------------------------
// XC-7 M13 witness (TypeScript side): javascript: scheme must be rejected.
// If the scheme filter in applyOsc is mutated to admit javascript:, the link
// survives and the assertion fails.
// ---------------------------------------------------------------------------
describe("sanitize: XC-7 M13 javascript scheme witness", () => {
	test("javascript: URI via OSC 8 is always rejected", () => {
		const runs = parseAnsiLine(`${ESC}]8;;javascript:alert(document.cookie)${BEL}click${ESC}]8;;${BEL}`);
		expect(text(runs)).toBe("click");
		expect(runs[0]?.style?.link).toBeUndefined();
	});

	test("javascript: URI with uppercase scheme is rejected", () => {
		const runs = parseAnsiLine(`${ESC}]8;;JAVASCRIPT:alert(1)${BEL}x${ESC}]8;;${BEL}`);
		expect(runs[0]?.style?.link).toBeUndefined();
	});

	test("javascript: URI with mixed case is rejected", () => {
		const runs = parseAnsiLine(`${ESC}]8;;JavaScript:alert(1)${BEL}x${ESC}]8;;${BEL}`);
		expect(runs[0]?.style?.link).toBeUndefined();
	});
});
