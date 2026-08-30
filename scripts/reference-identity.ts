/**
 * Reference identity authority leaf.
 *
 * Single source of truth for upstream reference checkouts. Active generators
 * and verifiers may read only the canonical `.references/pi-2.0` tree. The
 * legacy `.references/pi` identity remains only as a historical witness for
 * alignment regressions. The retired pin must not reappear in an active path.
 *
 * Dependency-free by contract: node stdlib only, no project imports, so every
 * consumer (including generated-data tooling) can import it without closure
 * risk. Active consumers import the canonical identity from here and call
 * {@link assertCanonicalReference} before their first read of reference data.
 */
import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";

/** Canonical reference checkout, repo-relative. */
export const CANONICAL_REFERENCE_ROOT = ".references/pi-2.0";
/** Exact commit the canonical checkout must sit at. */
export const CANONICAL_REFERENCE_SHA = "853a80d26c90a14c1886f0ebb8ffaae133ca2185";
export const CANONICAL_REFERENCE_SHA_SHORT = CANONICAL_REFERENCE_SHA.slice(0, 8);
/** Legacy reference checkout, repo-relative. Never an active consumer target. */
export const LEGACY_REFERENCE_ROOT = ".references/pi"; // historical witness: rejected root
export const LEGACY_REFERENCE_SHA = "8fa7eebd235355522c8104166b4f1f959b4e2f10"; // historical witness: rejected SHA
export const LEGACY_REFERENCE_SHA_SHORT = LEGACY_REFERENCE_SHA.slice(0, 8);
/** Pin retired before the pi-2.0 cut; must never appear in an active path. */
export const RETIRED_REFERENCE_SHA = "4488ad55c18f07ae89a489096c90de8667b3adfb"; // historical witness: retired SHA
export const RETIRED_REFERENCE_SHA_SHORT = RETIRED_REFERENCE_SHA.slice(0, 8);

/** Absolute harness repository root, derived from this file's location. */
export const REPOSITORY_ROOT = resolve(import.meta.dirname, "..");

const FULL_SHA_PATTERN = /^[0-9a-f]{40}$/;

/** Absolute path of the canonical reference checkout under `repoRoot`. */
export function canonicalReferenceRoot(repoRoot: string = REPOSITORY_ROOT): string {
	return join(repoRoot, CANONICAL_REFERENCE_ROOT);
}

/**
 * HEAD commit of a reference checkout. Fail-closed: throws when the checkout
 * is missing, is not a git repository, or reports anything other than one
 * full lowercase SHA. Never returns an empty string or a partial pin.
 */
export function readReferenceHead(referenceRoot: string): string {
	let head: string;
	try {
		head = execFileSync("git", ["-C", referenceRoot, "rev-parse", "HEAD"], {
			encoding: "utf8",
		}).trim();
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		throw new Error(
			`reference identity unreadable: git rev-parse HEAD failed for ${referenceRoot}: ${detail}`,
		);
	}
	if (!FULL_SHA_PATTERN.test(head)) {
		throw new Error(
			`reference identity unreadable: ${referenceRoot} HEAD is not a full SHA (got ${JSON.stringify(head)})`,
		);
	}
	return head;
}

/**
 * Fail-closed gate for reference consumers: verifies the canonical checkout
 * sits at the exact pinned SHA before any reference data is read. Throws with
 * the observed HEAD on any mismatch; there is no fallback. Returns the
 * verified HEAD.
 */
export function assertCanonicalReference(repoRoot: string = REPOSITORY_ROOT): string {
	const root = canonicalReferenceRoot(repoRoot);
	const head = readReferenceHead(root);
	if (head !== CANONICAL_REFERENCE_SHA) {
		throw new Error(
			`canonical reference mismatch: ${root} HEAD ${head} != pinned ${CANONICAL_REFERENCE_SHA}; move the checkout to the pinned commit or re-pin scripts/reference-identity.ts`,
		);
	}
	return head;
}
