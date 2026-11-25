/**
 * bun scripts/check-flags.ts <help.txt>
 *
 * Checks the pages against the server: every flag a Linux binary's `--help` (help.txt) or the
 * source of `--help` on any platform (crates/starfold-server/src/help.rs) prints is on the flags
 * page, and every flag any page names is one of those. Also checks that every package a page
 * imports or installs is named in lib/packages.ts and that every internal link names a page.
 */
import { readFileSync, readdirSync } from 'node:fs';
import { join, relative } from 'node:path';
import { packages } from '../lib/packages';

const root = join(import.meta.dir, '..');
const pagesDir = join(root, 'content/docs');
const helpSource = join(root, '../crates/starfold-server/src/help.rs');

const otherTools = new Set([
	'arg',
	'features',
	'flag',
	'from',
	'locked',
	'name',
	'recursive',
	'release',
	'spec',
	'tag',
	'triplet',
	'unix-socket',
]);

const flagsIn = (text: string) =>
	new Set(
		[...text.matchAll(/(?<![\w-])--([a-z][a-z0-9-]*[a-z0-9])/g)]
			.map((m) => m[1])
			.filter((f) => !otherTools.has(f))
	);

const helpFile = process.argv[2];
if (!helpFile) throw new Error('usage: bun scripts/check-flags.ts <help.txt>');
const linux = flagsIn(readFileSync(helpFile, 'utf8'));
if (!linux.has('port') || !linux.has('encoder'))
	throw new Error(`${helpFile} is not starfold --help output`);
const source = flagsIn(
	readFileSync(helpSource, 'utf8').split('#[cfg(test)]')[0]
);
const known = new Set([...linux, ...source]);

const pages = (readdirSync(pagesDir, { recursive: true }) as string[])
	.filter((f) => f.endsWith('.mdx'))
	.map((f) => ({ path: f, text: readFileSync(join(pagesDir, f), 'utf8') }));

const errors: string[] = [];
const flagsPage = pages.find((p) => p.path === join('server', 'flags.mdx'));
if (!flagsPage) throw new Error('content/docs/server/flags.mdx is missing');
const documented = flagsIn(flagsPage.text);
for (const f of linux)
	if (!documented.has(f))
		errors.push(`--${f}: in the Linux --help, not on the flags page`);
for (const f of source)
	if (!documented.has(f))
		errors.push(`--${f}: in help.rs, not on the flags page`);

for (const page of pages)
	for (const f of flagsIn(page.text))
		if (!known.has(f))
			errors.push(
				`${page.path}: --${f} is in neither the Linux --help nor help.rs`
			);

const npmNames = new Set<string>([
	packages.client.npm,
	packages.server.npm,
	packages.web.npm,
]);
const pypiNames = new Set<string>([packages.client.pypi, packages.server.pypi]);
const pyImports = new Set<string>([
	packages.client.pyImport,
	packages.server.pyImport,
]);
const ours = (n: string) => /^@?starfold/.test(n);
for (const page of pages) {
	const check = (name: string, allowed: Set<string>, how: string) => {
		const bare = name.replace(/\[.*$/, '').replace(/^"|"$/g, '');
		if (ours(bare) && !allowed.has(bare))
			errors.push(`${page.path}: ${how} ${bare}, not in lib/packages.ts`);
	};
	for (const m of page.text.matchAll(/from ['"]([^'"]+)['"]/g))
		check(m[1], npmNames, 'imports');
	for (const m of page.text.matchAll(
		/(?:npm (?:install|i)(?: -g)?|bun add|npx) ([@\w./-]+)/g
	))
		check(m[1], npmNames, 'installs');
	for (const m of page.text.matchAll(/(?:pip install|uv add) "?([\w.-]+)/g))
		check(m[1], pypiNames, 'installs');
	for (const m of page.text.matchAll(/^\s*(?:from|import) (\w+)/gm))
		check(m[1], pyImports, 'imports');
}

const routes = new Set(
	pages.map(
		(p) => '/' + p.path.replace(/\.mdx$/, '').replace(/(^|\/)index$/, '')
	)
);
for (const page of pages)
	for (const m of page.text.matchAll(/\]\((\/[^)#\s]*)(#[^)\s]*)?\)/g))
		if (!routes.has(m[1].replace(/\/$/, '') || '/'))
			errors.push(`${page.path}: link to ${m[1]}, which is no page`);

if (errors.length) {
	console.error(errors.join('\n'));
	process.exit(1);
}
console.log(
	`check-flags: ${documented.size} flags on the flags page; ${linux.size} in ${relative(root, helpFile) || helpFile}, ${source.size} in help.rs; ${pages.length} pages`
);
