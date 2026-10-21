'use client';

// Install: Homebrew, the script, or a download, with the visitor's platform
// picked out (a guess from the browser; every option stays visible).
import { useEffect, useState } from 'react';

const latest = 'https://github.com/vincentvella/velmux/releases/latest/download';
const downloads = [
  { id: 'mac-arm', label: 'macOS · Apple Silicon', file: 'velmux-aarch64-apple-darwin.tar.gz' },
  { id: 'mac-intel', label: 'macOS · Intel', file: 'velmux-x86_64-apple-darwin.tar.gz' },
  { id: 'linux-x64', label: 'Linux · x86_64', file: 'velmux-x86_64-unknown-linux-musl.tar.gz' },
  { id: 'linux-arm', label: 'Linux · arm64', file: 'velmux-aarch64-unknown-linux-musl.tar.gz' },
];

function Command({ label, command }: { label: string; command: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center gap-3 rounded-lg border bg-fd-card px-3 py-2.5 font-mono text-sm">
      <span className="w-20 shrink-0 font-sans text-xs text-fd-muted-foreground">{label}</span>
      <code className="flex-1 overflow-x-auto whitespace-nowrap">{command}</code>
      <button
        type="button"
        className="rounded border bg-fd-secondary px-2 py-0.5 font-sans text-xs hover:bg-fd-accent"
        onClick={async () => {
          await navigator.clipboard.writeText(command);
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        }}
      >
        {copied ? 'Copied' : 'Copy'}
      </button>
    </div>
  );
}

export function Install() {
  const [yours, setYours] = useState<string | null>(null);
  useEffect(() => {
    const ua = navigator.userAgent.toLowerCase();
    setYours(ua.includes('mac') ? 'mac-arm' : ua.includes('linux') ? (/aarch64|arm/.test(ua) ? 'linux-arm' : 'linux-x64') : null);
  }, []);
  return (
    <div className="grid gap-2">
      <Command label="Homebrew" command="brew install vincentvella/tap/velmux" />
      <Command label="Script" command="curl -fsSL https://velmux.com/install.sh | sh" />
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        {downloads.map((d) => (
          <a
            key={d.id}
            href={`${latest}/${d.file}`}
            className={`rounded-lg border px-3 py-2 text-sm hover:border-fd-primary ${yours === d.id ? 'border-fd-primary' : ''}`}
          >
            {d.label}
            {yours === d.id && <span className="text-fd-primary"> · yours</span>}
          </a>
        ))}
      </div>
    </div>
  );
}
