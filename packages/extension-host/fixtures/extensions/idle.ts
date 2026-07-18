/**
 * Idle extension: registers no handlers and no widgets. Used to measure the
 * cost of loading many installed-but-dormant extensions.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function idleExtension(_pi: ExtensionAPI): void {
	// Intentionally empty — installed and loaded, zero runtime work.
}
