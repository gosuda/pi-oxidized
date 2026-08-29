export const NOISE_RELATIVE_SPREAD_LIMIT = 0.2;
export const NOISE_EXIT_CODE = 2;

export interface SpreadStats {
	readonly stddev: number;
	readonly relativeSpread: number | null;
}

export interface NoisyDistribution extends SpreadStats {
	readonly label: string;
	readonly count: number;
	readonly median: number;
}

export const REMEDIATION_LADDER = [
	"pin CPU frequency/governor",
	"isolate the process",
	"widen sample counts",
	"enlarge the input",
] as const;

export function spreadStats(values: readonly number[], median: number): SpreadStats {
	if (values.length === 0) return { stddev: 0, relativeSpread: 0 };
	const mean = values.reduce((sum, value) => sum + value, 0) / values.length;
	const variance = values.reduce((sum, value) => sum + (value - mean) ** 2, 0) / values.length;
	const stddev = Math.sqrt(variance);
	return {
		stddev,
		relativeSpread: median === 0 ? (stddev === 0 ? 0 : null) : stddev / median,
	};
}

export class NoiseRejection extends Error {
	readonly noisy: readonly NoisyDistribution[];

	constructor(noisy: readonly NoisyDistribution[]) {
		super("benchmark distributions are too noisy");
		this.name = "NoiseRejection";
		this.noisy = noisy;
	}
}

export function requireQuiet(distributions: readonly NoisyDistribution[]): void {
	const noisy = distributions.filter(
		(distribution) =>
			distribution.relativeSpread === null ||
			distribution.relativeSpread > NOISE_RELATIVE_SPREAD_LIMIT,
	);
	if (noisy.length > 0) throw new NoiseRejection(noisy);
}

export function formatNoiseRejection(noisy: readonly NoisyDistribution[]): string {
	const distributions = noisy.map((distribution) => {
		const spread =
			distribution.relativeSpread === null
				? "undefined (median zero)"
				: `${(distribution.relativeSpread * 100).toFixed(2)}%`;
		return `${distribution.label}: n=${distribution.count}, median=${distribution.median}, stddev=${distribution.stddev}, relative spread=${spread}`;
	});
	const remediation = REMEDIATION_LADDER.map((step, index) => `${index + 1}. ${step}`);
	return [...distributions, "Remediation:", ...remediation].join("\n");
}
