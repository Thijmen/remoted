// Markdown plugin (Sätteri, Astro's Markdown processor) for links in every page:
//
// - Root-relative links (`/quick-start/`) get the site's base path (`/remoted/quick-start/`),
//   so pages can be written without knowing where the site is hosted. (Sätteri cannot change
//   MDX component attributes: use src/components/LinkCard.astro, which does the same.)
// - In pages synced from the repository (pages.mjs), relative links are resolved against the
//   source file: a link to another synced file becomes a link to its page (anchors kept),
//   anything else (code, scripts, workflows, …) a link to the file on GitHub.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { syncedPages } from './pages.mjs';

/**
 * @param {{ base: string, repoRoot: string, contentDir: string, repoUrl: string, branch: string }} options
 * @returns {import('satteri').MdastPluginEntry}
 */
export function markdownLinks({ base, repoRoot, contentDir, repoUrl, branch }) {
	const prefix = base.replace(/\/+$/, '');
	const bySource = new Map(syncedPages.map((p) => [p.source, p]));
	const byFile = new Map(syncedPages.map((p) => [path.join(contentDir, `${p.slug}.md`), p]));

	const withBase = (url) => (prefix && !url.startsWith(`${prefix}/`) ? `${prefix}${url}` : url);

	/** @param {string} url @param {{ source: string } | undefined} page */
	function rewrite(url, page) {
		if (/^[a-z][a-z\d+.-]*:/i.test(url) || url.startsWith('#') || url.startsWith('//')) return url;
		if (url.startsWith('/')) return withBase(url);
		if (!page) return url;

		const [target, hash] = splitHash(url);
		const resolved = path.posix.normalize(path.posix.join(path.posix.dirname(page.source), target));
		const linked = bySource.get(resolved);
		if (linked) return withBase(`/${linked.slug}/`) + hash;

		const stat = fs.statSync(path.join(repoRoot, resolved), { throwIfNoEntry: false });
		return `${repoUrl}/${stat?.isDirectory() ? 'tree' : 'blob'}/${branch}/${resolved}${hash}`;
	}

	/** The synced page a relative link in `page` points to, if any. */
	function linkedPage(url, page) {
		if (!page || /^[a-z][a-z\d+.-]*:/i.test(url) || url.startsWith('#') || url.startsWith('/')) return undefined;
		const [target] = splitHash(url);
		return bySource.get(path.posix.normalize(path.posix.join(path.posix.dirname(page.source), target)));
	}

	return ({ fileURL }) => {
		const page = fileURL ? byFile.get(fileURLToPath(fileURL)) : undefined;
		const url = (node, ctx) => {
			const next = rewrite(node.url, page);
			if (next !== node.url) ctx.setProperty(node, 'url', next);
		};
		return {
			name: 'rd-links',
			link(node, ctx) {
				// `[updates.md](updates.md)` reads better on the site as `[Updates](…)`.
				const linked = linkedPage(node.url, page);
				const text = node.children.length === 1 && node.children[0].type === 'text' ? node.children[0].value : '';
				if (linked && /\.md$/i.test(text) && path.posix.basename(text) === path.posix.basename(linked.source)) {
					ctx.setProperty(node, 'children', [{ type: 'text', value: linked.title }]);
				}
				url(node, ctx);
			},
			definition: url,
		};
	};
}

function splitHash(url) {
	const i = url.indexOf('#');
	return i === -1 ? [url, ''] : [url.slice(0, i), url.slice(i)];
}
