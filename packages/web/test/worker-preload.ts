// Loaded into the test's worker before worker.ts: a browser worker always has self.location.
const g = globalThis as unknown as { location?: URL };
g.location ??= new URL('http://localhost/worker.js');
