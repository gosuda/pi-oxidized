/**
 * Lean extension runner (Mode 2).
 *
 * Speaks the existing JSONL protocol over stdin/stdout using the shared
 * ProtocolClient, but owns ZERO upstream runtime graph: no host.ts, no
 * builtins, no virtual-modules, no jiti, no `@earendil-works/pi-coding-agent`.
 * Entries are strict manifest-selected, prebundled `.mjs` files loaded with
 * a plain dynamic `import()` and validated against the declarative lean API
 * (`lean-api.ts`). Unknown/unsupported module surfaces become per-extension
 * load errors; the runner itself stays up.
 *
 * Wire parity with Mode 1 (`host.ts`): identical method names, payload
 * shapes, registry snapshot (RegistrySnapshotWire-compatible), error codes,
 * and hook response shaping. The only intentional deviation is the hello
 * handshake: Mode 2 validates `protocolVersion` ONLY and ignores
 * `compatibilityVersion`.
 */

import { readFile, stat } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
	COMPATIBILITY_VERSION,
	type ByteReadable,
	type ByteWritable,
	type Frame,
	type FrameHandler,
	type Method,
	PROTOCOL_VERSION,
	ProtocolClient,
} from "./protocol.ts";
import {
	assertJsonValue,
	cloneJsonValue,
	jsonValuesEqual,
	LEAN_EVENT_TYPES,
	type LeanCommand,
	type LeanContext,
	type LeanExtension,
	type LeanFlag,
	type LeanProvider,
	type LeanShortcut,
	type LeanTool,
	parseLeanExtension,
} from "./lean-api.ts";
import { AssistantDeltaReducer } from "./assistant-delta.ts";

/** Host lifecycle state. */
const RunnerState = {
	WAITING_HELLO: "WAITING_HELLO",
	LOADING: "LOADING",
	READY: "READY",
	DISPOSED: "DISPOSED",
} as const;
type RunnerState = (typeof RunnerState)[keyof typeof RunnerState];

/** Pending load options captured before the hello handshake completes. */
interface LoadOptions {
	cwd: string;
	extensionPaths: string[];
}

interface RegisteredTool {
	tool: LeanTool;
	extensionPath: string;
}
interface RegisteredCommand {
	command: LeanCommand;
	extensionPath: string;
}
interface RegisteredFlag {
	flag: LeanFlag;
	extensionPath: string;
}
interface RegisteredShortcut {
	key: string;
	shortcut: LeanShortcut;
	extensionPath: string;
}
interface RegisteredProvider {
	provider: LeanProvider;
	extensionPath: string;
}
interface RegisteredHook {
	handler: (event: never, ctx: LeanContext) => unknown;
	extensionPath: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}


/**
 * Import specifiers a lean entry must never reference. Lean entries are
 * prebundled, so ANY upstream-compat specifier means the entry was built
 * for the wrong mode. `@earendil-works/pi-tui-protocol` stays legal: it is
 * the shared wire package, not the upstream runtime graph.
 */
const EXCLUDED_SPECIFIER = /^(?:@earendil-works\/(?:pi-coding-agent|pi-agent-core|pi-ai|pi-tui(?!-protocol))|@mariozechner\/|jiti(?:\/|$)|typebox(?:\/|$)|.*\/(?:host|virtual-modules)\.ts$)/;

function isAsciiIdentifierStartCode(code: number): boolean {
	return (
		(code >= 97 && code <= 122) // a-z
		|| (code >= 65 && code <= 90) // A-Z
		|| code === 95 // _
		|| code === 36 // $
	);
}


const IDENTIFIER_START = /^[$_\p{ID_Start}]$/u;
const IDENTIFIER_CONTINUE = /^[$_\u200C\u200D\p{ID_Continue}]$/u;

interface IdentifierEscape {
	char: string;
	next: number;
}

interface IdentifierName {
	word: string;
	end: number;
	malformedEscape: boolean;
}

/** Decode one valid Unicode escape in an IdentifierName without throwing. */
function decodeIdentifierEscape(source: string, from: number): IdentifierEscape | undefined {
	if (source[from] !== "\\" || source[from + 1] !== "u") return undefined;
	const digitsStart = from + 2;
	let digits: string;
	let next: number;
	if (source[digitsStart] === "{") {
		const close = source.indexOf("}", digitsStart + 1);
		if (close === -1) return undefined;
		digits = source.slice(digitsStart + 1, close);
		next = close + 1;
		if (!/^[0-9A-Fa-f]{1,6}$/.test(digits)) return undefined;
	} else {
		digits = source.slice(digitsStart, digitsStart + 4);
		next = digitsStart + 4;
		if (!/^[0-9A-Fa-f]{4}$/.test(digits)) return undefined;
	}
	const codePoint = Number.parseInt(digits, 16);
	if (codePoint > 0x10_FFFF || (codePoint >= 0xD800 && codePoint <= 0xDFFF)) return undefined;
	return { char: String.fromCodePoint(codePoint), next };
}

/**
 * Read one complete IdentifierName, cooking escapes before comparing loader
 * names. Raw Unicode and escaped code points share the same start/continue
 * rules so a suffix such as `π\u0072equire` cannot split into a false bare
 * `require` token.
 */
function readCookedIdentifier(source: string, from: number): IdentifierName | undefined {
	let at = from;
	let word = "";
	while (at < source.length) {
		const codePoint = source.codePointAt(at);
		if (codePoint === undefined) break;
		const char = String.fromCodePoint(codePoint);
		const validRaw = word.length === 0
			? codePoint <= 0x7F
				? isAsciiIdentifierStartCode(codePoint)
				: IDENTIFIER_START.test(char)
			: codePoint <= 0x7F
				? isAsciiIdentifierStartCode(codePoint) || (codePoint >= 48 && codePoint <= 57)
				: IDENTIFIER_CONTINUE.test(char);
		if (validRaw) {
			word += char;
			at += char.length;
			continue;
		}
		if (source[at] !== "\\" || source[at + 1] !== "u") break;
		const escaped = decodeIdentifierEscape(source, at);
		if (escaped === undefined) {
			return { word, end: Math.min(at + 2, source.length), malformedEscape: true };
		}
		const validEscape = word.length === 0
			? IDENTIFIER_START.test(escaped.char)
			: IDENTIFIER_CONTINUE.test(escaped.char);
		if (!validEscape) return { word, end: escaped.next, malformedEscape: true };
		word += escaped.char;
		at = escaped.next;
	}
	return word.length === 0 ? undefined : { word, end: at, malformedEscape: false };
}

function isWhitespaceChar(char: string): boolean {
	return char === " " || char === "\t" || char === "\n" || char === "\r" || char === "\v" || char === "\f";
}

const REGEX_PREFIX_KEYWORDS = new Set([
	"return",
	"typeof",
	"case",
	"default",
	"delete",
	"void",
	"throw",
	"new",
	"in",
	"of",
	"instanceof",
	"yield",
	"await",
	"do",
	"else",
]);
const REGEX_PREFIX_PUNCTUATORS = new Set([
	"(",
	"[",
	"{",
	",",
	";",
	":",
	"=",
	"==",
	"===",
	"!=",
	"!==",
	"<=",
	">=",
	"<<",
	">>",
	">>>",
	"=>",
	"?",
	"??",
	"*",
	"**",
	"%",
	// A regex may be the right operand of division, a relational compare, or
	// `+`/`-`. Whole-token tracking is what makes `+` and `-` unambiguous here:
	// `++` and `--` are their own tokens, so the postfix-then-divide reading of
	// `x++ / 2` is preserved and only a lone `+`/`-` admits a regex.
	"/",
	">",
	"+",
	"-",
	"&",
	"&&",
	"|",
	"||",
	"^",
	"<",
	"~",
	"!",
	"+=",
	"-=",
	"*=",
	"**=",
	"/=",
	"%=",
	"&=",
	"&&=",
	"|=",
	"||=",
	"^=",
	"<<=",
	">>=",
	">>>=",
	"??=",
]);
const MULTI_CHARACTER_PUNCTUATORS = [
	">>>=",
	"&&=",
	"||=",
	"??=",
	"**=",
	">>=",
	"<<=",
	"===",
	"!==",
	"...",
	">>>",
	"&&",
	"||",
	"??",
	"**",
	"==",
	"!=",
	"<=",
	">=",
	"=>",
	"++",
	"--",
	"<<",
	">>",
	"?.",
	"+=",
	"-=",
	"*=",
	"/=",
	"%=",
	"&=",
	"|=",
	"^=",
] as const;
const MEMBER_PUNCTUATORS: Readonly<Record<string, true>> = { ".": true, "?.": true, "#": true };

type SignificantToken =
	| { kind: "identifier" | "keyword"; value: string }
	| { kind: "punctuator"; value: string }
	| { kind: "literal"; value: "regex" | "string" | "template" };

interface ModuleLoadScan {
	specifiers: string[];
	unsupported: string | undefined;
}

/**
 * Cook a string literal's raw source content (the text between its quotes)
 * into the value JavaScript would produce, resolving backslash escapes. The
 * import scanner records specifiers this way so an escaped literal such as
 * `import("j\u0069ti")` matches the same exclusion — and resolves to the same
 * path — as the plain `import("jiti")`.
 */
function decodeStringEscapes(raw: string): string {
	let out = "";
	let at = 0;
	const length = raw.length;
	while (at < length) {
		const char = raw[at];
		if (char !== "\\") {
			out += char;
			at += 1;
			continue;
		}
		const next = raw[at + 1];
		at += 2;
		switch (next) {
			case "n": out += "\n"; break;
			case "r": out += "\r"; break;
			case "t": out += "\t"; break;
			case "b": out += "\b"; break;
			case "f": out += "\f"; break;
			case "v": out += "\v"; break;
			case "0": out += "\0"; break;
			case "\n": break;
			case "\r":
				if (raw[at] === "\n") at += 1;
				break;
			case "x":
				out += String.fromCharCode(Number.parseInt(raw.slice(at, at + 2), 16));
				at += 2;
				break;
			case "u":
				if (raw[at] === "{") {
					const close = raw.indexOf("}", at);
					if (close === -1) {
						out += "u";
						break;
					}
					out += String.fromCodePoint(Number.parseInt(raw.slice(at + 1, close), 16));
					at = close + 1;
				} else {
					out += String.fromCharCode(Number.parseInt(raw.slice(at, at + 4), 16));
					at += 4;
				}
				break;
			default:
				out += next;
				break;
		}
	}
	return out;
}

/**
 * Extract literal module specifiers from every import form that can load a
 * lean dependency. The same extractor drives direct exclusion and local
 * graph traversal, so minification cannot create a weaker path.
 *
 * One forward lexical pass mirrors the JS lexer's view of the source:
 * line/block comments, single/double-quoted strings, and template raw text
 * are opaque (import-shaped text inside them never counts), while template
 * `${...}` expressions are scanned as code with full nesting. Static
 * import/export-from clauses, side-effect imports, and dynamic
 * `import(...)` calls are recognized only in code, and whitespace or
 * inline comments may separate `import`, `(`, and the quoted literal.
 */
function scanModuleLoads(source: string): ModuleLoadScan {
	const specifiers: string[] = [];
	let unsupported: string | undefined;
	const length = source.length;
	/** Object-brace depth of each open `${...}` expression, innermost last. */
	const templateExpressionDepths: number[] = [];
	let inTemplateRaw = false;
	// Loader-shaped names after a member punctuator are property names, never
	// import statements or regex-prefix keywords.
	let lastSignificant: SignificantToken | undefined;
	let index = 0;

	const readPunctuator = (from: number): string =>
		MULTI_CHARACTER_PUNCTUATORS.find((punctuator) => source.startsWith(punctuator, from)) ?? source[from];
	const recordWord = (word: string, preceded = lastSignificant): void => {
		const member = preceded?.kind === "punctuator" && MEMBER_PUNCTUATORS[preceded.value] === true;
		lastSignificant = {
			kind: REGEX_PREFIX_KEYWORDS.has(word) && !member ? "keyword" : "identifier",
			value: word,
		};
	};

	const skipLineComment = (from: number): number => {
		const end = source.indexOf("\n", from);
		return end === -1 ? length : end;
	};
	const skipBlockComment = (from: number): number => {
		const end = source.indexOf("*/", from);
		return end === -1 ? length : end + 2;
	};
	/** Next code index at or after `from`, past whitespace and comments. */
	const skipInsignificant = (from: number): number => {
		let at = from;
		while (at < length) {
			const char = source[at];
			if (isWhitespaceChar(char)) at += 1;
			else if (char === "/" && source[at + 1] === "/") at = skipLineComment(at + 2);
			else if (char === "/" && source[at + 1] === "*") at = skipBlockComment(at + 2);
			else break;
		}
		return at;
	};
	/** Advance past the quoted string whose opening quote sits at `from`. */
	const skipString = (from: number, quote: string): number => {
		let at = from + 1;
		while (at < length) {
			const char = source[at];
			if (char === "\\") at += 2;
			else if (char === quote) return at + 1;
			else at += 1;
		}
		return at;
	};
	/** Advance past a terminated regular-expression literal, if present. */
	const skipRegex = (from: number): number | undefined => {
		let at = from + 1;
		let inCharacterClass = false;
		while (at < length) {
			const char = source[at];
			if (char === "\\") {
				at += 2;
				continue;
			}
			if (char === "\n" || char === "\r") return undefined;
			if (inCharacterClass) {
				if (char === "]") inCharacterClass = false;
				at += 1;
				continue;
			}
			if (char === "[") {
				inCharacterClass = true;
				at += 1;
				continue;
			}
			if (char !== "/") {
				at += 1;
				continue;
			}
			at += 1;
			while (at < length) {
				const code = source.charCodeAt(at);
				if ((code < 65 || code > 90) && (code < 97 || code > 122)) break;
				at += 1;
			}
			return at;
		}
		return undefined;
	};
	/**
	 * Permit regexes only after punctuation that cannot end an expression.
	 * Ambiguous punctuation (such as `+`, which may close `x++`) remains
	 * division so the scanner never hides following real code.
	 */
	const canStartRegex = (previous: SignificantToken | undefined): boolean =>
		previous === undefined
		|| (previous.kind === "keyword" && REGEX_PREFIX_KEYWORDS.has(previous.value))
		|| (previous.kind === "punctuator" && REGEX_PREFIX_PUNCTUATORS.has(previous.value));
	/**
	 * Record the terminated string literal at `from` as a specifier and
	 * return the index after its closing quote; undefined otherwise.
	 */
	const readSpecifier = (from: number): number | undefined => {
		const quote = source[from];
		if (quote !== '"' && quote !== "'") return undefined;
		const end = skipString(from, quote);
		if (end <= from + 1 || source[end - 1] !== quote) return undefined;
		specifiers.push(decodeStringEscapes(source.slice(from + 1, end - 1)));
		lastSignificant = { kind: "literal", value: "string" };
		return end;
	};
	/**
	 * Walk an import/export clause looking for `from "specifier"`. The
	 * clause region may span anything except quotes, parens, and backticks
	 * (the retired regex contract); a nested `import`/`export` keyword
	 * hands control back to the main scan so dynamic forms inside a
	 * malformed clause are not lost.
	 */
	const scanFromClause = (from: number): number => {
		let at = from;
		while (at < length) {
			const char = source[at];
			if (isWhitespaceChar(char)) {
				at += 1;
				continue;
			}
			if (char === "/" && (source[at + 1] === "/" || source[at + 1] === "*")) {
				at = source[at + 1] === "/" ? skipLineComment(at + 2) : skipBlockComment(at + 2);
				continue;
			}
			if (char === "/" && canStartRegex(lastSignificant)) {
				const regexEnd = skipRegex(at);
				if (regexEnd !== undefined) {
					at = regexEnd;
					lastSignificant = { kind: "literal", value: "regex" };
					continue;
				}
			}
			if (char === '"' || char === "'" || char === "(" || char === ")" || char === "`") return at;
			const start = at;
			const identifier = readCookedIdentifier(source, at);
			if (identifier === undefined) {
				const punctuator = readPunctuator(at);
				lastSignificant = { kind: "punctuator", value: punctuator };
				at += punctuator.length;
				continue;
			}
			if (identifier.malformedEscape) {
				unsupported ??= "malformed escaped identifier";
				at = identifier.end;
				continue;
			}
			const word = identifier.word;
			at = identifier.end;
			if (word === "import" || word === "export") return start;
			recordWord(word);
			if (word !== "from") continue;
			const literalEnd = readSpecifier(skipInsignificant(at));
			if (literalEnd !== undefined) return literalEnd;
		}
		return at;
	};

	while (index < length) {
		const char = source[index];
		if (inTemplateRaw) {
			if (char === "\\") {
				index += 2;
			} else if (char === "`") {
				inTemplateRaw = false;
				lastSignificant = { kind: "literal", value: "template" };
				index += 1;
			} else if (char === "$" && source[index + 1] === "{") {
				templateExpressionDepths.push(0);
				inTemplateRaw = false;
				lastSignificant = { kind: "punctuator", value: "{" };
				index += 2;
			} else {
				index += 1;
			}
			continue;
		}
		if (isWhitespaceChar(char)) {
			index += 1;
			continue;
		}
		if (char === "/" && (source[index + 1] === "/" || source[index + 1] === "*")) {
			index = source[index + 1] === "/" ? skipLineComment(index + 2) : skipBlockComment(index + 2);
			continue;
		}
		if (char === "/" && canStartRegex(lastSignificant)) {
			const regexEnd = skipRegex(index);
			if (regexEnd !== undefined) {
				index = regexEnd;
				lastSignificant = { kind: "literal", value: "regex" };
				continue;
			}
		}
		if (char === '"' || char === "'") {
			index = skipString(index, char);
			lastSignificant = { kind: "literal", value: "string" };
			continue;
		}
		if (char === "`") {
			inTemplateRaw = true;
			lastSignificant = { kind: "literal", value: "template" };
			index += 1;
			continue;
		}
		if (char === "{" || char === "}") {
			index += 1;
			lastSignificant = { kind: "punctuator", value: char };
			const top = templateExpressionDepths.length - 1;
			if (top < 0) continue;
			if (char === "{") {
				templateExpressionDepths[top] += 1;
			} else if (templateExpressionDepths[top] > 0) {
				templateExpressionDepths[top] -= 1;
			} else {
				templateExpressionDepths.pop();
				inTemplateRaw = true;
			}
			continue;
		}
		const identifier = readCookedIdentifier(source, index);
		if (identifier === undefined) {
			const punctuator = readPunctuator(index);
			lastSignificant = { kind: "punctuator", value: punctuator };
			index += punctuator.length;
			continue;
		}
		if (identifier.malformedEscape) {
			unsupported ??= "malformed escaped identifier";
			index = identifier.end;
			continue;
		}
		const word = identifier.word;
		index = identifier.end;
		const preceded = lastSignificant;
		recordWord(word, preceded);
		const isMember = preceded?.kind === "punctuator" && MEMBER_PUNCTUATORS[preceded.value] === true;
		// Bun exposes bare `require` in ESM. Keep this check lexical and
		// fail closed: proving a shadowed binding would require a full parser.
		if (word === "require" && !isMember) {
			const next = skipInsignificant(index);
			if (source[next] === "(") {
				const literalStart = skipInsignificant(next + 1);
				const literalEnd = readSpecifier(literalStart);
				if (literalEnd !== undefined) {
					index = literalEnd;
				} else {
					unsupported ??= "computed require(...)";
					lastSignificant = { kind: "punctuator", value: "(" };
					index = literalStart;
				}
				continue;
			}
		}
		if ((word !== "import" && word !== "export") || isMember) continue;
		if (word === "export") {
			index = scanFromClause(index);
			continue;
		}
		const next = skipInsignificant(index);
		const nextChar = next < length ? source[next] : "";
		if (nextChar === "(") {
			const literalStart = skipInsignificant(next + 1);
			const literalEnd = readSpecifier(literalStart);
			if (literalEnd !== undefined) {
				index = literalEnd;
			} else {
				unsupported ??= "computed import(...)";
				lastSignificant = { kind: "punctuator", value: "(" };
				index = literalStart;
			}
			continue;
		}
		if (nextChar === '"' || nextChar === "'") {
			index = readSpecifier(next) ?? next;
			continue;
		}
		if (nextChar === ".") {
			// `import.meta` — not an import; resume at the dot.
			index = next;
			continue;
		}
		index = scanFromClause(next);
	}
	return { specifiers, unsupported };
}

/** Extract literal ESM specifiers without treating unsupported forms as imports. */
function extractImportSpecifiers(source: string): string[] {
	return scanModuleLoads(source).specifiers;
}

/** Detect an excluded direct specifier without evaluating the module. */
export function findExcludedImport(source: string): string | undefined {
	return extractImportSpecifiers(source).find((specifier) => EXCLUDED_SPECIFIER.test(specifier));
}

const BUN_LOCAL_IMPORT_EXTENSIONS = [
	".tsx",
	".jsx",
	".ts",
	".mts",
	".js",
	".mjs",
	".cjs",
	".cts",
	".json",
] as const;

async function resolveLocalSpecifier(importer: string, specifier: string): Promise<string | undefined> {
	if (
		!specifier.startsWith("./")
		&& !specifier.startsWith("../")
		&& !specifier.startsWith("/")
		&& !specifier.startsWith("file:")
	) {
		return undefined;
	}
	const resolved = new URL(specifier, pathToFileURL(importer));
	if (resolved.protocol !== "file:") return undefined;
	const path = fileURLToPath(resolved);
	const candidates = specifier.endsWith("/")
		? BUN_LOCAL_IMPORT_EXTENSIONS.map((extension) => join(path, `index${extension}`))
		: [
			path,
			...BUN_LOCAL_IMPORT_EXTENSIONS.map((extension) => `${path}${extension}`),
			...BUN_LOCAL_IMPORT_EXTENSIONS.map((extension) => join(path, `index${extension}`)),
		];
	for (const candidate of candidates) {
		try {
			if ((await stat(candidate)).isFile()) return candidate;
		} catch {
			// Let the runtime report an unresolved local module as it did before graph scanning.
		}
	}
	return path;
}

type ModuleLoadViolation =
	| { kind: "excluded"; specifier: string }
	| { kind: "unsupported"; form: string };

/** Walk every local dependency whose module-loading form is statically provable. */
async function findModuleLoadViolationInGraph(entry: string): Promise<ModuleLoadViolation | undefined> {
	const pending = [entry];
	const visited = new Set<string>();
	while (pending.length > 0) {
		const current = pending.pop();
		if (current === undefined || visited.has(current)) continue;
		visited.add(current);
		const scan = scanModuleLoads(await readFile(current, "utf8"));
		if (scan.unsupported !== undefined) return { kind: "unsupported", form: scan.unsupported };
		for (const specifier of scan.specifiers) {
			if (EXCLUDED_SPECIFIER.test(specifier)) return { kind: "excluded", specifier };
			const local = await resolveLocalSpecifier(current, specifier);
			if (local !== undefined && !visited.has(local)) pending.push(local);
		}
	}
	return undefined;
}

/**
 * Structured cancellation only: a real Error (or DOMException, which is
 * not Error-derived in every runtime) named AbortError. Message text is
 * deliberately never consulted — an extension failure that merely says
 * "cancelled" must stay an extension_error.
 */
function isStructuredAbortError(error: unknown): boolean {
	if (error instanceof Error && error.name === "AbortError") return true;
	return typeof DOMException === "function" && error instanceof DOMException && error.name === "AbortError";
}

/**
 * Lean Mode-2 endpoint process. Owns the declarative registry and bridges
 * it to Rust over a single JSONL byte transport, mirroring the Mode-1
 * host's four-state machine (hello → loading → ready → disposed).
 */
export class LeanRunner {
	private readonly client: ProtocolClient;
	private state: RunnerState = RunnerState.WAITING_HELLO;
	private loadOptions: LoadOptions | undefined;
	private cwd: string = process.cwd();
	/** Frames buffered while extensions are loading. */
	private readonly pendingFrames: Frame[] = [];

	private readonly tools = new Map<string, RegisteredTool>();
	private readonly commands = new Map<string, RegisteredCommand>();
	private readonly flags = new Map<string, RegisteredFlag>();
	private readonly flagValues = new Map<string, boolean | string>();
	private readonly shortcuts: RegisteredShortcut[] = [];
	private readonly providers = new Map<string, RegisteredProvider>();
	private readonly hooks = new Map<string, RegisteredHook[]>();
	private loadedCount = 0;

	/** In-flight tool.execute AbortControllers keyed by request id. */
	private readonly inFlightTools = new Map<number, AbortController>();
	/** In-flight provider.stream AbortControllers keyed by request id. */
	private readonly inFlightProviders = new Map<number, AbortController>();
	/** Aborts every shortcut handler when this runner is disposed. */
	private readonly shortcutAbortController = new AbortController();
	/** System prompt mirrored from `session.update` control events. */
	private systemPrompt = "";
	/** Active assistant snapshot reconstructed from compact Rust updates. */
	private readonly assistantDelta = new AssistantDeltaReducer();

	constructor(stdin: ByteReadable, stdout: ByteWritable) {
		const onFrame: FrameHandler = (frame) => this.onInbound(frame);
		this.client = new ProtocolClient(stdout, { onFrame });
		this.client.start(stdin);
	}

	/**
	 * Configure load options and run until EOF/dispose. The hello handshake
	 * drives the rest: once the protocol version matches, CLI-selected
	 * extensions load and the runner enters the serving loop.
	 */
	run(options: LoadOptions): Promise<void> {
		this.loadOptions = options;
		this.cwd = options.cwd;
		return this.client.join();
	}

	get isDisposed(): boolean {
		return this.state === RunnerState.DISPOSED;
	}

	get extensionCount(): number {
		return this.loadedCount;
	}

	// -----------------------------------------------------------------------
	// Inbound frame state machine
	// -----------------------------------------------------------------------

	private onInbound(frame: Frame): void {
		switch (this.state) {
			case RunnerState.WAITING_HELLO:
				this.handleHelloFrame(frame);
				return;
			case RunnerState.LOADING:
				this.pendingFrames.push(frame);
				return;
			case RunnerState.READY:
				if (frame.kind === "req") {
					this.handleRequest(frame).catch((err) => {
						this.respondError(frame, err instanceof Error ? err.message : String(err));
					});
				} else if (frame.kind === "event") {
					this.handleControlEvent(frame);
				}
				return;
			case RunnerState.DISPOSED:
				return;
		}
	}

	/**
	 * Mode-2 handshake: validate `protocolVersion` ONLY. The
	 * `compatibilityVersion` field is ignored entirely — lean endpoints do
	 * not track the upstream coding-agent version.
	 */
	private handleHelloFrame(frame: Frame): void {
		if (frame.method !== "hello") {
			this.terminate(`expected hello as first frame, got: ${frame.method}`);
			return;
		}
		const payload = frame.payload as Record<string, unknown>;
		const remoteProtocol = payload["protocolVersion"];
		if (typeof remoteProtocol !== "number" || remoteProtocol !== PROTOCOL_VERSION) {
			this.terminate(
				`protocol version mismatch: remote=${String(remoteProtocol)} local=${PROTOCOL_VERSION}`,
			);
			return;
		}
		this.client.respond(frame.id, "hello", {
			protocolVersion: PROTOCOL_VERSION,
			compatibilityVersion: COMPATIBILITY_VERSION,
		}).then(
			() => this.startLoading(),
			(err) => this.terminate(err instanceof Error ? err.message : String(err)),
		);
	}

	private async startLoading(): Promise<void> {
		this.state = RunnerState.LOADING;
		const opts = this.loadOptions;
		if (opts === undefined) {
			this.terminate("no load options");
			return;
		}
		const { errors } = await this.loadAll(opts.extensionPaths, opts.cwd);
		for (const error of errors) {
			this.emitExtensionError(error.path, "load", error.error);
		}
		this.state = RunnerState.READY;

		const buffered = [...this.pendingFrames];
		this.pendingFrames.length = 0;
		for (const frame of buffered) {
			this.onInbound(frame);
		}
	}

	// -----------------------------------------------------------------------
	// Extension loading
	// -----------------------------------------------------------------------

	/**
	 * Load strict manifest-selected prebundled `.mjs` entries with a plain
	 * dynamic import. Every failure — wrong extension, excluded import,
	 * import error, unknown/unsupported module surface — is isolated to its
	 * own entry in `errors`; siblings still bind.
	 */
	private async loadAll(
		paths: readonly string[],
		cwd: string,
	): Promise<{ loaded: number; errors: Array<{ path: string; error: string }> }> {
		const errors: Array<{ path: string; error: string }> = [];
		let loaded = 0;
		for (const entryPath of paths) {
			try {
				await this.loadOne(entryPath, cwd);
				loaded++;
			} catch (err) {
				errors.push({
					path: entryPath,
					error: err instanceof Error ? err.message : String(err),
				});
			}
		}
		this.loadedCount += loaded;
		return { loaded, errors };
	}

	private async loadOne(entryPath: string, cwd: string): Promise<void> {
		if (!entryPath.endsWith(".mjs")) {
			throw new Error(
				`lean entries must be prebundled .mjs files (manifest runtime "ts-lean"): ${entryPath}`,
			);
		}
		const absolute = isAbsolute(entryPath) ? entryPath : resolve(cwd, entryPath);
		const violation = await findModuleLoadViolationInGraph(absolute);
		if (violation?.kind === "excluded") {
			throw new Error(
				`excluded import "${violation.specifier}" in lean entry: the upstream module graph ` +
					"is unavailable in lean mode; prebundle the entry instead",
			);
		}
		if (violation?.kind === "unsupported") {
			throw new Error(
				`unsupported ${violation.form} in lean entry: module loading must use literal ESM imports`,
			);
		}
		// Dynamic import is required: the entry specifier is runtime-selected
		// (manifest-chosen plugin path), so a static import cannot work.
		const module: unknown = await import(pathToFileURL(absolute).href);
		const definition = parseLeanExtension(
			isRecord(module) ? module["default"] : undefined,
		);
		this.register(definition, entryPath);
	}

	/** Bind one validated definition into the registry (first-wins, load order). */
	private register(definition: LeanExtension, extensionPath: string): void {
		for (const tool of definition.tools ?? []) {
			if (!this.tools.has(tool.name)) {
				this.tools.set(tool.name, { tool, extensionPath });
			}
		}
		for (const command of definition.commands ?? []) {
			if (!this.commands.has(command.name)) {
				this.commands.set(command.name, { command, extensionPath });
			}
		}
		for (const flag of definition.flags ?? []) {
			if (!this.flags.has(flag.name)) {
				this.flags.set(flag.name, { flag, extensionPath });
			}
		}
		for (const shortcut of definition.shortcuts ?? []) {
			this.shortcuts.push({ key: shortcut.key, shortcut, extensionPath });
		}
		for (const provider of definition.providers ?? []) {
			if (!this.providers.has(provider.name)) {
				this.providers.set(provider.name, { provider, extensionPath });
			}
		}
		for (const [eventType, handler] of Object.entries(definition.hooks ?? {})) {
			if (handler === undefined) continue;
			const list = this.hooks.get(eventType) ?? [];
			list.push({
				handler: handler as RegisteredHook["handler"],
				extensionPath,
			});
			this.hooks.set(eventType, list);
		}
	}

	/** Effective flag values: applied `flags.set` wins, then the declared default. */
	private effectiveFlags(): Record<string, boolean | string> {
		const flags: Record<string, boolean | string> = {};
		for (const [name, { flag }] of this.flags) {
			const applied = this.flagValues.get(name);
			if (applied !== undefined) flags[name] = applied;
			else if (flag.default !== undefined) flags[name] = flag.default;
		}
		return flags;
	}

	private hookContext(extensionPath: string): LeanContext {
		return { cwd: this.cwd, extensionPath, flags: this.effectiveFlags() };
	}

	// -----------------------------------------------------------------------
	// Registry snapshot (RegistrySnapshotWire-compatible)
	// -----------------------------------------------------------------------

	private buildRegistrySnapshot(): Record<string, unknown> {
		const tools = [...this.tools.values()].map(({ tool }) => {
			const entry: Record<string, unknown> = {
				name: tool.name,
				label: tool.label ?? tool.name,
				description: tool.description,
				parameters: tool.parameters ?? {},
			};
			if (tool.executionMode !== undefined) {
				entry["executionMode"] = tool.executionMode;
			}
			return entry;
		});

		const commands = [...this.commands.values()].map(({ command, extensionPath }) => ({
			name: command.name,
			description: command.description,
			source: extensionPath,
		}));

		const shortcuts = this.shortcuts.map(({ key, shortcut, extensionPath }) => ({
			key,
			description: shortcut.description,
			extensionPath,
		}));

		const flags = [...this.flags.values()].map(({ flag, extensionPath }) => {
			const entry: Record<string, unknown> = {
				name: flag.name,
				description: flag.description,
				type: flag.type,
				extensionPath,
			};
			if (flag.default !== undefined) {
				entry["default"] = flag.default;
			}
			if (this.flagValues.has(flag.name)) {
				entry["value"] = this.flagValues.get(flag.name);
			}
			return entry;
		});

		const providers = [...this.providers.values()].map(({ provider, extensionPath }) => {
			const entry: Record<string, unknown> = {
				name: provider.name,
				streamSimple: typeof provider.streamSimple === "function",
				extensionPath,
			};
			if (provider.baseUrl !== undefined) entry["baseUrl"] = provider.baseUrl;
			if (provider.api !== undefined) entry["api"] = provider.api;
			if (provider.displayName !== undefined) entry["displayName"] = provider.displayName;
			if (provider.apiKey !== undefined) entry["apiKey"] = provider.apiKey;
			if (provider.headers !== undefined) entry["headers"] = provider.headers;
			if (provider.authHeader !== undefined) entry["authHeader"] = provider.authHeader;
			if (provider.models !== undefined) entry["models"] = provider.models;
			return entry;
		});

		const handlers = LEAN_EVENT_TYPES.filter((eventType) => this.hooks.has(eventType));

		return {
			tools,
			commands,
			shortcuts,
			flags,
			renderers: [],
			providers,
			handlers,
			terminalInput: false,
		};
	}

	// -----------------------------------------------------------------------
	// Request dispatch (Rust → lean runner)
	// -----------------------------------------------------------------------

	private async handleRequest(frame: Frame): Promise<void> {
		const { id, method, payload } = frame;
		const p = isRecord(payload) ? payload : {};

		switch (method) {
			case "extensions.load":
				await this.handleExtensionsLoad(id, p);
				return;
			case "command.execute":
				await this.handleCommandExecute(id, p);
				return;
			case "tool.prepare":
				await this.handleToolPrepare(id, p);
				return;
			case "tool.validate":
				await this.handleToolValidate(id, p);
				return;
			case "tool.execute":
				await this.handleToolExecute(id, p);
				return;
			case "tool.renderHtml":
				// Lean tools carry no renderers; mirror Mode 1's no-renderer reply.
				await this.client.respond(id, "tool.renderHtml" as Method, {});
				return;
			case "provider.stream":
				await this.handleProviderStream(id, p);
				return;
			case "flags.set":
				await this.handleFlagsSet(id, p);
				return;
			case "shortcut.execute":
				await this.handleShortcutExecute(id, p);
				return;
			case "message_update_delta":
				await this.handleMessageUpdateDelta(id, p);
				return;
			default:
				if (this.hooks.has(method)) {
					await this.handleLifecycleHook(id, method, p);
					return;
				}
				this.respondError(frame, `unknown method: ${method}`);
		}
	}

	private async handleExtensionsLoad(id: number, p: Record<string, unknown>): Promise<void> {
		this.assistantDelta.clearActiveAssistant();
		const paths = p["extensionPaths"] ?? p["paths"];
		const extensionPaths = Array.isArray(paths)
			? paths.filter((path): path is string => typeof path === "string")
			: [];
		const cwd = typeof p["cwd"] === "string" ? p["cwd"] : this.cwd;
		this.cwd = cwd;

		const { loaded, errors } = await this.loadAll(extensionPaths, cwd);
		const snapshot = this.buildRegistrySnapshot();
		await this.client.respond(id, "extensions.load" as Method, {
			...snapshot,
			extensions: loaded,
			errors,
		});
	}

	private async handleCommandExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const commandName = (p["command"] ?? p["name"]) as string;
		const args = (p["args"] as string) ?? "";
		const registered = this.commands.get(commandName);
		if (registered === undefined) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "not_found",
				message: `Command not found: ${commandName}`,
				retryable: false,
			});
			return;
		}
		try {
			await registered.command.handler(args, this.hookContext(registered.extensionPath));
			await this.client.respond(id, "command.execute" as Method, { ok: true });
		} catch (err) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleToolPrepare(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const args = p["args"];
		const registered = this.tools.get(name);
		if (registered === undefined) {
			await this.client.respondError(id, "tool.prepare" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}
		try {
			const prepared = registered.tool.prepare !== undefined
				? await registered.tool.prepare(args, this.hookContext(registered.extensionPath))
				: args;
			await this.client.respond(id, "tool.prepare" as Method, { args: prepared });
		} catch (err) {
			await this.client.respondError(id, "tool.prepare" as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleToolValidate(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const args = p["args"];
		const registered = this.tools.get(name);
		if (registered === undefined) {
			await this.client.respondError(id, "tool.validate" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}
		try {
			const validated = registered.tool.validate !== undefined
				? await registered.tool.validate(args, this.hookContext(registered.extensionPath))
				: args;
			await this.client.respond(id, "tool.validate" as Method, { args: validated });
		} catch (err) {
			await this.client.respondError(id, "tool.validate" as Method, {
				code: "invalid_arguments",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleToolExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const toolCallId = String(p["toolCallId"] ?? "");
		const args = p["args"];
		const registered = this.tools.get(name);
		if (registered === undefined) {
			await this.client.respondError(id, "tool.execute" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}

		const controller = new AbortController();
		let acceptingUpdates = true;
		let pendingUpdate: unknown;
		let hasPendingUpdate = false;
		let drain: Promise<void> | undefined;
		const stopAcceptingUpdates = () => {
			acceptingUpdates = false;
		};
		const drainUpdates = (): void => {
			if (drain !== undefined) return;
			drain = (async () => {
				while (hasPendingUpdate) {
					const partial = pendingUpdate;
					hasPendingUpdate = false;
					try {
						await this.client.send({
							id,
							kind: "event",
							method: "toolUpdate",
							payload: { toolCallId, toolName: name, partialResult: partial },
						});
					} catch {
						// Preserve the legacy fire-and-forget failure behavior.
					}
				}
			})().finally(() => {
				drain = undefined;
				if (acceptingUpdates && hasPendingUpdate) drainUpdates();
			});
		};
		controller.signal.addEventListener("abort", stopAcceptingUpdates, { once: true });
		this.inFlightTools.set(id, controller);
		try {
			const prepared = p["prepared"] === true || registered.tool.prepare === undefined
				? args
				: await registered.tool.prepare(args, this.hookContext(registered.extensionPath));
			const result = await registered.tool.execute(prepared, {
				...this.hookContext(registered.extensionPath),
				toolCallId,
				signal: controller.signal,
				onUpdate: (partial) => {
					if (!acceptingUpdates || controller.signal.aborted) return;
					pendingUpdate = partial;
					hasPendingUpdate = true;
					drainUpdates();
				},
			});
			stopAcceptingUpdates();
			await drain;
			if (controller.signal.aborted) {
				await this.client.respondError(id, "tool.execute" as Method, {
					code: "cancelled",
					message: "extension tool cancelled",
					retryable: false,
				});
				return;
			}
			await this.client.respond(id, "tool.execute" as Method, result);
		} catch (err) {
			stopAcceptingUpdates();
			await drain;
			const cancelled = controller.signal.aborted || isStructuredAbortError(err);
			const message = err instanceof Error ? err.message : String(err);
			await this.client.respondError(id, "tool.execute" as Method, {
				code: cancelled ? "cancelled" : "extension_error",
				message: cancelled ? "extension tool cancelled" : message,
				retryable: false,
			});
		} finally {
			controller.signal.removeEventListener("abort", stopAcceptingUpdates);
			this.inFlightTools.delete(id);
		}
	}

	private async handleProviderStream(id: number, p: Record<string, unknown>): Promise<void> {
		const providerId = String(p["providerId"] ?? p["name"] ?? "");
		const registered = this.providers.get(providerId);
		if (registered === undefined || typeof registered.provider.streamSimple !== "function") {
			await this.client.respondError(id, "provider.stream" as Method, {
				code: "not_found",
				message: `Provider not found or missing streamSimple: ${providerId}`,
				retryable: false,
			});
			return;
		}

		const controller = new AbortController();
		this.inFlightProviders.set(id, controller);
		const options = {
			...(isRecord(p["options"]) ? p["options"] : {}),
			signal: controller.signal,
		};
		try {
			const stream = registered.provider.streamSimple(p["model"], p["context"], options);
			for await (const event of stream) {
				if (controller.signal.aborted) break;
				await this.client.send({ id, kind: "event", method: "providerEvent", payload: event });
			}
			if (controller.signal.aborted) {
				await this.client.respondError(id, "provider.stream" as Method, {
					code: "cancelled",
					message: "provider stream cancelled",
					retryable: false,
				});
				return;
			}
			await this.client.respond(id, "provider.stream" as Method, {});
		} catch (err) {
			const cancelled = controller.signal.aborted || isStructuredAbortError(err);
			const message = err instanceof Error ? err.message : String(err);
			await this.client.respondError(id, "provider.stream" as Method, {
				code: cancelled ? "cancelled" : "extension_error",
				message: cancelled ? "provider stream cancelled" : message,
				retryable: false,
			});
		} finally {
			this.inFlightProviders.delete(id);
		}
	}

	private async handleFlagsSet(id: number, p: Record<string, unknown>): Promise<void> {
		const values = p["values"];
		if (!isRecord(values)) {
			await this.client.respondError(id, "flags.set", {
				code: "invalid_arguments",
				message: "flags.set values must be an object",
				retryable: false,
			});
			return;
		}
		const entries: Array<readonly [string, boolean | string]> = [];
		for (const [name, value] of Object.entries(values)) {
			if (typeof value !== "boolean" && typeof value !== "string") {
				await this.client.respondError(id, "flags.set", {
					code: "invalid_arguments",
					message: `flags.set value for "${name}" must be boolean or string`,
					retryable: false,
				});
				return;
			}
			entries.push([name, value]);
		}
		for (const [name, value] of entries) {
			this.flagValues.set(name, value);
		}
		await this.client.respond(id, "flags.set", { ok: true });
	}

	private async handleShortcutExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"];
		if (typeof key !== "string") {
			await this.client.respond(id, "shortcut.execute", { handled: false });
			return;
		}
		// Product resolution is last-wins across extensions (load order).
		let registered: RegisteredShortcut | undefined;
		for (let index = this.shortcuts.length - 1; index >= 0; index--) {
			const candidate = this.shortcuts[index];
			if (candidate?.key === key) {
				registered = candidate;
				break;
			}
		}
		if (registered === undefined) {
			await this.client.respond(id, "shortcut.execute", { handled: false });
			return;
		}
		await this.client.respond(id, "shortcut.execute", { handled: true });
		void Promise.resolve()
			.then(() => registered.shortcut.handler({
				...this.hookContext(registered.extensionPath),
				signal: this.shortcutAbortController.signal,
			}))
			.catch((error) => {
				this.emitExtensionError(
					registered.extensionPath,
					"shortcut.execute",
					error instanceof Error ? error.message : String(error),
				);
			});
	}

	// -----------------------------------------------------------------------
	// Lifecycle hooks
	// -----------------------------------------------------------------------

	/**
	 * Invoke every hook registered for `eventType` in load order, isolating
	 * per-hook failures to `extensionError` events (mirroring the upstream
	 * runner). Result threading mirrors Mode 1 per event type. A thunk event
	 * is rebuilt per handler so ordered folds (input, before_agent_start,
	 * tool_result, message_end) hand each handler the running values, exactly
	 * like the upstream specialized emitters.
	 */
	private async runHooks(
		eventType: string,
		event: Record<string, unknown> | (() => Record<string, unknown>),
		onResult: (result: unknown, extensionPath: string) => boolean | void,
	): Promise<void> {
		for (const { handler, extensionPath } of this.hooks.get(eventType) ?? []) {
			try {
				const currentEvent = typeof event === "function" ? event() : event;
				const result = await handler(currentEvent as never, this.hookContext(extensionPath));
				if (onResult(result, extensionPath) === false) return;
			} catch (err) {
				this.emitExtensionError(
					extensionPath,
					eventType,
					err instanceof Error ? err.message : String(err),
				);
			}
		}
	}

	/**
	 * Drive the declared hooks for a lifecycle request. The method name IS
	 * the event type discriminant; response shaping mirrors Mode 1 exactly.
	 */
	private async handleLifecycleHook(
		id: number,
		eventType: string,
		payload: Record<string, unknown>,
	): Promise<void> {
		try {
			if (eventType === "message_start") {
				const message = payload["message"];
				if (isRecord(message) && message["role"] === "assistant") {
					this.assistantDelta.seedActiveAssistant(message);
				}
			}
			switch (eventType) {
				case "tool_call": {
					const input = payload["input"];
					if (!isRecord(input)) throw new Error("tool_call.input is required");
					// Snapshot for omission: Rust treats wire.input = Some as
					// arguments_changed. Only echo input when a handler actually
					// mutated JSON content (key reorder alone is not a change).
					const baseline = cloneJsonValue("tool_call.input", input);
					let result: unknown;
					await this.runHooks(eventType, { type: eventType, ...payload, input }, (r) => {
						if (r === undefined || r === null) return;
						result = r;
						if (isRecord(r) && r["block"] === true) return false;
					});
					const response: Record<string, unknown> = {
						...(isRecord(result) ? result : {}),
					};
					if (!jsonValuesEqual(input, baseline)) {
						response["input"] = input;
					} else {
						delete response["input"];
					}
					await this.client.respond(id, eventType as Method, response);
					return;
				}
				case "tool_result": {
					const input = payload["input"];
					if (!isRecord(input)) throw new Error("tool_result.input is required");
					assertJsonValue("tool_result.input", input);
					// `current` threads running values to later handlers; `response`
					// is omission-shaped for Rust AfterToolCallWire (presence of a
					// field marks that field changed — never echo untouched payload).
					const current: Record<string, unknown> = {
						content: payload["content"],
						details: payload["details"],
						isError: payload["isError"] === true,
					};
					const response: Record<string, unknown> = {};
					await this.runHooks(eventType, () => ({ type: eventType, ...payload, input, ...current }), (r) => {
						if (!isRecord(r)) return;
						if (r["content"] !== undefined) {
							current["content"] = r["content"];
							response["content"] = r["content"];
						}
						if (r["details"] !== undefined) {
							current["details"] = r["details"];
							response["details"] = r["details"];
						}
						if (r["isError"] !== undefined) {
							current["isError"] = r["isError"];
							response["isError"] = r["isError"];
						}
					});
					await this.client.respond(id, eventType as Method, response);
					return;
				}
				case "before_agent_start": {
					// Cross-endpoint folds carry the running prompt in the payload;
					// the session.update mirror is only the single-endpoint fallback.
					let systemPrompt: unknown =
						typeof payload["systemPrompt"] === "string"
							? payload["systemPrompt"]
							: this.systemPrompt;
					const messages: unknown[] = [];
					let systemPromptModified = false;
					await this.runHooks(
						eventType,
						() => ({
							type: eventType,
							...payload,
							systemPrompt,
							systemPromptOptions: { cwd: this.cwd },
						}),
						(r) => {
							if (!isRecord(r)) return;
							if (r["message"] !== undefined && r["message"] !== null) {
								messages.push(r["message"]);
							}
							if (r["systemPrompt"] !== undefined) {
								// Preserve the reference runner's defined-key fold; malformed
								// values must reach the typed wire boundary instead of being hidden.
								systemPrompt = r["systemPrompt"];
								systemPromptModified = true;
							}
						},
					);
					const result: Record<string, unknown> = {};
					if (messages.length > 0) result["messages"] = messages;
					if (systemPromptModified) result["systemPrompt"] = systemPrompt;
					await this.client.respond(id, eventType as Method, result);
					return;
				}
				case "message_end": {
					this.assistantDelta.clearActiveAssistant();
					// Rust sends the raw AgentMessage AS the request payload (no
					// `{ message }` wrapper); the payload itself is the running value.
					let currentMessage: unknown = payload;
					let modified = false;
					await this.runHooks(eventType, () => ({ type: eventType, message: currentMessage }), (r, path) => {
						if (!isRecord(r) || !isRecord(r["message"])) return;
						const replacement = r["message"];
						if (
							isRecord(currentMessage)
							&& replacement["role"] !== currentMessage["role"]
						) {
							this.emitExtensionError(
								path,
								eventType,
								"message_end handlers must return a message with the same role",
							);
							return;
						}
						currentMessage = replacement;
						modified = true;
					});
					await this.client.respond(id, eventType as Method, {
						message: modified ? currentMessage : undefined,
					});
					return;
				}
				case "input": {
					let text = payload["text"];
					let images = payload["images"];
					const originalImages =
						images === undefined ? undefined : cloneJsonValue("input.images", images);
					let handled = false;
					await this.runHooks(eventType, () => ({ type: eventType, ...payload, text, images }), (r) => {
						if (!isRecord(r)) return;
						if (r["action"] === "handled") {
							handled = true;
							return false;
						}
						if (r["action"] === "transform") {
							text = r["text"];
							images = r["images"] ?? images;
						}
					});
					if (handled) {
						await this.client.respond(id, eventType as Method, { action: "handled" });
						return;
					}
					const imagesChanged =
						originalImages === undefined
							? images !== undefined
							: images === undefined || !jsonValuesEqual(images, originalImages);
					const changed = text !== payload["text"] || imagesChanged;
					await this.client.respond(
						id,
						eventType as Method,
						changed ? { action: "transform", text, images } : { action: "continue" },
					);
					return;
				}
				case "resources_discover": {
					const discovered: Record<string, unknown[]> = {
						skillPaths: [],
						promptPaths: [],
						themePaths: [],
					};
					await this.runHooks(eventType, { type: eventType, ...payload }, (r, path) => {
						if (!isRecord(r)) return;
						for (const key of ["skillPaths", "promptPaths", "themePaths"] as const) {
							const list = r[key];
							if (!Array.isArray(list)) continue;
							for (const entry of list) {
								if (typeof entry === "string") {
									discovered[key]?.push({ path: entry, extensionPath: path });
								}
							}
						}
					});
					await this.client.respond(id, eventType as Method, discovered);
					return;
				}
				case "session_before_switch":
				case "session_before_fork":
				case "session_before_compact":
				case "session_before_tree": {
					let result: unknown;
					await this.runHooks(eventType, { type: eventType, ...payload }, (r) => {
						if (r === undefined || r === null) return;
						result = r;
						if (isRecord(r) && r["cancel"] === true) return false;
					});
					await this.client.respond(id, eventType as Method, result ?? { ok: true });
					return;
				}
				default: {
					if (eventType === "agent_end" || eventType === "session_shutdown") {
						this.assistantDelta.clearActiveAssistant();
					}
					await this.runHooks(eventType, { type: eventType, ...payload }, () => void 0);
					await this.client.respond(id, eventType as Method, { ok: true });
					return;
				}
			}
		} catch (err) {
			await this.client.respondError(id, eventType as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	// -----------------------------------------------------------------------
	// message_update_delta: compact stream → full message_update hook
	// -----------------------------------------------------------------------

	private async handleMessageUpdateDelta(
		id: number,
		payload: Record<string, unknown>,
	): Promise<void> {
		const event = payload["event"];
		if (!isRecord(event) || typeof event["type"] !== "string") {
			await this.client.respondError(id, "message_update_delta" as Method, {
				code: "invalid_request",
				message: "message_update_delta.event is required",
				retryable: false,
			});
			return;
		}
		try {
			const type = event["type"];
			const final = event["final"];
			if ((type === "done" || type === "error") && isRecord(final)) {
				const message = structuredClone(final);
				const assistantMessageEvent = type === "done"
					? { type, reason: event["reason"], message }
					: { type, reason: event["reason"], error: message };
				this.assistantDelta.clearActiveAssistant();
				let result: unknown;
				await this.runHooks(
					"message_update",
					{ type: "message_update", message, assistantMessageEvent },
					(r) => {
						// CancelWire short-circuit — same fold as session_before_*;
						// any other return keeps the `{ ok: true }` response.
						if (!isRecord(r) || r["cancel"] !== true) return;
						result = r;
						return false;
					},
				);
				await this.client.respond(
					id,
					"message_update_delta" as Method,
					result ?? { ok: true },
				);
				return;
			}

			this.assistantDelta.applyAssistantDelta(event);
			const activeAssistant = this.assistantDelta.getActiveAssistant();
			if (activeAssistant === undefined) {
				throw new Error("message update arrived before assistant start");
			}
			const message = structuredClone(activeAssistant);
			const assistantMessageEvent = this.assistantDelta.expandAssistantEvent(event, message);
			let result: unknown;
			await this.runHooks(
				"message_update",
				{ type: "message_update", message, assistantMessageEvent },
				(r) => {
					// CancelWire short-circuit — same fold as session_before_*;
					// any other return keeps the `{ ok: true }` response.
					if (!isRecord(r) || r["cancel"] !== true) return;
					result = r;
					return false;
				},
			);
			await this.client.respond(
				id,
				"message_update_delta" as Method,
				result ?? { ok: true },
			);
		} catch (error) {
			await this.client.respondError(id, "message_update_delta" as Method, {
				code: "extension_error",
				message: error instanceof Error ? error.message : String(error),
				retryable: false,
			});
		}
	}


	// -----------------------------------------------------------------------
	// Control events, errors, shutdown
	// -----------------------------------------------------------------------

	private handleControlEvent(frame: Frame): void {
		if (frame.method === "session.update") {
			const payload = frame.payload;
			this.systemPrompt = isRecord(payload) && typeof payload["systemPrompt"] === "string"
				? payload["systemPrompt"]
				: "";
			return;
		}
		if (frame.method !== "tool.cancel" && frame.method !== "provider.cancel") {
			return;
		}
		const payload = frame.payload;
		if (!isRecord(payload)) return;
		const requestId = typeof payload["id"] === "number" ? payload["id"] : undefined;
		if (requestId === undefined) return;
		this.inFlightTools.get(requestId)?.abort();
		this.inFlightProviders.get(requestId)?.abort();
	}

	private emitExtensionError(path: string, event: string, message: string): void {
		this.client.send({
			id: 0,
			kind: "event",
			method: "extensionError",
			payload: {
				code: "extension_error",
				message: `[${path}] ${event}: ${message}`,
				retryable: false,
			},
		}).catch(() => void 0);
	}

	private respondError(frame: Frame, message: string): void {
		this.client.respondError(frame.id, frame.method as Method, {
			code: "extension_error",
			message,
			retryable: false,
		}).catch(() => void 0);
	}

	private terminate(reason: string): void {
		this.assistantDelta.clearActiveAssistant();
		console.error(`[lean] fatal: ${reason}`);
		this.dispose(reason);
	}

	dispose(reason = "lean runner disposed"): void {
		if (this.state === RunnerState.DISPOSED) return;
		this.assistantDelta.clearActiveAssistant();
		this.state = RunnerState.DISPOSED;
		for (const controller of this.inFlightTools.values()) controller.abort();
		this.inFlightTools.clear();
		for (const controller of this.inFlightProviders.values()) controller.abort();
		this.inFlightProviders.clear();
		this.shortcutAbortController.abort();
		this.client.dispose(reason);
	}
}
