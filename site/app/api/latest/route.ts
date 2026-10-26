// The newest velmux release, for update checks: its version and a download
// per platform. Cached for an hour, so GitHub is asked at most that often.
export const revalidate = 3600;

const targets = ['aarch64-apple-darwin', 'x86_64-apple-darwin', 'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl', 'x86_64-pc-windows-msvc'];

export async function GET() {
  const res = await fetch('https://api.github.com/repos/vincentvella/velmux/releases/latest', {
    headers: { Accept: 'application/vnd.github+json' },
    next: { revalidate },
  });
  if (!res.ok) {
    return Response.json({ error: `GitHub answered ${res.status}` }, { status: 502 });
  }
  const release = (await res.json()) as { tag_name: string; html_url: string; published_at: string };
  const tag = release.tag_name;
  const downloads = Object.fromEntries(
    targets.map((t) => [t, `https://github.com/vincentvella/velmux/releases/download/${tag}/velmux-${tag}-${t}.tar.gz`]),
  );
  return Response.json({ version: tag.replace(/^v/, ''), tag, url: release.html_url, published: release.published_at, downloads });
}
