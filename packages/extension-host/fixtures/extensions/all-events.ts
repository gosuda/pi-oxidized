/**
 * All-35-events fixture: registers a handler for every lifecycle event type
 * in the ExtensionAPI. Used to verify the REAL ExtensionRunner dispatches all
 * 35 methods without error.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const ALL_EVENTS = [
	"project_trust",
	"resources_discover",
	"session_start",
	"session_info_changed",
	"session_before_switch",
	"session_before_fork",
	"session_before_compact",
	"session_compact",
	"session_shutdown",
	"session_before_tree",
	"session_tree",
	"context",
	"before_provider_request",
	"before_provider_headers",
	"after_provider_response",
	"before_agent_start",
	"agent_start",
	"agent_end",
	"agent_settled",
	"ui_prompt_start",
	"ui_prompt_end",
	"turn_start",
	"turn_end",
	"message_start",
	"message_update",
	"message_end",
	"tool_execution_start",
	"tool_execution_update",
	"tool_execution_end",
	"model_select",
	"thinking_level_select",
	"tool_call",
	"tool_result",
	"user_bash",
	"input",
] as const;

export default function allEventsExtension(pi: ExtensionAPI): void {
	for (const event of ALL_EVENTS) {
		pi.on(event, () => {
			// Observer handler — return void for stream events.
			// For control events, the runner calls specialized emit methods
			// which handle the return value; the generic emit path ignores it.
		});
	}
}

export { ALL_EVENTS };
