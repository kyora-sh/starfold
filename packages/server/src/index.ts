/** Start a starfold server from code; the client is the starfold package's. */

export {
	BinaryNotFoundError,
	LINUX_LIBRARIES_HINT,
	LocalServer,
	MissingLibrariesError,
	ServerStartError,
	binaryPath,
	checkLibraries,
	missingLibraries,
	platformKey,
	serverPackage,
	startServer,
} from './native.js';
export type { StartServerOptions } from './native.js';
