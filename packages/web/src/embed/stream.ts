// Embedded: what the page takes from the worker's ordinary channel. That channel carries the
// server's messages, so only stream messages are acted on; nothing here is lifecycle (that has
// its own port, life.ts) and nothing is logged, so no server text reaches the console.
import type { FromWorker } from '../protocol';

export interface StreamSink {
	frames(m: Extract<FromWorker, { t: 'frames' }>): void;
	surface(m: Extract<FromWorker, { t: 'surface' }>): void;
	hello(m: Extract<FromWorker, { t: 'hello' }>): void;
	codec(m: Extract<FromWorker, { t: 'codec' }>): void;
	cursor(m: Extract<FromWorker, { t: 'cursor' }>): void;
}

export function onStream(m: FromWorker, sink: StreamSink) {
	switch (m.t) {
		case 'frames':
			return sink.frames(m);
		case 'surface':
			return sink.surface(m);
		case 'hello':
			return sink.hello(m);
		case 'codec':
			return sink.codec(m);
		case 'cursor':
			return sink.cursor(m);
	}
}
