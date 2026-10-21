import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // The docs start at getting started.
  async redirects() {
    return [{ source: '/docs', destination: '/docs/getting-started', permanent: false }];
  },
};

export default withMDX(config);
