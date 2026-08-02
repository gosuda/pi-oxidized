/**
 * Rejection fixture: a Mode-1 compat factory function as the default export.
 * The lean runner must reject this unknown module surface with a
 * per-extension load error, not a host failure.
 */
export default function compatFactory(pi) {
	pi.registerTool({ name: "noop" });
}
