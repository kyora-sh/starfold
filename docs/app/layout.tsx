import type { Metadata } from 'next';
import type { ReactNode } from 'react';
import { RootProvider } from 'fumadocs-ui/provider/next';
import { Inter } from 'next/font/google';
import { site } from '@/lib/layout.shared';
import './global.css';

const inter = Inter({ subsets: ['latin'] });

const description =
	'Documentation for starfold: low-latency desktop streaming to the browser, with HTTP APIs and SDKs for agents.';

export const metadata: Metadata = {
	metadataBase: new URL(site),
	title: { default: 'starfold docs', template: '%s | starfold docs' },
	description,
	applicationName: 'starfold docs',
	openGraph: {
		type: 'website',
		url: site,
		title: 'starfold docs',
		siteName: 'starfold docs',
		description,
	},
	twitter: { card: 'summary', title: 'starfold docs', description },
};

export default function Layout({ children }: { children: ReactNode }) {
	return (
		<html lang="en" className={inter.className} suppressHydrationWarning>
			<body className="flex flex-col min-h-screen">
				<RootProvider>{children}</RootProvider>
			</body>
		</html>
	);
}
