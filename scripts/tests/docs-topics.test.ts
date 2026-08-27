import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
	FENCE_MARKER_PREFIX,
	INDEX_PENDING_BEGIN,
	INDEX_PENDING_END,
	INDEX_SHIPPED_BEGIN,
	INDEX_SHIPPED_END,
	MANIFEST_SCHEMA,
	TERMINAL_VISUAL_TOPICS,
	checkDocsTopics,
	enumerateCorpusTopics,
	type DocsTopicsContext,
} from "../verification/docs-topics.ts";

const REFERENCE_PIN = "8fa7eebd235355522c8104166b4f1f959b4e2f10";

/** Scratch corpus: one coding-agent topic, one agent-core topic, one terminal topic. */
const SCRATCH_TOPICS = [
	{ slug: "alpha", dir: "packages/coding-agent/docs" },
	{ slug: "omega", dir: "packages/agent/docs" },
	{ slug: "tui", dir: "packages/coding-agent/docs" },
] as const;

interface ScratchOptions {
	readonly indexBody?: string;
	readonly alphaDoc?: string;
	readonly alphaManifest?: string;
	readonly ledgerRows?: DocsTopicsContext["ledgerRows"];
	readonly freshRowIds?: readonly string[];
	readonly extraCorpusFile?: string;
}

function topicDoc(body: string): string {
	return [
		"# Topic",
		"",
		"```text",
		...body.split("\n"),
		"<!-- doc-c:fence=alpha.01 source=target/verification/docs-topics/cli-help/pi--version.txt -->",
		"```",
		"",
	].join("\n");
}

function alphaManifest(overrides: Partial<Record<string, unknown>> = {}): string {
	return JSON.stringify({
		schema: MANIFEST_SCHEMA,
		topic: "alpha",
		doc: "docs/alpha.md",
		referenceSource: ".references/pi/packages/coding-agent/docs/alpha.md",
		transcripts: [
			{
				kind: "cli-help",
				source: "target/verification/docs-topics/cli-help/pi--version.txt",
				producer: "target/release/pi --version",
			},
		],
		claims: [
			{
				rowId: "dc-claim-alpha-1",
				source: "target/verification/docs-topics/cli-help/pi--version.txt",
				claim: "0.1.0",
			},
		],
		pending: [],
		...overrides,
	});
}

function greenIndex(): string {
	return [
		"# Index",
		"",
		INDEX_SHIPPED_BEGIN,
		"- [alpha](alpha.md) — shipped",
		"- [omega](omega.md) — shipped",
		INDEX_SHIPPED_END,
		"",
		INDEX_PENDING_BEGIN,
		"- tui — terminal-visual gate — TUI-CLOSE",
		INDEX_PENDING_END,
		"",
	].join("\n");
}

function baseLedgerRows(): DocsTopicsContext["ledgerRows"] {
	return [
		{
			id: "dc-index",
			surface: "docs/index.md",
			owner: "DOC-C",
			class: "review-only-prose",
			params: { source: "docs/index.md" },
		},
		{
			id: "dc-topic-alpha",
			surface: "docs/alpha.md",
			owner: "DOC-C",
			class: "review-only-prose",
			params: { source: "docs/alpha.md" },
		},
		{
			id: "dc-topic-omega",
			surface: "docs/omega.md",
			owner: "DOC-C",
			class: "review-only-prose",
			params: { source: "docs/omega.md" },
		},
		{
			id: "dc-fence-alpha.01",
			surface: "docs/alpha.md#fence/alpha.01",
			owner: "DOC-C",
			class: "fenced-compile",
			params: { topic: "docs/alpha.md", fenceMarker: `${FENCE_MARKER_PREFIX}alpha.01` },
		},
		{
			id: "dc-claim-alpha-1",
			surface: "target/verification/docs-topics/cli-help/pi--version.txt#claim",
			owner: "DOC-C",
			class: "transcript-claim",
			params: { source: "target/verification/docs-topics/cli-help/pi--version.txt", claim: "0.1.0" },
		},
	];
}

/** Build a complete scratch repo root with a green DOC-C state, then mutate it. */
function buildScratch(options: ScratchOptions = {}): string {
	const root = mkdtempSync(join(tmpdir(), "doc-c-"));
	for (const topic of SCRATCH_TOPICS) {
		const dir = join(root, ".references/pi", topic.dir);
		mkdirSync(dir, { recursive: true });
		writeFileSync(join(dir, `${topic.slug}.md`), `# ${topic.slug}\n`);
	}
	if (options.extraCorpusFile !== undefined) {
		const dir = join(root, ".references/pi/packages/coding-agent/docs");
		writeFileSync(join(dir, options.extraCorpusFile), "# extra\n");
	}
	mkdirSync(join(root, "docs/evidence"), { recursive: true });
	writeFileSync(join(root, "docs/index.md"), options.indexBody ?? greenIndex());
	writeFileSync(join(root, "docs/alpha.md"), options.alphaDoc ?? topicDoc("0.1.0"));
	writeFileSync(join(root, "docs/omega.md"), "# Omega\n");
	writeFileSync(join(root, "docs/evidence/alpha.json"), options.alphaManifest ?? alphaManifest());
	writeFileSync(join(root, "docs/evidence/omega.json"), alphaManifest({
		topic: "omega",
		doc: "docs/omega.md",
		referenceSource: ".references/pi/packages/agent/docs/omega.md",
	}));
	const snapDir = join(root, "target/verification/docs-topics/cli-help");
	mkdirSync(snapDir, { recursive: true });
	writeFileSync(join(snapDir, "pi--version.txt"), "0.1.0\n");
	writeFileSync(join(snapDir, "pi--help.txt"), "pi - AI coding assistant\n  --print, -p\n");
	return root;
}

function check(root: string, mutations: Partial<DocsTopicsContext> = {}): readonly string[] {
	return checkDocsTopics({
		root,
		referencePin: REFERENCE_PIN,
		ledgerRows: mutations.ledgerRows ?? baseLedgerRows(),
		freshRowIds: mutations.freshRowIds ?? new Set(["dc-topic-alpha", "dc-topic-omega", "dc-fence-alpha.01"]),
		sidecarDir: join(root, "target/verification/docs-evidence"),
	});
}

describe("doc-c green path", () => {
	test("scratch corpus with matching index, manifests, and rows passes", () => {
		const root = buildScratch();
		try {
			expect(check(root)).toEqual([]);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("corpus enumeration reads the reference tree at checker time", () => {
		const root = buildScratch({ extraCorpusFile: "zeta.md" });
		try {
			const slugs = enumerateCorpusTopics(root).map((t) => t.slug);
			expect(slugs).toContain("zeta");
			const problems = check(root);
			expect(problems.some((p) => p.includes("[doc-c-index]") && p.includes("zeta"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
});

describe("doc-c index mutations", () => {
	test("dropping a shipped topic from the index fails", () => {
		const index = greenIndex().replace("- [omega](omega.md) — shipped\n", "");
		const root = buildScratch({ indexBody: index });
		try {
			expect(check(root).some((p) => p.startsWith("[doc-c-index]"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a pending topic with an unknown reason fails", () => {
		const index = greenIndex().replace("TUI-CLOSE", "whenever-feels-right");
		const root = buildScratch({ indexBody: index });
		try {
			expect(check(root).some((p) => p.includes("unknown reason"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
});

describe("doc-c terminal gate", () => {
	test("shipping a terminal-visual topic fails while the gate is locked", () => {
		const index = [
			INDEX_SHIPPED_BEGIN,
			"- [alpha](alpha.md) — shipped",
			"- [omega](omega.md) — shipped",
			"- [tui](tui.md) — shipped",
			INDEX_SHIPPED_END,
			"",
			INDEX_PENDING_BEGIN,
			INDEX_PENDING_END,
			"",
		].join("\n");
		const root = buildScratch({ indexBody: index });
		try {
			expect(check(root).some((p) => p.startsWith("[doc-c-terminal-gate]"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a DOC-C ledger row registering a terminal topic fails", () => {
		const rows = [
			...baseLedgerRows(),
			{
				id: "dc-topic-tui",
				surface: "docs/tui.md",
				owner: "DOC-C",
				class: "review-only-prose",
				params: { source: "docs/tui.md" },
			},
		];
		const root = buildScratch();
		// mutate via check: buildScratch ignores ledger options
		try {
			expect(check(root, { ledgerRows: rows }).some((p) => p.includes("dc-topic-tui") && p.startsWith("[doc-c-terminal-gate]"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a manifest for a locked terminal topic fails", () => {
		const root = buildScratch();
		try {
			writeFileSync(join(root, "docs/evidence/tui.json"), alphaManifest({ topic: "tui" }));
			expect(check(root).some((p) => p.includes("locked terminal topic"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("terminal-visual topics are exactly the five locked slugs", () => {
		expect([...TERMINAL_VISUAL_TOPICS].sort()).toEqual(["terminal-setup", "termux", "tmux", "tui", "windows"]);
	});
});

describe("doc-c fence and import mutations", () => {
	test("an unregistered fenced block fails", () => {
		const doc = "# Topic\n\n```bash\ncargo build --release -p pi\n```\n";
		const root = buildScratch({ alphaDoc: doc });
		try {
			expect(check(root).some((p) => p.includes("unregistered fenced block"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a fenced-compile row without a matching fence fails cross-ledger", () => {
		const rows = [
			...baseLedgerRows(),
			{
				id: "dc-fence-alpha.99",
				surface: "docs/alpha.md#fence/alpha.99",
				owner: "DOC-C",
				class: "fenced-compile",
				params: { topic: "docs/alpha.md", fenceMarker: `${FENCE_MARKER_PREFIX}alpha.99` },
			},
		];
		const root = buildScratch();
		try {
			expect(check(root, { ledgerRows: rows }).some((p) => p.includes("dc-fence-alpha.99") && p.includes("no fence"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a snapshot-derived fence that misquotes its transcript fails", () => {
		const root = buildScratch({ alphaDoc: topicDoc("9.9.9-not-a-real-banner") });
		try {
			expect(check(root).some((p) => p.includes("does not quote") || p.includes("verbatim"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a rust fence importing outside the closed crate set fails", () => {
		const doc = [
			"# Topic",
			"",
			"```rust",
			"use serde_json::Value;",
			"<!-- doc-c:fence=alpha.01 -->",
			"```",
			"",
		].join("\n");
		const root = buildScratch({ alphaDoc: doc });
		try {
			expect(check(root).some((p) => p.includes("[doc-c-imports]") && p.includes("serde_json"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a ts fence importing a forbidden specifier fails", () => {
		const doc = [
			"# Topic",
			"",
			"```ts",
			"import { pi } from \"@earendil-works/pi-coding-agent\";",
			"<!-- doc-c:fence=alpha.01 -->",
			"```",
			"",
		].join("\n");
		const root = buildScratch({ alphaDoc: doc });
		try {
			expect(check(root).some((p) => p.includes("[doc-c-imports]") && p.includes("@earendil-works"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
});

describe("doc-c manifest and sidecar mutations", () => {
	test("a manifest claim naming an unknown ledger row fails", () => {
		const manifest = alphaManifest({
			claims: [{ rowId: "dc-claim-alpha-missing", source: "target/verification/docs-topics/cli-help/pi--version.txt", claim: "0.1.0" }],
		});
		const root = buildScratch({ alphaManifest: manifest });
		try {
			expect(check(root).some((p) => p.includes("dc-claim-alpha-missing"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
	test("a topic with no fresh sidecar from the current run fails", () => {
		const root = buildScratch();
		try {
			expect(check(root, { freshRowIds: new Set<string>() }).some((p) => p.startsWith("[doc-c-sidecar]"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a manifest binding a non-canonical or missing transcript artifact fails", () => {
		const manifest = alphaManifest({
			transcripts: [{ kind: "cli-help", source: "target/verification/docs-topics/cli-help/absent.txt", producer: "x" }],
		});
		const root = buildScratch({ alphaManifest: manifest });
		try {
			expect(check(root).some((p) => p.includes("outside the canonical cli-help artifact paths"))).toBe(true);
			rmSync(join(root, "target/verification/docs-topics/cli-help/pi--version.txt"), { force: true });
			const problems = check(root);
			expect(problems.some((p) => p.includes("transcript artifact missing"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a manifest with a pending item of unknown reason fails", () => {
		const manifest = alphaManifest({ pending: [{ item: "x", reason: "someday" }] });
		const root = buildScratch({ alphaManifest: manifest });
		try {
			expect(check(root).some((p) => p.includes("unknown reason"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("a manifest forking the settled reference pin fails", () => {
		const manifest = `${alphaManifest().slice(0, -1)},"referencePin":"deadbeef"}`;
		const root = buildScratch({ alphaManifest: manifest });
		try {
			expect(check(root).some((p) => p.includes("forks the settled pin"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
	test("a missing topic file or manifest fails", () => {
		const root = buildScratch();
		try {
			rmSync(join(root, "docs/omega.md"), { force: true });
			expect(check(root).some((p) => p.includes("missing ported topic file"))).toBe(true);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
});
