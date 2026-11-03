/// <reference types="vitest/config" />
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { readFileSync } from 'node:fs';
import { defineConfig } from 'vite';

// The app's version is the workspace's (Cargo.toml [workspace.package]), the same one Tauri puts in the bundle.
const cargo = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8');
const version = /\[workspace\.package\][^[]*?\nversion\s*=\s*"([^"]+)"/.exec(cargo)?.[1];
if (!version) throw new Error('no version in [workspace.package] of Cargo.toml');

// The Tauri window loads the dev server on a fixed port (crates/revenant-app/tauri.conf.json `devUrl`).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  define: { __APP_VERSION__: JSON.stringify(version) },
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: 'dist', emptyOutDir: true },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test-setup.ts'],
  },
});
