/**
 * Bun `--preload` fixture proving the lean module graph never touches the
 * Mode-1 compat graph. Registered as a runtime resolution plugin: every
 * specifier resolved after preload is appended to $LEAN_RESOLVE_LOG, and
 * any specifier from the compat/upstream graph (host.ts, virtual-modules,
 * builtins, upstream @earendil-works/* runtime packages, jiti, typebox and
 * its aliased @sinclair/typebox spellings, node:module / module — createRequire bypasses the preload onResolve
 * hook for its string argument) hard-fails the process.
 * `@earendil-works/pi-tui-protocol` stays legal — it is the shared wire
 * package, not the upstream runtime graph.
 */
import { appendFileSync } from "node:fs";
import { plugin } from "bun";

const LOG_PATH = process.env["LEAN_RESOLVE_LOG"];

const FORBIDDEN =
	/^(?:@earendil-works\/(?:pi-coding-agent|pi-agent-core|pi-ai(?:\/|$)|pi-tui(?!-protocol))|@mariozechner\/|jiti(?:\/|$)|@sinclair\/typebox(?:\/|$)|typebox(?:\/|$)|(?:node:)?module$|(?:[^:]*\/)?(?:host|virtual-modules)\.ts$)/;

plugin({
	name: "lean-forbid-compat-graph",
	setup(build) {
		build.onResolve({ filter: /.*/ }, (args) => {
			if (LOG_PATH !== undefined && LOG_PATH !== "") {
				appendFileSync(LOG_PATH, `${args.path}\t${args.importer}\n`);
			}
			if (FORBIDDEN.test(args.path)) {
				throw new Error(
					`forbidden import in lean mode: ${args.path} (from ${args.importer})`,
				);
			}
			return undefined;
		});
	},
});
