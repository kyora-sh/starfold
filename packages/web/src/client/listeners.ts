// Listeners added through one list, so whoever set them up can remove every one of them
// explicitly (an element's listeners stay with it after it is removed from the page).

export function listeners() {
	const added: (() => void)[] = [];
	return {
		on<E extends Event>(
			target: EventTarget,
			type: string,
			fn: (e: E) => void,
			options?: AddEventListenerOptions
		) {
			const l = fn as EventListener;
			target.addEventListener(type, l, options);
			added.push(() => target.removeEventListener(type, l, options));
		},
		/** Removes every listener added so far. */
		off() {
			for (const remove of added.splice(0)) remove();
		},
	};
}
