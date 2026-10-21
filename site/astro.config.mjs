// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
	site: 'https://velmux.com',
	integrations: [
		starlight({
			title: 'velmux',
			description: 'A terminal workspace for running many coding agents at once, on warm git worktrees.',
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/vincentvella/velmux' }],
			editLink: { baseUrl: 'https://github.com/vincentvella/velmux/edit/main/' },
			customCss: ['./src/styles/custom.css'],
			sidebar: [
				{
					label: 'Docs',
					items: [
						{ label: 'Getting started', slug: 'docs/getting-started' },
						{ label: 'Using velmux', slug: 'docs/using-velmux' },
						{ label: 'Working in slots', slug: 'docs/slots' },
						{ label: 'Configuration', slug: 'docs/configuration' },
						{ label: 'Command line', slug: 'docs/cli' },
						{ label: 'How it works', slug: 'docs/how-it-works' },
					],
				},
				{
					label: 'Project',
					items: [
						{ label: 'Releases', link: 'https://github.com/vincentvella/velmux/releases' },
						{ label: 'Contributing', link: 'https://github.com/vincentvella/velmux/blob/main/CONTRIBUTING.md' },
					],
				},
			],
		}),
	],
});
