/** Drive a starfold desktop: screenshots, waiting for the screen, mouse and keyboard. */

export * as act from './act.js';
export type { Action, Button } from './act.js';
export { DEFAULT_TIMEOUT_MS, DEFAULT_URL, Starfold } from './client.js';
export type { StarfoldOptions, WaitOptions } from './client.js';
export {
	AuthError,
	ForbiddenError,
	NotFoundError,
	ReadOnlyError,
	RecordingOffError,
	Screenshot,
	StarfoldConnectionError,
	StarfoldError,
	StarfoldTimeoutError,
} from './types.js';
export type {
	FileEntry,
	FileList,
	InputResult,
	Recording,
	Segments,
	Upload,
	WaitResult,
} from './types.js';
export {
	LINUX_LIBRARIES_HINT,
	LocalServer,
	MissingLibrariesError,
	binaryPath,
	checkLibraries,
	missingLibraries,
	platformKey,
	serverPackage,
	startServer,
} from './native.js';
export type { StartServerOptions } from './native.js';
