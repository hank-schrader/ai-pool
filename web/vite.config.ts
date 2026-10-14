import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
export default defineConfig({
  plugins: [svelte()],
  base: './',
  build: { outDir: '../crates/pool-server/static/dist', emptyOutDir: true, sourcemap: false },
  server: { proxy: { '/v1': 'http://127.0.0.1:8080' } },
});
