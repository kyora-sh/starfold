import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import { repo } from './packages';

export const site = 'https://starfold.kyora.sh';

export function baseOptions(): BaseLayoutProps {
	return {
		nav: { title: 'starfold' },
		githubUrl: `https://github.com/${repo}`,
	};
}
