#!/usr/bin/env node
// Runs the starfold server binary with this command's arguments and passes its exit code through.
import { spawn } from 'node:child_process';
import { binaryPath, checkLibraries } from '../dist/native.js';

let bin;
try {
	bin = binaryPath();
	checkLibraries(bin);
} catch (e) {
	console.error(`starfold: ${e instanceof Error ? e.message : e}`);
	process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });
const signals = ['SIGINT', 'SIGTERM', 'SIGHUP'];
// A terminal's Ctrl-C already reaches the server (same process group); other signals are passed on.
const forward = (signal) => {
	if (signal !== 'SIGINT') child.kill(signal);
};
if (process.platform !== 'win32') {
	for (const s of signals) process.on(s, forward);
}
child.on('error', (e) => {
	console.error(`starfold: ${bin}: ${e.message}`);
	process.exit(1);
});
child.on('exit', (code, signal) => {
	for (const s of signals) process.off(s, forward);
	if (signal) {
		process.kill(process.pid, signal);
	} else {
		process.exit(code ?? 1);
	}
});
