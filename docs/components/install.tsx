import { DynamicCodeBlock } from 'fumadocs-ui/components/dynamic-codeblock';
import { Tab, Tabs } from 'fumadocs-ui/components/tabs';
import {
	name,
	platforms,
	previousRelease,
	release,
	repo,
} from '@/lib/packages';

type Which = 'client' | 'server' | 'web';

const lines: Record<Which, { label: string; code: string }[]> = {
	web: [
		{ label: 'npm', code: `npm install ${name('web.npm')}` },
		{ label: 'bun', code: `bun add ${name('web.npm')}` },
	],
	client: [
		{ label: 'npm', code: `npm install ${name('client.npm')}` },
		{ label: 'bun', code: `bun add ${name('client.npm')}` },
		{ label: 'pip', code: `pip install ${name('client.pypi')}` },
		{ label: 'uv', code: `uv add ${name('client.pypi')}` },
	],
	server: [
		{
			label: 'npm',
			code: `npm install ${name('server.npm')}\nnpx ${name('server.command')} --version`,
		},
		{
			label: 'pip',
			code: `pip install ${name('server.pypi')}\n${name('server.command')} --version`,
		},
		{
			label: 'cargo',
			code: `cargo install --locked ${name('server.crate')}\n${name('server.command')} --version`,
		},
	],
};

/** The install commands for the client or the server, one tab per package manager. */
export function Install({ which }: { which: Which }) {
	const items = lines[which];
	return (
		<Tabs items={items.map((i) => i.label)}>
			{items.map((i) => (
				<Tab key={i.label} value={i.label}>
					<DynamicCodeBlock lang="sh" code={i.code} />
				</Tab>
			))}
		</Tabs>
	);
}

/** A package name from lib/packages.ts, as inline code. */
export function Pkg({ path }: { path: Parameters<typeof name>[0] }) {
	return <code>{name(path)}</code>;
}

export function Release({ previous }: { previous?: boolean }) {
	return <>{previous ? previousRelease : release}</>;
}

/** Each platform's release archives and the npm package that carries its binary. */
export function ServerPlatforms() {
	return (
		<table>
			<thead>
				<tr>
					<th>Platform</th>
					<th>Default (MIT)</th>
					<th>x264 (GPL-2.0-or-later)</th>
					<th>npm platform package</th>
				</tr>
			</thead>
			<tbody>
				{platforms.map((p) => (
					<tr key={p.name}>
						<td>{p.name}</td>
						<td>
							<code>starfold-&lt;version&gt;-{p.archive}</code>
						</td>
						<td>
							<code>starfold-x264-&lt;version&gt;-{p.archive}</code>
						</td>
						<td>
							<code>{p.npm}</code>
						</td>
					</tr>
				))}
			</tbody>
		</table>
	);
}

export function ReleasesLink() {
	return <a href={`https://github.com/${repo}/releases`}>GitHub releases</a>;
}
