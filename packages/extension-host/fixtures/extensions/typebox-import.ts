/**
 * Typebox alias fixture: imports the schema builder through BOTH specifier
 * families the reference loader aliases (`typebox` and `@sinclair/typebox`)
 * and registers a tool whose parameters come from each import, proving the
 * host jiti alias table resolves them to the bundled reference copy.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { Type as SinclairType } from "@sinclair/typebox";
import { Value } from "typebox/value";

export default function typeboxImportExtension(pi: ExtensionAPI): void {
	const modern = Type.Object({ path: Type.String() });
	const sinclair = SinclairType.Object({ query: SinclairType.String() });
	const valid = Value.Check(modern, { path: "x" });

	pi.registerTool({
		name: "typeboxProbe",
		label: "Typebox Probe",
		description: `valid=${String(valid)}`,
		parameters: modern,
		async execute() {
			return {
				content: [{ type: "text", text: JSON.stringify({ sinclair }) }],
				details: undefined,
			};
		},
	});
}
