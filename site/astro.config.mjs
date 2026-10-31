// @ts-check
import { fileURLToPath } from 'node:url';
import { satteri } from '@astrojs/markdown-satteri';
import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';
import { markdownLinks } from './integrations/markdown-links.mjs';
import { syncDocs } from './integrations/sync-docs.mjs';

const repoUrl = 'https://github.com/Thijmen/remoted';
const branch = 'main';
const repoRoot = fileURLToPath(new URL('..', import.meta.url));
const contentDir = fileURLToPath(new URL('./src/content/docs/', import.meta.url));

// GitHub Pages serves the site at https://thijmen.github.io/remoted/. The Pages workflow passes
// the values GitHub reports (a custom domain changes both); these are the defaults for local builds.
const site = process.env.SITE_ORIGIN || 'https://thijmen.github.io';
const base = process.env.SITE_BASE_PATH ?? '/remoted';

const description =
	'rd is a low-latency remote desktop that runs in a plain browser tab, over one WebSocket on port 443.';

export default defineConfig({
	site,
	base: base || '/',
	trailingSlash: 'always',
	devToolbar: { enabled: false },
	markdown: {
		processor: satteri({ mdastPlugins: [markdownLinks({ base, repoRoot, contentDir, repoUrl, branch })] }),
	},
	integrations: [
		syncDocs({ repoRoot, contentDir, repoUrl, branch }),
		starlight({
			title: 'rd',
			description,
			logo: { src: './src/assets/logo.svg', alt: 'rd' },
			favicon: '/favicon.svg',
			social: [{ icon: 'github', label: 'GitHub', href: repoUrl }],
			editLink: { baseUrl: `${repoUrl}/edit/${branch}/site/` },
			lastUpdated: true,
			customCss: ['./src/styles/theme.css'],
			components: {
				Hero: './src/components/Hero.astro',
			},
			head: [
				{ tag: 'meta', attrs: { property: 'og:image', content: `${site}${base}/og.png` } },
				{ tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
				{ tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
				{ tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
				{ tag: 'meta', attrs: { name: 'theme-color', content: '#0d1016' } },
			],
			expressiveCode: {
				styleOverrides: { borderRadius: '0.625rem' },
			},
			sidebar: [
				{
					label: 'Start here',
					items: ['introduction', 'quick-start', 'how-it-works'],
				},
				{
					label: 'Set up',
					items: [
						'guides/gateway',
						'install-agent',
						'guides/mac-agent',
						'guides/windows-agent',
						'guides/linux-agent',
						'guides/devices',
						'guides/desktop-app',
					],
				},
				{
					label: 'Use',
					items: ['using-rd', 'guides/files', 'guides/terminal', 'guides/updates', 'guides/recovery'],
				},
				{
					label: 'Reference',
					items: ['reference/protocol', 'reference/desktop-bridge', 'reference/viewer'],
				},
				{
					label: 'Contribute',
					items: ['development'],
				},
			],
		}),
	],
});
