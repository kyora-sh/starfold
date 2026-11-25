import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import {
	DocsBody,
	DocsDescription,
	DocsPage,
	DocsTitle,
	MarkdownCopyButton,
	ViewOptionsPopover,
} from 'fumadocs-ui/layouts/docs/page';
import { createRelativeLink } from 'fumadocs-ui/mdx';
import { getMDXComponents } from '@/components/mdx';
import { repo } from '@/lib/packages';
import { source } from '@/lib/source';

type Params = { slug?: string[] };

export default async function Page({ params }: { params: Promise<Params> }) {
	const { slug } = await params;
	const page = source.getPage(slug);
	if (!page) notFound();
	const MDX = page.data.body;
	const markdownUrl = `${page.url === '/' ? '/index' : page.url}.mdx`;

	return (
		<DocsPage toc={page.data.toc} full={page.data.full}>
			<DocsTitle>{page.data.title}</DocsTitle>
			<DocsDescription className="mb-0">
				{page.data.description}
			</DocsDescription>
			<div className="flex flex-row gap-2 items-center border-b pb-6">
				<MarkdownCopyButton markdownUrl={markdownUrl} />
				<ViewOptionsPopover
					markdownUrl={markdownUrl}
					githubUrl={`https://github.com/${repo}/blob/main/docs/content/docs/${page.path}`}
				/>
			</div>
			<DocsBody>
				<MDX
					components={getMDXComponents({ a: createRelativeLink(source, page) })}
				/>
			</DocsBody>
		</DocsPage>
	);
}

export function generateStaticParams() {
	return source.generateParams();
}

export async function generateMetadata({
	params,
}: {
	params: Promise<Params>;
}): Promise<Metadata> {
	const { slug } = await params;
	const page = source.getPage(slug);
	if (!page) notFound();
	return { title: page.data.title, description: page.data.description };
}
