// Pages the site takes from the repository's own Markdown, so there is one copy of each text.
// `sync-docs.mjs` copies them into src/content/docs/<slug>.md at the start of every build and
// dev server (those folders are generated: see .gitignore), and `remark-links.mjs` points their
// relative links at the matching site page, or at the file on GitHub.
//
// `source` is relative to the repository root; its first `# Heading` is replaced by `title`.

/** @type {{ source: string, slug: string, title: string, label?: string, description: string }[]} */
export const syncedPages = [
	{
		source: 'docs/gateway.md',
		slug: 'guides/gateway',
		title: 'Gateway',
		description:
			'Run and operate rd-gateway: listeners, configuration, authentication, devices, sessions, logs, metrics and the container image.',
	},
	{
		source: 'docs/setup-mac.md',
		slug: 'guides/mac-agent',
		title: 'Mac agent',
		description:
			'Install RD Agent.app on a Mac, grant Screen Recording and Accessibility, and keep it running and up to date.',
	},
	{
		source: 'docs/setup-windows.md',
		slug: 'guides/windows-agent',
		title: 'Windows agent',
		description: 'Install rd-agent on a Windows 10 or 11 PC, set it up, and learn what it cannot reach.',
	},
	{
		source: 'docs/setup-linux.md',
		slug: 'guides/linux-agent',
		title: 'Linux agent',
		description: 'Run rd-agent on a Linux server: a terminal and the file browser in your browser, no screen and no SSH.',
	},
	{
		source: 'docs/devices.md',
		slug: 'guides/devices',
		title: 'Devices',
		description: 'How the gateway recognises a device, and how to add, rotate, rename and remove one.',
	},
	{
		source: 'clients/desktop/README.md',
		slug: 'guides/desktop-app',
		title: 'rd Desktop',
		description:
			'The Windows and macOS app that shows the viewer in a window and also captures Alt+Tab, Cmd+Tab and the Windows key.',
	},
	{
		source: 'docs/files.md',
		slug: 'guides/files',
		title: 'File transfer',
		description:
			'Browse the remote machine, download files and folders, upload by dropping them in, and the file channel protocol.',
	},
	{
		source: 'docs/terminal.md',
		slug: 'guides/terminal',
		title: 'Terminal',
		description:
			'A shell on the remote machine in the browser, over port 443, that survives a dropped connection; settings and the terminal channel protocol.',
	},
	{
		source: 'docs/updates.md',
		slug: 'guides/updates',
		title: 'Updates',
		description: 'How rd Desktop and the agents find, verify and install new versions, and roll back a bad one.',
	},
	{
		source: 'docs/recovery.md',
		slug: 'guides/recovery',
		title: 'Recovery path',
		label: 'Recovery',
		description: 'Reach a locked, logged-out or rebooted Mac through Apple Screen Sharing over Guacamole.',
	},
	{
		source: 'docs/protocol.md',
		slug: 'reference/protocol',
		title: 'Wire protocol',
		description: 'Protocol v1: the contract between the viewer, the gateway and the agent.',
	},
	{
		source: 'docs/desktop-client.md',
		slug: 'reference/desktop-bridge',
		title: 'Desktop bridge',
		description: 'How rd Desktop grabs the keyboard, and the contract between the viewer page and the app.',
	},
	{
		source: 'viewer/README.md',
		slug: 'reference/viewer',
		title: 'Viewer internals',
		description: 'The browser client: source layout, keyboard handling and the desktop app bridge.',
	},
];

/** Folders under src/content/docs that hold only synced pages (wiped and rewritten on sync). */
export const generatedDirs = [...new Set(syncedPages.map((p) => p.slug.split('/')[0]))];
