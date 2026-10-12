import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightAurora from 'starlight-aurora';

// The Pages workflow sets DOCS_SITE and DOCS_BASE. On a pull request and
// locally they are empty, and the site is at /.
const site = process.env.DOCS_SITE || undefined;
const base = process.env.DOCS_BASE || '/';

const PROJECT = 'CRT';
const REPO = 'https://github.com/colliery-io/crt';

export default defineConfig({
	site,
	base,
	integrations: [
		starlight({
			title: PROJECT,
			favicon: '/favicon.png',
			social: [{ icon: 'github', label: 'GitHub', href: REPO }],
			plugins: [starlightAurora({ hyper: true })],
			sidebar: [
				{ label: 'Tutorials', items: [{ autogenerate: { directory: 'tutorials' } }] },
				{ label: 'How-to guides', items: [{ autogenerate: { directory: 'how-to' } }] },
				{ label: 'Reference', items: [{ autogenerate: { directory: 'reference' } }] },
				{ label: 'Explanation', items: [{ autogenerate: { directory: 'explanation' } }] },
			],
		}),
	],
});
