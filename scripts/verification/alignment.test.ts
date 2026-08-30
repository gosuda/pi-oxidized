import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
	ALIGNMENT_POLICY_PATH,
	CLASSIFIER_FIXTURE_PATH,
	HISTORICAL_LABEL,
	ISSUE_RECORD_PATH,
	ISSUE_RECORD_SHA256,
	LEDGER_CARRIER_PATH,
	LEGACY_ALLOWANCES,
	PIN_LITERAL_PATHS,
	REPO_ROOT,
	loadAlignmentInputs,
	runAlignmentWitnesses,
	scanLegacyIdentity,
	verifyLegacyIdentity,
	verifyPinLiterals,
	verifyPortableToolSelection,
	verifyReferenceCheckout,
} from "./alignment.ts";
import { describe, expect, test } from "bun:test";

import {
	CANONICAL_REFERENCE_ROOT,
	CANONICAL_REFERENCE_SHA,
	LEGACY_REFERENCE_ROOT,
	LEGACY_REFERENCE_SHA,
	LEGACY_REFERENCE_SHA_SHORT,
	RETIRED_REFERENCE_SHA,
	RETIRED_REFERENCE_SHA_SHORT,
} from "../reference-identity.ts";

import {
	REQUIRED_TOOL_NAMES,
	loadCanonicalToolRegistry,
	selectPortableToolParameters,
} from "../generate-tool-schemas.ts";

function parametersFor(names: readonly string[]): Record<string, unknown> {
	const definitions: Record<string, unknown> = {};
	for (const name of names) {
		definitions[name] = { parameters: { type: "object", properties: { [name]: { type: "string" } } } };
	}
	return definitions;
}

describe("VER-ALIGN reference identity", () => {
	test("canonical identity is the settled pi-2.0 baseline", () => {
		expect(CANONICAL_REFERENCE_ROOT).toBe(".references/pi-2.0");
		expect(CANONICAL_REFERENCE_SHA).toBe("853a80d26c90a14c1886f0ebb8ffaae133ca2185");
	});

	test("retired identity literals keep their historical values", () => {
		expect(LEGACY_REFERENCE_ROOT).toBe(".references/pi"); // historical witness: retired root literal
		expect(LEGACY_REFERENCE_SHA).toBe("8fa7eebd235355522c8104166b4f1f959b4e2f10"); // historical witness: legacy full SHA
		expect(LEGACY_REFERENCE_SHA_SHORT).toBe("8fa7eebd"); // historical witness: legacy short SHA
		expect(RETIRED_REFERENCE_SHA).toBe("4488ad55c18f07ae89a489096c90de8667b3adfb"); // historical witness: retired full SHA
		expect(RETIRED_REFERENCE_SHA_SHORT).toBe("4488ad55"); // historical witness: retired short SHA
	});

	test("every legacy witness allowance is closure-ineligible", () => {
		for (const allowance of Object.values(LEGACY_ALLOWANCES)) {
			expect(allowance.closureEligible).toBe(false);
			expect(allowance.reason.length).toBeGreaterThan(0);
			expect(allowance.label.length).toBeGreaterThan(0);
		}
		expect(Object.keys(LEGACY_ALLOWANCES).sort()).toEqual(
			[
				ALIGNMENT_POLICY_PATH,
				CLASSIFIER_FIXTURE_PATH,
				"docs/PERF-R2-workload-surface-ranking.md",
				"docs/PERF-R8-paired-baselines.md",
				"docs/performance/floors/memory-resource-units.md",
				"docs/performance/t11-iterations.md",
				ISSUE_RECORD_PATH,
				"scripts/reference-identity.ts",
			].sort(),
		);
	});
});

describe("VER-ALIGN reference pin carriers", () => {
	test("owned carriers pin the canonical identity exactly", async () => {
		const inputs = await loadAlignmentInputs(REPO_ROOT);
		expect(verifyPinLiterals(inputs.files)).toEqual([]);
		expect(inputs.files[".github/workflows/release-verification.yml"]).toContain(
			CANONICAL_REFERENCE_ROOT,
		);
		expect(inputs.files[".github/workflows/musl-bakeoff.yml"]).toContain(
			CANONICAL_REFERENCE_ROOT,
		);
		const identitySource = inputs.files["scripts/reference-identity.ts"];
		if (identitySource === undefined) throw new Error("reference identity carrier is missing");
		expect(identitySource.split(CANONICAL_REFERENCE_SHA)).toHaveLength(2);
		for (const path of PIN_LITERAL_PATHS) {
			if (path === "scripts/reference-identity.ts") continue;
			expect(inputs.files[path]).not.toContain(LEGACY_REFERENCE_SHA);
			expect(inputs.files[path]).not.toContain(RETIRED_REFERENCE_SHA);
		}
	});

	test("docs-evidence ledger carrier pins the canonical SHA exactly once", async () => {
		const inputs = await loadAlignmentInputs(REPO_ROOT);
		const ledger = inputs.files[LEDGER_CARRIER_PATH];
		if (ledger === undefined) throw new Error("docs-evidence ledger carrier is missing");
		expect((JSON.parse(ledger) as { referencePin: string }).referencePin).toBe(CANONICAL_REFERENCE_SHA);
		expect(ledger.split(CANONICAL_REFERENCE_SHA).length - 1).toBe(1);
		expect(ledger).not.toContain(LEGACY_REFERENCE_SHA);
		expect(ledger).not.toContain(RETIRED_REFERENCE_SHA);
	});

	test("pin witness fails on retired workflow literals", () => {
		const files = {
			".github/workflows/release-verification.yml": `ref: ${RETIRED_REFERENCE_SHA}\nassert ${RETIRED_REFERENCE_SHA}\n`,
			"scripts/reconstruct-provider-data.ts": `// pinned reference ${CANONICAL_REFERENCE_SHA}\n`,
		};
		const problems = verifyPinLiterals(files);
		expect(problems.some((problem) => problem.includes(RETIRED_REFERENCE_SHA))).toBe(true);
	});

	test("pin witness fails on a legacy ledger pin", () => {
		const files = {
			".github/workflows/release-verification.yml": `ref: ${CANONICAL_REFERENCE_SHA}\nassert ${CANONICAL_REFERENCE_SHA}\npath: ${CANONICAL_REFERENCE_ROOT}\n`,
			"scripts/reconstruct-provider-data.ts": `// pinned reference ${CANONICAL_REFERENCE_SHA}\n`,
			[LEDGER_CARRIER_PATH]: JSON.stringify({ schema: "pi.docs.evidence.v1", referencePin: LEGACY_REFERENCE_SHA }),
		};
		const problems = verifyPinLiterals(files);
		expect(
			problems.some((problem) => problem.includes(LEDGER_CARRIER_PATH) && problem.includes(LEGACY_REFERENCE_SHA)),
		).toBe(true);
		expect(
			problems.some((problem) => problem.includes(LEDGER_CARRIER_PATH) && problem.includes("exactly once")),
		).toBe(true);
	});

	test("reference checkout witness accepts only the canonical HEAD", () => {
		expect(verifyReferenceCheckout(CANONICAL_REFERENCE_SHA)).toEqual([]);
		expect(verifyReferenceCheckout(LEGACY_REFERENCE_SHA)).toEqual([
			`${CANONICAL_REFERENCE_ROOT} HEAD is ${LEGACY_REFERENCE_SHA}, expected ${CANONICAL_REFERENCE_SHA}`,
		]);
		expect(verifyReferenceCheckout("")).toEqual([
			`${CANONICAL_REFERENCE_ROOT} HEAD is (missing or unreadable), expected ${CANONICAL_REFERENCE_SHA}`,
		]);
	});
});

describe("VER-ALIGN portable tool selection", () => {
	test("selects the seven portable tools and drops powershell", () => {
		const selected = selectPortableToolParameters(
			parametersFor([...REQUIRED_TOOL_NAMES, "powershell"]),
		);
		expect(Object.keys(selected).sort()).toEqual([...REQUIRED_TOOL_NAMES].sort());
		expect(selected).not.toHaveProperty("powershell");
	});

	test("fails when any required portable tool is absent", () => {
		const incomplete = parametersFor(REQUIRED_TOOL_NAMES.filter((name) => name !== "bash"));
		expect(() => selectPortableToolParameters(incomplete)).toThrow(/lacks required tools: bash/);
	});

	test("fails when a required tool has no parameters schema", () => {
		const definitions = parametersFor(REQUIRED_TOOL_NAMES);
		definitions.edit = { description: "missing parameters" };
		expect(() => selectPortableToolParameters(definitions)).toThrow(
			/reference tool "edit" has no parameters schema/,
		);
	});

	test("portable selection witness tolerates reference-only platform tools", () => {
		const registryTools = parametersFor([...REQUIRED_TOOL_NAMES, "powershell"]);
		expect(verifyPortableToolSelection(registryTools)).toEqual([]);
	});

	test("portable selection witness fails when a required tool is missing", () => {
		const registryTools = parametersFor(REQUIRED_TOOL_NAMES.filter((name) => name !== "ls"));
		expect(
			verifyPortableToolSelection(registryTools).some((problem) => problem.includes("ls")),
		).toBe(true);
	});
});

describe("VER-ALIGN legacy identity classifier", () => {
	test("classifier detects direct and split retired roots", () => {
		expect(
			scanLegacyIdentity(`read("${LEGACY_REFERENCE_ROOT}/README.md")`).map((o) => o.kind),
		).toEqual(["legacy-root-direct"]);
		const splitForm = 'join(repo, ".references", "pi")'; // historical witness: split-root regression literal
		expect(scanLegacyIdentity(`const p = ${splitForm};`).map((o) => o.kind)).toEqual(["legacy-root-split"]);
		expect(
			scanLegacyIdentity(`read("${CANONICAL_REFERENCE_ROOT}/README.md")`),
		).toEqual([]);
	});

	test("classifier separates full and short retired SHAs", () => {
		expect(scanLegacyIdentity(`pin ${LEGACY_REFERENCE_SHA} end`).map((o) => o.kind)).toEqual(["legacy-sha-full"]);
		expect(
			scanLegacyIdentity(`see ${LEGACY_REFERENCE_SHA_SHORT} for the short form`).map((o) => o.kind),
		).toEqual(["legacy-sha-short"]);
		expect(scanLegacyIdentity(`pin ${RETIRED_REFERENCE_SHA} end`).map((o) => o.kind)).toEqual(["retired-sha-full"]);
		expect(
			scanLegacyIdentity(`see ${RETIRED_REFERENCE_SHA_SHORT} for the short form`).map((o) => o.kind),
		).toEqual(["retired-sha-short"]);
		// A short prefix inside a full SHA is never double-counted.
		expect(
			scanLegacyIdentity(`${LEGACY_REFERENCE_SHA}${LEGACY_REFERENCE_SHA_SHORT}`).map((o) => o.kind),
		).toEqual(["legacy-sha-full"]);
	});

	test("classifier rejects unknown legacy occurrences in tracked text", () => {
		const problems = verifyLegacyIdentity(
			{ "scripts/example.ts": `const p = "${LEGACY_REFERENCE_ROOT}/README.md";\n` },
			{},
		);
		expect(problems).toEqual([
			"unclassified legacy legacy-root-direct occurrence at scripts/example.ts:1",
		]);
	});

	test("classifier rejects an extra occurrence beyond the fixture allowance", () => {
		const fixtureAllowance = LEGACY_ALLOWANCES[CLASSIFIER_FIXTURE_PATH];
		if (fixtureAllowance === undefined) throw new Error("fixture allowance is missing");
		const problems = verifyLegacyIdentity(
			{
				[CLASSIFIER_FIXTURE_PATH]: `pin ${LEGACY_REFERENCE_SHA} // ${HISTORICAL_LABEL}\npin ${LEGACY_REFERENCE_SHA} // ${HISTORICAL_LABEL}\n`,
			},
			{ [CLASSIFIER_FIXTURE_PATH]: fixtureAllowance },
		);
		expect(problems.some((p) => p.includes("extra") && p.includes("legacy-sha-full"))).toBe(true);
		expect(problems.some((p) => p.includes("unused"))).toBe(true);
	});

	test("classifier rejects occurrences missing the historical label", () => {
		const fixtureAllowance = LEGACY_ALLOWANCES[CLASSIFIER_FIXTURE_PATH];
		if (fixtureAllowance === undefined) throw new Error("fixture allowance is missing");
		const problems = verifyLegacyIdentity(
			{ [CLASSIFIER_FIXTURE_PATH]: `pin ${LEGACY_REFERENCE_SHA}\n` },
			{ [CLASSIFIER_FIXTURE_PATH]: fixtureAllowance },
		);
		expect(problems.some((p) => p.includes("unlabelled") && p.includes("legacy-sha-full"))).toBe(true);
	});

	test("classifier rejects a drifted immutable issue record", () => {
		const issueAllowance = LEGACY_ALLOWANCES[ISSUE_RECORD_PATH];
		if (issueAllowance === undefined) throw new Error("issue allowance is missing");
		const problems = verifyLegacyIdentity(
			{ [ISSUE_RECORD_PATH]: "{}\n" },
			{ [ISSUE_RECORD_PATH]: issueAllowance },
		);
		expect(problems.some((p) => p.includes("digest drift") && p.includes(ISSUE_RECORD_SHA256))).toBe(true);
	});

	test("classifier rejects closure contamination in DOC-F and PERF-CLOSE sources", () => {
		const docf = verifyLegacyIdentity(
			{ "docs/DOC-F-closure-evidence.md": `witness: ${LEGACY_REFERENCE_ROOT}/README.md\n` },
			{},
		);
		expect(docf.some((p) => p.includes("current DOC-F source consuming a legacy witness"))).toBe(true);
		const issueAllowance = LEGACY_ALLOWANCES[ISSUE_RECORD_PATH];
		if (issueAllowance === undefined) throw new Error("issue allowance is missing");
		const perf = verifyLegacyIdentity(
			{ "docs/performance/PERF-CLOSE-evidence.md": `tickets: ${ISSUE_RECORD_PATH}\n` },
			{ [ISSUE_RECORD_PATH]: issueAllowance },
		);
		expect(perf.some((p) => p.includes("current PERF-CLOSE source consuming a legacy witness"))).toBe(true);
	});

	test("classifier passes the canonical witnesses byte-exactly", () => {
		const trackedFiles = Object.fromEntries(
			Object.keys(LEGACY_ALLOWANCES).map((path) => [
				path,
				readFileSync(join(REPO_ROOT, path), "utf8"),
			]),
		);
		expect(verifyLegacyIdentity(trackedFiles)).toEqual([]);
	});
});

describe("VER-ALIGN acceptance path", () => {
	test("runAlignmentWitnesses is green against the repository", async () => {
		expect(runAlignmentWitnesses(await loadAlignmentInputs(REPO_ROOT))).toEqual([]);
	});

	test("acceptance selection defends against real-registry mutations", async () => {
		const { definitions } = await loadCanonicalToolRegistry();

		// Removing a required real registry entry must fail the acceptance witness.
		const missing = { ...definitions };
		delete missing.ls;
		expect(
			verifyPortableToolSelection(missing).some((problem) => problem.includes("ls")),
		).toBe(true);

		// The acceptance path must never retain a reference-only entry. The real
		// registry does carry platform-only tools (e.g. powershell), so selection
		// proving it drops them is a real, non-tautological mutation defence.
		const selected = selectPortableToolParameters({ ...definitions });
		const retainedReferenceOnly = Object.keys(definitions).filter(
			(name) =>
				!(REQUIRED_TOOL_NAMES as readonly string[]).includes(name) &&
				Object.hasOwn(selected, name),
		);
		expect(retainedReferenceOnly).toEqual([]);
		expect(
			Object.keys(definitions).some(
				(name) => !(REQUIRED_TOOL_NAMES as readonly string[]).includes(name),
			),
		).toBe(true);
	});
});
