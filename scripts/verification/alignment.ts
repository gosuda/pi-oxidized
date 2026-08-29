#!/usr/bin/env bun
/**
 * Alignment witness suite (VER-ALIGN, issue #145).
 *
 * Freezes the verification/workflow reference pin and the live
 * `.references/pi` checkout to the canonical baseline SHA, and checks that
 * the portable seven-tool schema selection contract still holds against the
 * canonical registry surface.
 */

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { REQUIRED_TOOL_NAMES, loadCanonicalToolRegistry, selectPortableToolParameters } from "../generate-tool-schemas.ts";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

/** Settled upstream baseline that every verification/workflow pin must name. */
export const CANONICAL_REFERENCE_SHA = "8fa7eebd235355522c8104166b4f1f959b4e2f10";

/** Retired pin that must not remain in owned verification/workflow literals. */
export const STALE_REFERENCE_SHA = "4488ad55c18f07ae89a489096c90de8667b3adfb";

/** Owned files that carry the reference pin literal. */
export const PIN_LITERAL_PATHS = [
	".github/workflows/release-verification.yml",
	"scripts/reconstruct-provider-data.ts",
] as const;

export interface AlignmentInputs {
	readonly files: Readonly<Record<string, string>>;
	readonly referenceHead: string;
	readonly registryTools: Readonly<Record<string, unknown>>;
}

/** Fail when an owned pin file lacks the canonical SHA or still names the stale one. */
export function verifyPinLiterals(files: Readonly<Record<string, string>>): string[] {
	const problems: string[] = [];
	for (const path of PIN_LITERAL_PATHS) {
		const body = files[path];
		if (body === undefined) {
			problems.push(`${path} is not readable`);
			continue;
		}
		if (!body.includes(CANONICAL_REFERENCE_SHA)) {
			problems.push(`${path} does not contain canonical reference SHA ${CANONICAL_REFERENCE_SHA}`);
		}
		if (body.includes(STALE_REFERENCE_SHA)) {
			problems.push(`${path} still contains stale reference SHA ${STALE_REFERENCE_SHA}`);
		}
	}
	const workflow = files[".github/workflows/release-verification.yml"];
	if (workflow !== undefined) {
		const occurrences = workflow.split(CANONICAL_REFERENCE_SHA).length - 1;
		if (occurrences < 2) {
			problems.push(
				`.github/workflows/release-verification.yml must pin ${CANONICAL_REFERENCE_SHA} in both checkout ref and rev-parse assertion`,
			);
		}
	}
	return problems;
}

/** Fail when the checked-out reference HEAD is not the canonical baseline. */
export function verifyReferenceCheckout(headSha: string): string[] {
	if (headSha !== CANONICAL_REFERENCE_SHA) {
		return [
			`.references/pi HEAD is ${headSha === "" ? "(empty)" : headSha}, expected ${CANONICAL_REFERENCE_SHA}`,
		];
	}
	return [];
}

/**
 * Fail when the registry cannot supply the seven portable tools, or when
 * selection accidentally keeps a reference-only platform tool.
 */
export function verifyPortableToolSelection(registryTools: Readonly<Record<string, unknown>>): string[] {
	const problems: string[] = [];
	let selected: Record<string, unknown>;
	try {
		selected = selectPortableToolParameters({ ...registryTools });
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		return [`portable tool selection failed: ${detail}`];
	}
	const selectedNames = Object.keys(selected).sort();
	const expected = [...REQUIRED_TOOL_NAMES].sort();
	if (selectedNames.join("\0") !== expected.join("\0")) {
		problems.push(
			`portable tool selection mismatch (expected ${expected.join(", ")}; got ${selectedNames.join(", ")})`,
		);
	}
	for (const name of Object.keys(registryTools)) {
		if (!(REQUIRED_TOOL_NAMES as readonly string[]).includes(name) && Object.hasOwn(selected, name)) {
			problems.push(`portable tool selection retained reference-only tool ${name}`);
		}
	}
	return problems;
}

export function readReferenceHead(root: string): string {
	try {
		return execFileSync("git", ["-C", join(root, ".references/pi"), "rev-parse", "HEAD"], {
			encoding: "utf8",
		}).trim();
	} catch {
		return "";
	}
}

export async function loadAlignmentInputs(root: string): Promise<AlignmentInputs> {
	const files: Record<string, string> = {};
	for (const path of PIN_LITERAL_PATHS) {
		try {
			files[path] = readFileSync(join(root, path), "utf8");
		} catch {
			// verifyPinLiterals reports missing files
		}
	}
	const { definitions } = await loadCanonicalToolRegistry();
	return {
		files,
		referenceHead: readReferenceHead(root),
		registryTools: definitions,
	};
}

/** Run every alignment witness; empty means green. */
export function runAlignmentWitnesses(inputs: AlignmentInputs): string[] {
	return [
		...verifyPinLiterals(inputs.files).map((problem) => `[pin-literals] ${problem}`),
		...verifyReferenceCheckout(inputs.referenceHead).map((problem) => `[reference-checkout] ${problem}`),
		...verifyPortableToolSelection(inputs.registryTools).map((problem) => `[portable-tools] ${problem}`),
	];
}

async function main(): Promise<void> {
	const violations = runAlignmentWitnesses(await loadAlignmentInputs(REPO_ROOT));
	if (violations.length > 0) {
		console.error(`alignment witness suite failed with ${violations.length} violation(s):`);
		for (const violation of violations) console.error(`  - ${violation}`);
		process.exit(1);
	}
	process.stdout.write("ALIGNMENT_WITNESSES_OK\n");
}

if (import.meta.main) main();
