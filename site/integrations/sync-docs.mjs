// Astro integration: copies the repository's Markdown docs (pages.mjs) into the content
// collection before Astro reads it. The dev server restarts when one of the sources changes.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { generatedDirs, syncedPages } from './pages.mjs';

/**
 * @param {{ repoRoot: string, contentDir: string, repoUrl: string, branch: string }} options
 * @returns {import('astro').AstroIntegration}
 */
export function syncDocs({ repoRoot, contentDir, repoUrl, branch }) {
	return {
		name: 'rd:sync-docs',
		hooks: {
			'astro:config:setup': ({ addWatchFile, logger }) => {
				for (const dir of generatedDirs) {
					fs.rmSync(path.join(contentDir, dir), { recursive: true, force: true });
				}
				for (const page of syncedPages) {
					const source = path.join(repoRoot, page.source);
					const target = path.join(contentDir, `${page.slug}.md`);
					fs.mkdirSync(path.dirname(target), { recursive: true });
					fs.writeFileSync(target, render(page, fs.readFileSync(source, 'utf8'), { repoRoot, repoUrl, branch }));
					addWatchFile(source);
				}
				logger.info(`synced ${syncedPages.length} pages from the repository`);
			},
		},
	};
}

function render(page, markdown, { repoRoot, repoUrl, branch }) {
	const body = markdown
		.replace(/\r\n/g, '\n')
		.replace(/^\s*# .*\n+/, '') // the page title comes from the frontmatter
		.trimEnd();
	const frontmatter = {
		title: page.title,
		description: page.description,
		editUrl: `${repoUrl}/edit/${branch}/${page.source}`,
		...(page.label ? { sidebar: { label: page.label } } : {}),
		...lastCommit(repoRoot, page.source),
	};
	// JSON is valid YAML; a date stays unquoted so that YAML reads it as a date, as Starlight wants.
	const yaml = Object.entries(frontmatter)
		.map(([key, value]) => `${key}: ${value instanceof Date ? value.toISOString() : JSON.stringify(value)}`)
		.join('\n');
	return `---\n${yaml}\n---\n\n<!-- Generated from ${page.source} (site/integrations/sync-docs.mjs): edit that file instead. -->\n\n${body}\n`;
}

/** `lastUpdated` from the source's last commit (the copy itself is not in git). */
function lastCommit(repoRoot, file) {
	try {
		const date = execFileSync('git', ['log', '-1', '--format=%cI', '--', file], {
			cwd: repoRoot,
			encoding: 'utf8',
			stdio: ['ignore', 'pipe', 'ignore'],
		}).trim();
		return date ? { lastUpdated: new Date(date) } : {};
	} catch {
		return {};
	}
}
