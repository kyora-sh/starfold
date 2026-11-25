import type { MetadataRoute } from 'next';
import { site } from '@/lib/layout.shared';
import { source } from '@/lib/source';

export const dynamic = 'force-static';

export default function sitemap(): MetadataRoute.Sitemap {
	return source
		.getPages()
		.map((page) => ({ url: new URL(page.url, site).href }));
}
