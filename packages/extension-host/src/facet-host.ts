/** Optional Facet integration seam. The integration is supplied by downstream builds. */
export class FacetHostBridge {
	constructor(_options: unknown) {}

	handlesRequest(_method: string): boolean {
		return false;
	}

	async handleRequest(_method: string, _payload: unknown): Promise<unknown> {
		throw new Error("Facet integration is not enabled");
	}

	handleEvent(_method: string, _payload: unknown): void {}

	async dispose(): Promise<void> {}
}
