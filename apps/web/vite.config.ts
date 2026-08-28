import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/**
 * In development the browser has no way to read the service's bearer token —
 * it lives in an owner-only file. Rather than putting the token into the page,
 * the dev server proxies `/api` and attaches the header itself, reading the
 * handshake file the service wrote. The packaged application does not use this
 * path: there, the desktop shell injects the runtime configuration directly.
 */
function readDevToken(): string | null {
  const explicit = process.env.OTWONO_DATA_DIR;
  const candidates = explicit
    ? [path.join(explicit, 'runtime.json')]
    : [
        path.join(os.homedir(), '.local', 'share', 'otwono-ai', 'runtime.json'),
        path.join(os.homedir(), 'Library', 'Application Support', 'OTWONO AI', 'runtime.json'),
        path.join(process.env.APPDATA ?? '', 'OTWONO AI', 'runtime.json'),
      ];
  for (const candidate of candidates) {
    try {
      const parsed = JSON.parse(fs.readFileSync(candidate, 'utf8'));
      if (parsed?.token && parsed?.port) return JSON.stringify(parsed);
    } catch {
      // Try the next location.
    }
  }
  return null;
}

const handshake = readDevToken();
const runtime = handshake ? JSON.parse(handshake) : null;
const target = runtime
  ? `http://${runtime.address}:${runtime.port}`
  : (process.env.OTWONO_SERVICE_URL ?? 'http://127.0.0.1:8787');

export default defineConfig({
  plugins: [react()],
  server: {
    port: 1420,
    strictPort: true,
    proxy: {
      '/api': {
        target,
        changeOrigin: false,
        configure: (proxy) => {
          proxy.on('proxyReq', (proxyRequest) => {
            if (runtime?.token) {
              proxyRequest.setHeader('authorization', `Bearer ${runtime.token}`);
            }
            proxyRequest.setHeader('origin', 'http://localhost:1420');
          });
        },
      },
      '/health': { target, changeOrigin: false },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: true,
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test-setup.ts'],
    css: false,
  },
});
