// The docs live in the repo's docs/ folder (readable on GitHub); this turns
// them into the site's pages before each build: frontmatter from each file's
// heading, the sidebar order, and links between docs, to the screenshots and to
// the rest of the repo rewritten for the site.
import { cpSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const repo = new URL('../..', import.meta.url).pathname;
const docs = join(repo, 'docs');
const out = new URL('../content/docs', import.meta.url).pathname;
const github = 'https://github.com/vincentvella/velmux/blob/main';
// Sidebar order; anything else follows alphabetically.
const order = ['getting-started', 'using-velmux', 'slots', 'configuration', 'cli', 'how-it-works'];

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
const pages = [];
for (const file of readdirSync(docs).filter((f) => f.endsWith('.md'))) {
	const slug = file.replace(/\.md$/, '');
	pages.push(slug);
	let text = readFileSync(join(docs, file), 'utf8');
	const title = text.match(/^# (.+)$/m)?.[1] ?? slug;
	text = text.replace(/^# .+\n+/m, '');
	text = text
		// Another doc: x.md#y → /docs/x#y
		.replace(/\]\(([a-z-]+)\.md(#[^)]*)?\)/g, (_, page, hash = '') => `](/docs/${page}${hash})`)
		.replace(/\]\(screenshots\//g, '](/screenshots/')
		.replace(/\]\(\.\.\/([^)]+)\)/g, (_, path) => `](${github}/${path})`);
	// For search results and link previews: the first sentence of the first
	// paragraph, as plain text.
	const paragraph = text.split(/\n\s*\n/).find((p) => p.trim() && !/^[#!|`<-]/.test(p.trim())) ?? '';
	const plain = paragraph.replace(/\s+/g, ' ').replace(/\*\*|`/g, '').replace(/\[([^\]]+)\]\([^)]*\)/g, '$1').trim();
	const description = (plain.match(/^.+?[.!?](\s|$)/)?.[0] ?? plain).trim().slice(0, 200);
	writeFileSync(join(out, `${slug}.md`), `---\ntitle: ${JSON.stringify(title)}\ndescription: ${JSON.stringify(description)}\n---\n\n${text}`);
}
pages.sort((a, b) => (order.indexOf(a) + 1 || 99) - (order.indexOf(b) + 1 || 99) || a.localeCompare(b));
writeFileSync(join(out, 'meta.json'), JSON.stringify({ title: 'Docs', pages }, null, 2));
cpSync(join(docs, 'screenshots'), new URL('../public/screenshots', import.meta.url).pathname, { recursive: true });
console.log(`synced ${pages.length} docs into the site`);
