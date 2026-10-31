// Takes the portal and viewer screenshots used on the docs site (src/assets/screenshots/), and
// the social preview card (public/og.png, from scripts/og.html).
//
// Needs Google Chrome and, except for `og`, a dev gateway with a synthetic agent named "mbp"
// (see site/README.md):
//   node scripts/screenshots.mjs                all shots
//   node scripts/screenshots.mjs portal og      only these
// RD_URL picks another gateway (default http://127.0.0.1:18580).
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';
import sharp from 'sharp';

const base = (process.env.RD_URL ?? 'http://127.0.0.1:18580').replace(/\/$/, '');
const outDir = fileURLToPath(new URL('../src/assets/screenshots/', import.meta.url));
const wanted = new Set(process.argv.slice(2));

const browser = await chromium.launch({ channel: 'chrome', headless: true });
const context = await browser.newContext({ deviceScaleFactor: 2, colorScheme: 'dark' });

// Show a normal signed-in user instead of "dev@localhost (dev mode)".
await context.route('**/api/me', (route) =>
	route.fulfill({
		contentType: 'application/json',
		body: JSON.stringify({ email: 'you@example.com', uid: null, dev: false, devices: true }),
	}),
);

async function save(page, name, options = {}) {
	const png = await page.screenshot({ animations: 'disabled', ...options });
	await sharp(png).webp({ quality: 86 }).toFile(`${outDir}${name}.webp`);
	console.log(`  ${name}.webp`);
}

async function shot(name, viewport, fn) {
	if (wanted.size > 0 && !wanted.has(name)) return;
	const page = await context.newPage();
	await page.setViewportSize(viewport);
	try {
		await fn(page);
	} finally {
		await page.close();
	}
}

/** The element's box plus `pad` CSS pixels around it, for a tight crop. */
async function around(locator, pad) {
	const b = await locator.boundingBox();
	return { x: Math.max(0, b.x - pad), y: Math.max(0, b.y - pad), width: b.width + 2 * pad, height: b.height + 2 * pad };
}

async function removeDevice(page, machine) {
	await page.evaluate((m) => fetch(`/api/devices/${m}`, { method: 'DELETE' }), machine);
}

async function startSession(page) {
	await page.goto(`${base}/m/mbp`);
	await page.getByRole('button', { name: 'Connect' }).click();
	await page.locator('canvas').first().waitFor();
	await page.waitForTimeout(3000);
}

if (wanted.size === 0 || wanted.has('og')) {
	const card = await browser.newContext({ viewport: { width: 1200, height: 630 } });
	const page = await card.newPage();
	await page.goto(new URL('./og.html', import.meta.url).href);
	await page.screenshot({ path: fileURLToPath(new URL('../public/og.png', import.meta.url)) });
	await card.close();
	console.log('  og.png');
	wanted.delete('og');
	if (process.argv.length > 2 && wanted.size === 0) {
		await browser.close();
		process.exit(0);
	}
}

console.log(`Screenshots from ${base}:`);

await shot('portal', { width: 1120, height: 620 }, async (page) => {
	await page.goto(base);
	await removeDevice(page, 'windows-pc');
	await page.reload();
	await page.locator('.device-row[data-machine="mbp"]').waitFor();
	await save(page, 'portal');
});

// The details panel of the MacBook Pro, cropped to its content.
await shot('portal-details', { width: 1440, height: 900 }, async (page) => {
	await page.goto(base);
	await page.locator('.device-row[data-machine="mbp"] .device-name').click();
	const panel = page.locator('.console-inspector');
	await panel.waitFor();
	const box = await panel.boundingBox();
	const bottom = await panel.evaluate((el) => Math.max(...[...el.children].map((c) => c.getBoundingClientRect().bottom)));
	await save(page, 'portal-details', {
		clip: { x: box.x + 1, y: box.y, width: box.width - 1, height: Math.ceil(bottom - box.y + 24) },
	});
});

await shot('add-device', { width: 1120, height: 760 }, async (page) => {
	await page.goto(base);
	await removeDevice(page, 'windows-pc');
	await page.reload();
	await page.getByRole('button', { name: 'Add device' }).click();
	await page.getByLabel('Name').fill('Windows PC');
	await page.getByRole('button', { name: 'Add', exact: true }).click();
	await page.getByText('Windows PC added').waitFor();
	await page.locator('textarea').blur();
	await save(page, 'add-device', { clip: await around(page.locator('dialog[open]'), 40) });
	await removeDevice(page, 'windows-pc');
});

await shot('connect', { width: 1120, height: 900 }, async (page) => {
	await page.goto(`${base}/m/mbp`);
	await page.getByText('Stream settings').click();
	await page.waitForTimeout(300);
	await save(page, 'connect', { clip: await around(page.locator('main > div').first(), 40) });
});

await shot('session', { width: 1280, height: 800 }, async (page) => {
	await startSession(page);
	await page.keyboard.press('Control+Alt+Shift+KeyH');
	await page.getByRole('button', { name: 'Menu' }).click();
	await page.waitForTimeout(2500);
	await save(page, 'session');
});

await shot('settings', { width: 1280, height: 800 }, async (page) => {
	await startSession(page);
	await page.getByRole('button', { name: 'Menu' }).click();
	await page.getByRole('button', { name: 'Settings' }).click();
	await page.waitForTimeout(500);
	await save(page, 'settings');
});

await browser.close();
