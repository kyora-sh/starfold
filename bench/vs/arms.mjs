import { ARMS } from './config.mjs';

/** Suffix of an A/A twin: the same binary and settings as its arm, run as an arm of its own. */
export const TWIN = '~aa';

export const baseArm = (name) => (name.endsWith(TWIN) ? name.slice(0, -TWIN.length) : name);

/** An arm's settings; a twin has its arm's. */
export const armOf = (name) => ARMS[baseArm(name)];

/** The arms a set runs. With the A/A on (the default), the first arm also runs as its twin, so
 * every set shows what the same binary against itself looks like on these VMs. */
export function expandArms(arms, aa = true) {
	if (!aa || !arms.length || arms.includes(arms[0] + TWIN)) return [...arms];
	return [arms[0], arms[0] + TWIN, ...arms.slice(1)];
}

/** The order the arms run in, in round `round` of workload `w`. Run `run` of `rounds`-round runs
 * starts where the one before stopped, so over a set's runs every arm takes every position. */
export function roundOrder(arms, round, w = 0, run = 0, rounds = 0) {
	const s = round + w + run * rounds;
	return arms.map((_, k) => arms[(k + s) % arms.length]);
}
