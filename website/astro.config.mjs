// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Deployed as a GitHub Pages project site: https://levyks.github.io/pgsaci/
export default defineConfig({
	site: 'https://levyks.github.io',
	base: '/pgsaci/',
	integrations: [
		starlight({
			title: 'pgSaci',
			favicon: '/favicon.svg',
			description:
				'An Oracle TNS/TTC wire-protocol proxy in front of stock PostgreSQL. ' +
				'Unmodified Oracle drivers connect believing they are talking to Oracle.',
			social: [
				{
					icon: 'github',
					label: 'GitHub',
					href: 'https://github.com/Levyks/pgsaci',
				},
			],
			editLink: {
				baseUrl: 'https://github.com/Levyks/pgsaci/edit/main/website/',
			},
			lastUpdated: true,
			tableOfContents: { minHeadingLevel: 2, maxHeadingLevel: 3 },
			sidebar: [
				{
					label: 'Start here',
					items: [
						{ label: 'What pgSaci is', slug: 'overview' },
						{ label: 'Getting started', slug: 'getting-started' },
					],
				},
				{
					label: 'Compatibility',
					items: [
						{ label: 'What works', slug: 'what-works' },
						{ label: 'What does not (yet, or ever)', slug: 'limitations' },
						{ label: 'Compatibility matrix', slug: 'compatibility' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'Configuration', slug: 'configuration' },
						{ label: 'How it works', slug: 'how-it-works' },
						{ label: 'Benchmarks', slug: 'benchmarks' },
						{ label: 'Legal & trademarks', slug: 'legal' },
					],
				},
			],
		}),
	],
});
