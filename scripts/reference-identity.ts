/**
 * Reference identity authority leaf.
 *
 * Single source of truth for the two upstream reference checkouts and every
 * historical SHA retired from them. The native behavioral authority
 * (`.references/pi`) drives all generators and native-behavior verifiers; // historical witness: active native root literal
 * the TypeScript extension compatibility pin (`.references/pi-2.0`) is the
 * frozen upstream that `packages/extension-host` builds against and that the
 * Mode 1 registration-conflict witnesses read. A retired SHA must never
 * appear in an active path; the root paths themselves are both active.
 *
 * Dependency-free by contract: node stdlib only, no project imports, so every
 * consumer (including generated-data tooling) can import it without closure
 * risk. Active consumers import their authority from here and call
 * {@link assertNativeReference} or {@link assertExtensionCompatReference}
 * before their first read of reference data.
 */
import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";

/** Native behavioral authority checkout, repo-relative. */
export const NATIVE_REFERENCE_ROOT = ".references/pi"; // historical witness: active native root literal
/** Exact commit the native checkout must sit at. */
export const NATIVE_REFERENCE_SHA = "95fbc04997eaee961eb673fa7923e9220609ebd5";
/** TypeScript extension compatibility checkout, repo-relative. */
export const EXTENSION_COMPAT_REFERENCE_ROOT = ".references/pi-2.0";
/** Exact commit the extension compatibility checkout must sit at. */
export const EXTENSION_COMPAT_REFERENCE_SHA = "853a80d26c90a14c1886f0ebb8ffaae133ca2185";
/** Historical SHAs retired from both roots; must never appear in an active path. */
export const RETIRED_REFERENCE_SHAS = [
	"8fa7eebd235355522c8104166b4f1f959b4e2f10", // historical witness: retired native SHA
	"4488ad55c18f07ae89a489096c90de8667b3adfb", // historical witness: retired pin
] as const;

/** Absolute harness repository root, derived from this file's location. */
export const REPOSITORY_ROOT = resolve(import.meta.dirname, "..");

const FULL_SHA_PATTERN = /^[0-9a-f]{40}$/;

/** Absolute path of the native reference checkout under `repoRoot`. */
export function nativeReferenceRoot(repoRoot: string = REPOSITORY_ROOT): string {
	return join(repoRoot, NATIVE_REFERENCE_ROOT);
}

/** Absolute path of the extension compatibility checkout under `repoRoot`. */
export function extensionCompatReferenceRoot(repoRoot: string = REPOSITORY_ROOT): string {
	return join(repoRoot, EXTENSION_COMPAT_REFERENCE_ROOT);
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
 * Fail-closed gate for native-behavior consumers: verifies the native
 * checkout sits at the exact pinned SHA before any reference data is read.
 * Throws with the observed HEAD on any mismatch; there is no fallback.
 * Returns the verified HEAD.
 */
export function assertNativeReference(repoRoot: string = REPOSITORY_ROOT): string {
	const root = nativeReferenceRoot(repoRoot);
	const head = readReferenceHead(root);
	if (head !== NATIVE_REFERENCE_SHA) {
		throw new Error(
			`native reference mismatch: ${root} HEAD ${head} != pinned ${NATIVE_REFERENCE_SHA}; move the checkout to the pinned commit or re-pin scripts/reference-identity.ts`,
		);
	}
	return head;
}

/**
 * Fail-closed gate for extension-compatibility consumers: verifies the
 * extension compatibility checkout sits at the exact pinned SHA before any
 * reference data is read. Throws with the observed HEAD on any mismatch;
 * there is no fallback. Returns the verified HEAD.
 */
export function assertExtensionCompatReference(repoRoot: string = REPOSITORY_ROOT): string {
	const root = extensionCompatReferenceRoot(repoRoot);
	const head = readReferenceHead(root);
	if (head !== EXTENSION_COMPAT_REFERENCE_SHA) {
		throw new Error(
			`extension compat reference mismatch: ${root} HEAD ${head} != pinned ${EXTENSION_COMPAT_REFERENCE_SHA}; move the checkout to the pinned commit or re-pin scripts/reference-identity.ts`,
		);
	}
	return head;
}
