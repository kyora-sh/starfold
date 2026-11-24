/** Builders for `Starfold.actions()`: each returns one action of `POST /input`. */

export type Button = 'left' | 'middle' | 'right' | (string & {}) | number;

export type Action =
	| { type: 'move'; x: number; y: number }
	| { type: 'click'; button: Button; count: number; x?: number; y?: number }
	| { type: 'down' | 'up'; button: Button; x?: number; y?: number }
	| { type: 'scroll'; dx: number; dy: number; x?: number; y?: number }
	| { type: 'key' | 'key_down' | 'key_up'; key: string }
	| { type: 'type'; text: string }
	| { type: 'pause'; ms: number };

function at<T extends object>(
	action: T,
	x: number | undefined,
	y: number | undefined
): T {
	if ((x === undefined) !== (y === undefined)) {
		throw new RangeError('give both x and y, or neither');
	}
	return x === undefined ? action : { ...action, x, y };
}

export function move(x: number, y: number): Action {
	return { type: 'move', x, y };
}

export function click(
	x?: number,
	y?: number,
	button: Button = 'left',
	count = 1
): Action {
	return at({ type: 'click', button, count }, x, y);
}

export function down(x?: number, y?: number, button: Button = 'left'): Action {
	return at({ type: 'down', button }, x, y);
}

export function up(x?: number, y?: number, button: Button = 'left'): Action {
	return at({ type: 'up', button }, x, y);
}

/** Wheel notches: positive `dy` scrolls down, positive `dx` right. */
export function scroll(dx = 0, dy = 0, x?: number, y?: number): Action {
	return at({ type: 'scroll', dx, dy }, x, y);
}

/** Press and release keys together: "Enter", "ctrl+c", "ctrl+shift+t", "alt+F4". */
export function key(combo: string): Action {
	return { type: 'key', key: combo };
}

export function keyDown(combo: string): Action {
	return { type: 'key_down', key: combo };
}

export function keyUp(combo: string): Action {
	return { type: 'key_up', key: combo };
}

export function typeText(text: string): Action {
	return { type: 'type', text };
}

export function pause(ms: number): Action {
	return { type: 'pause', ms };
}

/** Press at (x1, y1), move to (x2, y2) in `steps` moves `stepMs` apart, release. */
export function drag(
	x1: number,
	y1: number,
	x2: number,
	y2: number,
	button: Button = 'left',
	steps = 10,
	stepMs = 10
): Action[] {
	const n = Math.max(1, steps);
	const path: Action[] = [down(x1, y1, button)];
	for (let i = 1; i <= n; i++) {
		path.push(
			pause(stepMs),
			move(
				x1 + Math.floor(((x2 - x1) * i) / n),
				y1 + Math.floor(((y2 - y1) * i) / n)
			)
		);
	}
	path.push(up(undefined, undefined, button));
	return path;
}
