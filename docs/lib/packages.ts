/** Every package name and version the pages print; the install pages read nothing else. */
export const release = '0.4.0';

/** The last release that shipped the server inside the client packages. */
export const previousRelease = '0.3.2';

/** The release whose Linux binary CI runs `--help` on to check the flags page. */
export const checkedRelease = '0.3.2';

export const repo = 'kyora-sh/starfold';

export const packages = {
	client: { npm: 'starfold', pypi: 'starfold', pyImport: 'starfold' },
	server: {
		npm: '@starfold/server',
		pypi: 'starfold-server',
		pyImport: 'starfold_server',
		crate: 'starfold',
		command: 'starfold',
	},
	web: { npm: '@starfold/web' },
} as const;

export const platforms = [
	{
		name: 'Linux x86_64',
		archive: 'x86_64-linux.tar.gz',
		npm: '@starfold/server-linux-x64-gnu',
	},
	{
		name: 'Linux aarch64',
		archive: 'aarch64-linux.tar.gz',
		npm: '@starfold/server-linux-arm64-gnu',
	},
	{
		name: 'macOS (Apple silicon)',
		archive: 'aarch64-macos.tar.gz',
		npm: '@starfold/server-darwin-arm64',
	},
	{
		name: 'Windows x86_64',
		archive: 'x86_64-windows.zip',
		npm: '@starfold/server-win32-x64',
	},
] as const;

type Path = {
	[K in keyof typeof packages]: `${K & string}.${keyof (typeof packages)[K] & string}`;
}[keyof typeof packages];

export function name(path: Path): string {
	const [group, key] = path.split('.') as [keyof typeof packages, string];
	return (packages[group] as Record<string, string>)[key];
}
