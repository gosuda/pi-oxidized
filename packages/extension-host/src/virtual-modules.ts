/**
 * Virtual module resolution for extension loading via jiti.
 *
 * Mirrors the reference loader's dual strategy exactly:
 * - Compiled sidecar: the reference packages are STATICALLY imported below so
 *   Bun bundles them (and their npm deps) into the binary; jiti serves them
 *   through `virtualModules`. The shipped binary needs no reference sources
 *   on disk.
 * - Source mode (`bun test` / fixtures): jiti `alias` maps every specifier to
 *   the pinned reference source files, keeping fresh per-load evaluation.
 *
 * Legacy `@mariozechner/*` names alias to their `@earendil-works/*`
 * counterparts. `@sinclair/typebox*` aliases to modern `typebox*`.
 */

import { createJiti, type Jiti } from "jiti/static";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
// Static imports so Bun bundles these into the compiled binary, matching the
// reference loader's VIRTUAL_MODULES. The bunfig resolver maps the pi-*
// specifiers to reference source; `pi-coding-agent-full` maps to the
// coding-agent FULL package index (the org-scoped specifier resolves to the
// extensions subset the host itself consumes).
import * as _bundledPiAgentCore from "@earendil-works/pi-agent-core";
import * as _bundledPiAiCompat from "@earendil-works/pi-ai/compat";
import * as _bundledPiAiOauth from "@earendil-works/pi-ai/oauth";
import * as _bundledPiAiProviders from "@earendil-works/pi-ai/providers/all";
import * as _bundledPiTui from "@earendil-works/pi-tui";
import * as _bundledTypebox from "typebox";
import * as _bundledTypeboxCompile from "typebox/compile";
import * as _bundledTypeboxValue from "typebox/value";
import * as _bundledPiCodingAgent from "pi-coding-agent-full";

const __dirname = dirname(fileURLToPath(import.meta.url));

/** Whether this module runs from a compiled Bun binary (`/$bunfs` vfs). */
const COMPILED = __dirname.startsWith("/$bunfs") || __dirname === "/";
/** Bundled module instances served to extensions in compiled mode. */
function getVirtualModules(): Record<string, unknown> {
	const modules: Record<string, unknown> = {
		typebox: _bundledTypebox,
		"typebox/compile": _bundledTypeboxCompile,
		"typebox/value": _bundledTypeboxValue,
		"@sinclair/typebox": _bundledTypebox,
		"@sinclair/typebox/compile": _bundledTypeboxCompile,
		"@sinclair/typebox/value": _bundledTypeboxValue,
		"@earendil-works/pi-agent-core": _bundledPiAgentCore,
		"@earendil-works/pi-tui": _bundledPiTui,
		"@earendil-works/pi-ai": _bundledPiAiCompat,
		"@earendil-works/pi-ai/compat": _bundledPiAiCompat,
		"@earendil-works/pi-ai/oauth": _bundledPiAiOauth,
		"@earendil-works/pi-ai/providers/all": _bundledPiAiProviders,
		"@earendil-works/pi-coding-agent": _bundledPiCodingAgent,
	};
	for (const [name, module] of Object.entries({ ...modules })) {
		if (name.startsWith("@earendil-works/")) {
			modules[name.replace("@earendil-works/", "@mariozechner/")] = module;
		}
	}
	return modules;
}

/** Reference packages root for source-mode alias resolution. */
const REF_ROOT = resolve(__dirname, "..", "..", "..", ".references", "pi", "packages");

/** Map every extension-importable specifier to reference source (source mode). */
export function getExtensionAliases(): Record<string, string> {
	// Full package index, matching upstream's _bundledPiCodingAgent target.
	const codingAgent = `${REF_ROOT}/coding-agent/src/index.ts`;
	const agent = `${REF_ROOT}/agent/src/index.ts`;
	const tui = `${REF_ROOT}/tui/src/index.ts`;
	const aiCompat = `${REF_ROOT}/ai/src/compat.ts`;
	const aiOauth = `${REF_ROOT}/ai/src/oauth.ts`;
	const aiProviders = `${REF_ROOT}/ai/src/providers/all.ts`;
	// The reference loader resolves typebox via require.resolve from the
	// coding-agent package; bun hoists that copy to the workspace root.
	const typeboxRoot = `${REF_ROOT}/../node_modules/typebox/build`;
	const typebox = `${typeboxRoot}/index.mjs`;
	const typeboxCompile = `${typeboxRoot}/compile/index.mjs`;
	const typeboxValue = `${typeboxRoot}/value/index.mjs`;

	return {
		"@earendil-works/pi-coding-agent": codingAgent,
		"@earendil-works/pi-agent-core": agent,
		"@earendil-works/pi-tui": tui,
		"@earendil-works/pi-ai": aiCompat,
		"@earendil-works/pi-ai/compat": aiCompat,
		"@earendil-works/pi-ai/oauth": aiOauth,
		"@earendil-works/pi-ai/providers/all": aiProviders,

		"@mariozechner/pi-coding-agent": codingAgent,
		"@mariozechner/pi-agent-core": agent,
		"@mariozechner/pi-tui": tui,
		"@mariozechner/pi-ai": aiCompat,
		"@mariozechner/pi-ai/compat": aiCompat,
		"@mariozechner/pi-ai/oauth": aiOauth,
		"@mariozechner/pi-ai/providers/all": aiProviders,

		typebox,
		"typebox/compile": typeboxCompile,
		"typebox/value": typeboxValue,
		"@sinclair/typebox": typebox,
		"@sinclair/typebox/compile": typeboxCompile,
		"@sinclair/typebox/value": typeboxValue,
	};
}

/**
 * Create a jiti instance configured for loading TypeScript extensions.
 *
 * Compiled binaries serve the statically bundled modules; source mode aliases
 * to the reference files. `moduleCache: false` ensures each load gets a fresh
 * module evaluation (matching the reference loader).
 */
export function createExtensionJiti(): Jiti {
	if (COMPILED) {
		return createJiti(__dirname, {
			moduleCache: false,
			virtualModules: getVirtualModules(),
		});
	}
	return createJiti(__dirname, {
		moduleCache: false,
		alias: getExtensionAliases(),
	});
}
