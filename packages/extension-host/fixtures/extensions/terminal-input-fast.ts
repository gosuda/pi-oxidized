/**
 * Fast onTerminalInput handler: rewrites lowercase a→A and consumes "x".
 * Must stay well under the 4 ms deadline (target <5 ms p99).
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function terminalInputFastExtension(pi: ExtensionAPI): void {
	pi.on("session_start", (_event, ctx) => {
		if (!ctx.hasUI) return;
		ctx.ui.onTerminalInput((data) => {
			if (data === "x") {
				return { consume: true, data: "x" };
			}
			if (data === "a") {
				return { consume: false, data: "A" };
			}
			return undefined;
		});
	});
}
