import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';

/**
 * The core engine default (see docs/api-contract.md).
 * `VITE_CORE_URL` overrides it for both the dev proxy and the runtime client.
 */
const DEFAULT_CORE_URL = 'http://localhost:8080';

/** The repo root holds the shared `.env`; the UI package lives at `<root>/ts/packages/ui`. */
const ENV_DIR = '../../..';

/**
 * GitHub Pages serves the build from `https://<user>.github.io/<repo>/`, so
 * the asset paths need the repository prefix. Override it with
 * `VITE_BASE_PATH` when serving from a custom domain.
 */
const DEFAULT_BASE_PATH = '/openether/';

export default defineConfig(({ mode, command }) => {
  const env = loadEnv(mode, ENV_DIR);
  const target = env.VITE_CORE_URL || DEFAULT_CORE_URL;
  const wsTarget = target.replace(/^http/, 'ws');
  // The dev server keeps the site at the root; the production build carries
  // the repository prefix GitHub Pages serves it under.
  const base = command === 'serve' ? '/' : env.VITE_BASE_PATH || DEFAULT_BASE_PATH;

  return {
    plugins: [react()],
    envDir: ENV_DIR,
    base,
    server: {
      port: 5173,
      host: '127.0.0.1',
      proxy: {
        '/api': { target, changeOrigin: true },
        '/ws': { target: wsTarget, ws: true, changeOrigin: true },
      },
    },
    preview: {
      port: 4173,
      host: '127.0.0.1',
    },
    build: {
      outDir: 'dist',
      target: 'es2022',
      sourcemap: false,
      chunkSizeWarningLimit: 700,
    },
  };
});
