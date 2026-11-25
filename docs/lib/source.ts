import { docs } from 'collections/server';
import { type InferPageType, loader } from 'fumadocs-core/source';
import { lucideIconsPlugin } from 'fumadocs-core/source/lucide-icons';

export const source = loader({
	baseUrl: '/',
	source: docs.toFumadocsSource(),
	plugins: [lucideIconsPlugin()],
});

export async function markdownOf(page: InferPageType<typeof source>) {
	return `# ${page.data.title}\n\n${await page.data.getText('processed')}`;
}
