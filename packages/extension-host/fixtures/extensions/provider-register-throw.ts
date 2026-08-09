/**
 * Fixture: registers "throw_provider" then throws an error.
 * Used to verify that a register-then-throw replacement leaks nothing
 * and preserves the old provider state.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function providerRegisterThrow(pi: ExtensionAPI): void {
	pi.registerProvider("throw_provider", {
		baseUrl: "https://throw.example",
		api: "custom",
		streamSimple: () => {
			throw new Error("should not be called");
		},
	});
	throw new Error("factory-throws-after-register");
}
