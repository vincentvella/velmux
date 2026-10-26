import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // The docs start at getting started.
  async redirects() {
    return [{ source: '/docs', destination: '/docs/getting-started', permanent: false }];
  },
  // As text, so `irm …/install.ps1 | iex` gets a script, not bytes.
  async headers() {
    return [{ source: '/install.ps1', headers: [{ key: 'Content-Type', value: 'text/plain; charset=utf-8' }] }];
  },
};

export default withMDX(config);
