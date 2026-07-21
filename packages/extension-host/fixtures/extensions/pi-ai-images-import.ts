import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
	generateImages,
	getImageProviders,
	getImagesApiProvider,
} from "@earendil-works/pi-ai";

export default function piAiImagesImportExtension(pi: ExtensionAPI): void {
	const imageProviders = getImageProviders();
	const openRouter = getImagesApiProvider("openrouter-images");
	const description = [
		`generate=${String(typeof generateImages === "function")}`,
		`models=${String(imageProviders.length > 0)}`,
		`api=${String(openRouter?.api === "openrouter-images")}`,
	].join(";");

	pi.registerTool({
		name: "piAiImagesProbe",
		label: "pi-ai images probe",
		description,
		parameters: { type: "object", properties: {}, additionalProperties: false },
		async execute() {
			return {
				content: [{ type: "text", text: description }],
				details: undefined,
			};
		},
	});
}
