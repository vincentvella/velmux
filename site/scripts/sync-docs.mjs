// The docs live in the repo's docs/ folder (readable on GitHub); this turns
// them into the site's pages before each build: frontmatter from each file's
// heading, links between docs and to the screenshots rewritten for the site.
import { cpSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const repo = new URL('../..', import.meta.url).pathname;
const docs = join(repo, 'docs');
const out = new URL('../src/content/docs/docs', import.meta.url).pathname;
const github = 'https://github.com/vincentvella/velmux/blob/main';

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
for (const file of readdirSync(docs).filter((f) => f.endsWith('.md'))) {
	const slug = file.replace(/\.md$/, '');
	let text = readFileSync(join(docs, file), 'utf8');
	const title = text.match(/^# (.+)$/m)?.[1] ?? slug;
	text = text.replace(/^# .+\n+/m, '');
	text = text
		// Another doc: docs/x.md#y → /docs/x/#y
		.replace(/\]\(([a-z-]+)\.md(#[^)]*)?\)/g, (_, page, hash = '') => `](/docs/${page}/${hash})`)
		// Screenshots
		.replace(/\]\(screenshots\//g, '](/screenshots/')
		// Anything else in the repo
		.replace(/\]\(\.\.\/([^)]+)\)/g, (_, path) => `](${github}/${path})`);
	const description = text.split('\n').find((l) => l && !l.startsWith('#') && !l.startsWith('!') && !l.startsWith('|'))?.slice(0, 160);
	const edit = `https://github.com/vincentvella/velmux/edit/main/docs/${file}`;
	const front = `---\ntitle: ${JSON.stringify(title)}\ndescription: ${JSON.stringify(description ?? '')}\neditUrl: ${edit}\n---\n\n`;
	writeFileSync(join(out, `${slug}.md`), front + text);
}
cpSync(join(docs, 'screenshots'), new URL('../public/screenshots', import.meta.url).pathname, { recursive: true });
console.log('synced docs/ into the site');
