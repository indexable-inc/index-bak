import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { mdsvex } from 'mdsvex';
import { siteMdsvexOptions } from './mdsvex.config.js';

/** @type {import('@sveltejs/kit').Config} */
const config = {
  extensions: ['.svelte', '.svx'],
  preprocess: [
    vitePreprocess(),
    mdsvex(siteMdsvexOptions)
  ],
  kit: {
    adapter: adapter({
      pages: 'build',
      assets: 'build',
      fallback: '404.html',
      precompress: false,
      strict: true
    }),
    // No deployment serves this app under a path prefix: GitHub Pages (the
    // only consumer of the old `/index` base) is gone (#3975, #3978), and
    // ix.dev serves this package's content at the domain root via the web
    // app in the ix repo. Override with BASE_PATH for a prefixed deployment.
    paths: {
      base: process.env.BASE_PATH ?? ''
    },
    // SvelteKit's default version.name is Date.now(); that stamp lands in the
    // client bundle, so every rebuild changed a chunk's content hash and
    // filename and the Nix output never reproduced (indexable-inc/ix#8465).
    // Nothing here polls /_app/version.json for live update detection, so a
    // fixed name only removes the nondeterminism.
    version: {
      name: 'static'
    }
  }
};

export default config;
