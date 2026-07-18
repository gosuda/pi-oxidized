/**
 * Deliberately slow onTerminalInput handler: schedules work past the 4 ms
 * deadline so the host times it out once, disables only that handler, and
 * later input stays local. Must yield to the event loop (setTimeout) so the
 * host's deadline timer can fire; a busy-wait would block the timer.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function terminalInputSlowExtension(pi: ExtensionAPI): void {
	pi.on("session_start", (_event, ctx) => {
		if (!ctx.hasUI) return;
		ctx.ui.onTerminalInput((data) => {
			const { promise, resolve } = Promise.withResolvers<{
				consume: boolean;
				data: string;
			}>();
			setTimeout(() => {
				resolve({ consume: true, data: `slow:${data}` });
			}, 20);
			return promise;
		});
	});
}
