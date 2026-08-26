import { afterAll, describe, expect, test } from "bun:test";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	EXCLUDED_EXAMPLE_PRODUCTS,
	REQUIRED_SNIPPET_FIXTURES,
	REPO_ROOT,
	classifyFence,
	collectDocFences,
	extractFences,
	inferRustDeps,
	mapCargoDiagnostic,
	mapTscDiagnostic,
	runRustLane,
	runSnippetHarness,
	runTypeScriptLane,
	validateSourcePath,
	verifyNoExcludedExampleProducts,
	verifyRequiredSnippetFixtures,
	wrapRustSnippet,
} from "./snippet-harness.ts";

const temporaryPaths: string[] = [];

afterAll(() => {
	for (const path of temporaryPaths) rmSync(path, { recursive: true, force: true });
});

function temporaryDirectory(prefix: string): string {
	const path = mkdtempSync(join(tmpdir(), prefix));
	temporaryPaths.push(path);
	return path;
}

const NEGATIVE_PATH = "scripts/verification/fixtures/docs-snippets/negative-stale-import.md";
const NEGATIVE_SOURCE = readFileSync(join(REPO_ROOT, NEGATIVE_PATH), "utf8");

describe("validateSourcePath", () => {
	test("accepts repo-relative paths and rejects traversal or absolute paths", () => {
		expect(validateSourcePath(REPO_ROOT, "docs/README.md")).toBe("docs/README.md");
		expect(validateSourcePath(REPO_ROOT, "scripts/verification/fixtures/docs-snippets/rust/pi.md")).toBe(
			"scripts/verification/fixtures/docs-snippets/rust/pi.md",
		);
		expect(validateSourcePath(REPO_ROOT, "../outside.md")).toBeUndefined();
		expect(validateSourcePath(REPO_ROOT, "/tmp/evil.md")).toBeUndefined();
		expect(validateSourcePath(REPO_ROOT, "docs/../../etc/passwd")).toBeUndefined();
	});
});

describe("extractFences", () => {
	test("captures info strings and exact open/body line numbers", () => {
		const source = ["# title", "", "```rust", "fn main() {}", "```", "", "```ts", "const x = 1;", "```"].join("\n");
		const { fences, failures } = extractFences(source, "docs/example.md");
		expect(failures).toEqual([]);
		expect(fences).toEqual([
			{
				docPath: "docs/example.md",
				openLine: 3,
				bodyStartLine: 4,
				infoString: "rust",
				body: "fn main() {}",
			},
			{
				docPath: "docs/example.md",
				openLine: 7,
				bodyStartLine: 8,
				infoString: "ts",
				body: "const x = 1;",
			},
		]);
	});

	test("rejects unclosed fences and backtick-bearing info strings", () => {
		const unclosed = extractFences("```rust\nfn main() {}\n", "docs/open.md");
		expect(unclosed.fences).toEqual([]);
		expect(unclosed.failures.some((item) => item.line === 1 && item.message.includes("unclosed"))).toBe(true);

		const badInfo = extractFences("```rust`bad\nfn main() {}\n```\n", "docs/bad.md");
		expect(badInfo.failures.some((item) => item.message.includes("backticks"))).toBe(true);
	});

	test("preserves empty bodies and indented opening fences", () => {
		const source = ["   ```rust", "```", "", "```ts", "", "```"].join("\n");
		const { fences, failures } = extractFences(source, "docs/empty.md");
		expect(failures).toEqual([]);
		expect(fences.map((fence) => ({ info: fence.infoString, body: fence.body, open: fence.openLine }))).toEqual([
			{ info: "rust", body: "", open: 1 },
			{ info: "ts", body: "", open: 4 },
		]);
	});
});

describe("classifyFence", () => {
	test("routes the supported corpus info strings", () => {
		expect(classifyFence("rust")).toBe("rust");
		expect(classifyFence("rust,no_run")).toBe("rust");
		expect(classifyFence("rust,ignore")).toBe("rust-skip");
		expect(classifyFence("rust,text")).toBe("rust-skip");
		expect(classifyFence("rust,compile_fail")).toBe("unsupported");
		expect(classifyFence("rust,should_panic")).toBe("unsupported");
		expect(classifyFence("ts")).toBe("ts");
		expect(classifyFence("typescript")).toBe("ts");
		expect(classifyFence("bash")).toBe("ignore");
		expect(classifyFence("json")).toBe("ignore");
		expect(classifyFence("text")).toBe("ignore");
	});
});

describe("wrapRustSnippet and inferRustDeps", () => {
	test("wraps fragments, keeps fn main verbatim, and uncomments hidden lines in place", () => {
		expect(wrapRustSnippet("let x = 1;")).toEqual({
			code: "fn main() {\n    let x = 1;\n}\n",
			headerLines: 1,
		});
		expect(wrapRustSnippet("fn main() {\n    let x = 1;\n}")).toEqual({
			code: "fn main() {\n    let x = 1;\n}\n",
			headerLines: 0,
		});
		expect(wrapRustSnippet("# use pi_ai::estimate_text_tokens;\nlet _ = estimate_text_tokens(\"x\");")).toEqual({
			code: "fn main() {\n    use pi_ai::estimate_text_tokens;\n    let _ = estimate_text_tokens(\"x\");\n}\n",
			headerLines: 1,
		});
	});

	test("maps crate tokens and leaves zero-dep snippets empty", () => {
		expect(inferRustDeps("use pi_ai::estimate_text_tokens;")).toEqual(["pi-ai"]);
		expect(inferRustDeps("let _ = pi::VERSION;\nlet _ = pi_ext::protocol::FLAGS_SET_METHOD;").sort()).toEqual([
			"pi",
			"pi-ext",
		]);
		expect(inferRustDeps("let x = 1;")).toEqual([]);
	});
});

describe("diagnostic mapping", () => {
	test("maps cargo and tsc diagnostics to original document lines", () => {
		const fence = {
			docPath: "scripts/verification/fixtures/docs-snippets/rust/pi-ai.md",
			openLine: 3,
			bodyStartLine: 4,
			infoString: "rust",
			body: "use pi_ai::missing;",
		};
		const cargoLine = JSON.stringify({
			reason: "compiler-message",
			message: {
				code: { code: "E0432" },
				message: "unresolved import",
				spans: [{ file_name: "src/bin/snippet_000.rs", line_start: 2, column_start: 99, is_primary: true }],
			},
		});
		const rustMapped = mapCargoDiagnostic(cargoLine, new Map([["snippet_000.rs", { fence, headerLines: 1 }]]));
		expect(rustMapped).toMatchObject({
			docPath: fence.docPath,
			line: 4,
			tool: "rustc",
			code: "E0432",
		});
		expect(rustMapped?.column).toBeUndefined();

		const tsFence = {
			docPath: "scripts/verification/fixtures/docs-snippets/ts/protocol.md",
			openLine: 7,
			bodyStartLine: 8,
			infoString: "ts",
			body: 'import { missing } from "@earendil-works/pi-tui-protocol";',
		};
		expect(
			mapTscDiagnostic(
				"snippet_000.ts(1,10): error TS2305: Module has no exported member 'missing'.",
				new Map([["snippet_000.ts", tsFence]]),
			),
		).toMatchObject({
			docPath: tsFence.docPath,
			line: 8,
			tool: "tsc",
			code: "TS2305",
		});
	});

	test(
		"attributes multiline TypeScript syntax errors to the later body line",
		async () => {
			const body = ["const ok = 1;", "function broken( {", "  return ok;", "}"].join("\n");
			const fence = {
				docPath: "scripts/verification/fixtures/docs-snippets/negative-stale-import.md",
				openLine: 10,
				bodyStartLine: 11,
				infoString: "ts",
				body,
				kind: "ts" as const,
				snippetId: "scripts/verification/fixtures/docs-snippets/negative-stale-import.md:10",
			};
			const result = await runTypeScriptLane(REPO_ROOT, [fence]);
			expect(result.failures.some((item) => item.docPath === fence.docPath && item.line === fence.bodyStartLine)).toBe(false);
			expect(result.failures.some((item) => item.docPath === fence.docPath && item.line > fence.bodyStartLine && item.tool === "tsc")).toBe(true);
		},
		{ timeout: 120_000 },
	);
});

describe("required fixture registry", () => {
	test("live fixture corpus satisfies every required entrypoint contract", () => {
		expect(verifyRequiredSnippetFixtures(REPO_ROOT)).toEqual([]);
		expect(REQUIRED_SNIPPET_FIXTURES.filter((entry) => entry.lane === "rust")).toHaveLength(5);
		expect(REQUIRED_SNIPPET_FIXTURES.filter((entry) => entry.lane === "ts")).toHaveLength(3);
		const readme = readFileSync(join(REPO_ROOT, "scripts/verification/fixtures/docs-snippets/README.md"), "utf8");
		for (const entry of REQUIRED_SNIPPET_FIXTURES) {
			expect(readme.includes(entry.path.replace("scripts/verification/fixtures/docs-snippets/", ""))).toBe(true);
		}
	});

	test("deleting one registered fixture fails the registry witness", () => {
		const root = temporaryDirectory("snippet-registry-");
		const relativeFixtureRoot = "scripts/verification/fixtures/docs-snippets";
		cpSync(join(REPO_ROOT, relativeFixtureRoot), join(root, relativeFixtureRoot), { recursive: true });
		const target = REQUIRED_SNIPPET_FIXTURES[0];
		if (target === undefined) throw new Error("required fixture registry is empty");
		rmSync(join(root, target.path), { force: true });
		const problems = verifyRequiredSnippetFixtures(root);
		expect(problems.some((problem) => problem.includes(target.path) && problem.includes("missing"))).toBe(true);
	});
});

describe("exclusion guard", () => {
	test("rejects excluded product names in fixture trees", () => {
		const root = temporaryDirectory("snippet-exclusion-");
		const fixtureDir = join(root, "scripts/verification/fixtures/docs-snippets");
		mkdirSync(fixtureDir, { recursive: true });
		writeFileSync(join(fixtureDir, "bad.md"), "mentions with-deps product\n");
		expect(verifyNoExcludedExampleProducts(root).some((problem) => problem.includes("with-deps"))).toBe(true);
	});

	test("real fixture corpus stays free of excluded products", () => {
		expect(verifyNoExcludedExampleProducts(REPO_ROOT)).toEqual([]);
		for (const name of EXCLUDED_EXAMPLE_PRODUCTS) {
			expect(NEGATIVE_SOURCE.includes(name)).toBe(false);
		}
	});
});

describe("corpus accounting and determinism", () => {
	test("current docs contribute zero rust/ts fences and fixture collection is sorted", () => {
		const { fences, failures } = collectDocFences(REPO_ROOT);
		expect(failures).toEqual([]);
		expect(fences.filter((fence) => fence.docPath.startsWith("docs/") && (fence.kind === "rust" || fence.kind === "ts"))).toEqual([]);
		const ordered = fences.map((fence) => `${fence.docPath}:${fence.openLine}`);
		expect(ordered).toEqual(
			[...fences]
				.sort((a, b) => a.docPath.localeCompare(b.docPath) || a.openLine - b.openLine)
				.map((fence) => `${fence.docPath}:${fence.openLine}`),
		);
		expect(fences.some((fence) => fence.docPath.includes("negative-"))).toBe(false);
	});

	test("pure collection composition is deterministic", () => {
		const first = collectDocFences(REPO_ROOT);
		const second = collectDocFences(REPO_ROOT);
		expect(second).toEqual(first);
	});
});

describe("snippet harness e2e", () => {
	test(
		"compiles both fixture lanes against live sources",
		async () => {
			const report = await runSnippetHarness(REPO_ROOT);
			expect(report.ok).toBe(true);
			expect(report.violations).toEqual([]);
			expect(report.lanes.rust.documents).toBe(0);
			expect(report.lanes.ts.documents).toBe(0);
			expect(report.lanes.rust.fixtures).toBeGreaterThan(0);
			expect(report.lanes.ts.fixtures).toBeGreaterThan(0);
			expect(report.lanes.rust.compiled).toBe(report.lanes.rust.extracted);
			expect(report.lanes.ts.compiled).toBe(report.lanes.ts.extracted);
			const again = await runSnippetHarness(REPO_ROOT);
			expect(again).toEqual(report);
		},
		{ timeout: 600_000 },
	);

	test(
		"stale imports fail with exact document line attribution",
		async () => {
			const extracted = extractFences(NEGATIVE_SOURCE, NEGATIVE_PATH);
			expect(extracted.failures).toEqual([]);
			const rustFence = extracted.fences.find((fence) => classifyFence(fence.infoString) === "rust");
			const tsFence = extracted.fences.find((fence) => classifyFence(fence.infoString) === "ts");
			expect(rustFence).toBeDefined();
			expect(tsFence).toBeDefined();
			if (rustFence === undefined || tsFence === undefined) throw new Error("negative fixtures missing");

			const rustRegistered = [
				{
					...rustFence,
					kind: "rust" as const,
					snippetId: `${rustFence.docPath}:${rustFence.openLine}`,
				},
			];
			const tsRegistered = [
				{
					...tsFence,
					kind: "ts" as const,
					snippetId: `${tsFence.docPath}:${tsFence.openLine}`,
				},
			];
			const [rust, ts] = await Promise.all([
				runRustLane(REPO_ROOT, rustRegistered),
				runTypeScriptLane(REPO_ROOT, tsRegistered),
			]);
			expect(rust.failures.some((item) => item.docPath === NEGATIVE_PATH && item.line === rustFence.bodyStartLine)).toBe(true);
			expect(ts.failures.some((item) => item.docPath === NEGATIVE_PATH && item.line === tsFence.bodyStartLine)).toBe(true);
		},
		{ timeout: 600_000 },
	);
});
