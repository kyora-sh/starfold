import defaultMdxComponents from 'fumadocs-ui/mdx';
import { Tab, Tabs } from 'fumadocs-ui/components/tabs';
import type { MDXComponents } from 'mdx/types';
import {
	Install,
	Pkg,
	Release,
	ReleasesLink,
	ServerPlatforms,
} from './install';

export function getMDXComponents(components?: MDXComponents) {
	return {
		...defaultMdxComponents,
		Tab,
		Tabs,
		Install,
		Pkg,
		Release,
		ReleasesLink,
		ServerPlatforms,
		...components,
	} satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
	type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
