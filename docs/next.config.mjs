import { createMDX } from 'fumadocs-mdx/next';
import { initOpenNextCloudflareForDev } from '@opennextjs/cloudflare';

if (process.argv.includes('dev')) initOpenNextCloudflareForDev();

/** @type {import('next').NextConfig} */
const config = {
	reactStrictMode: true,
	turbopack: { root: import.meta.dirname },
	outputFileTracingRoot: import.meta.dirname,
	async rewrites() {
		return [{ source: '/:path*.mdx', destination: '/llms.mdx/:path*' }];
	},
};

export default createMDX()(config);
