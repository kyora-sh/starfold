import type { MetadataRoute } from 'next';
import { site } from '@/lib/layout.shared';

export const dynamic = 'force-static';

export default function robots(): MetadataRoute.Robots {
	return {
		rules: { userAgent: '*', allow: '/' },
		sitemap: `${site}/sitemap.xml`,
	};
}
