/**
 * The end-to-end harness.
 *
 * Starts the real local service against a throwaway data directory, starts a
 * fake AI runtime, and serves the built web assets with a proxy that attaches
 * the service's bearer token — the same arrangement the desktop shell provides,
 * without needing a window.
 *
 * It also answers two control requests of its own, under `/__harness`, so a
 * test can start from an empty machine or restart the service over a data
 * directory it has already filled. Those routes belong to the harness; the
 * product has no such thing.
 */

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { startFakeOllama } from './fake-ollama.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const distDir = path.join(root, 'apps/web/dist');
// The service only accepts requests from origins it knows: the packaged
// Tauri window and the development server on 1420. The harness stands in for
// the window, so it serves the built assets on that same development port and
// presents that origin honestly rather than widening the allow-list for tests.
const servicePort = Number(process.env.OTWONO_E2E_PORT ?? 1420);

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.map': 'application/json',
};

function serviceBinary() {
  for (const profile of ['debug', 'release']) {
    const candidate = path.join(root, 'target', profile, 'otwono-local-service');
    if (fs.existsSync(candidate)) return candidate;
  }
  throw new Error(
    'The local service binary was not found. Run `cargo build -p otwono-local-service` first.',
  );
}

async function waitForHandshake(file, since, timeoutMs = 30_000) {
  const started = Date.now();
  for (;;) {
    try {
      // A restart reuses the file, so wait for one written after the restart
      // began rather than believing the previous instance's handshake.
      if (fs.statSync(file).mtimeMs >= since) {
        const parsed = JSON.parse(fs.readFileSync(file, 'utf8'));
        if (parsed?.token && parsed?.port) return parsed;
      }
    } catch {
      // Not written yet.
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`The local service did not write ${file} within ${timeoutMs}ms.`);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

export async function startHarness() {
  const ollama = await startFakeOllama();

  let dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'otwono-e2e-'));
  let service = null;
  let handshake = null;
  const createdDirs = [dataDir];

  async function startService() {
    const since = Date.now();
    service = spawn(serviceBinary(), [], {
      env: { ...process.env, OTWONO_DATA_DIR: dataDir, OTWONO_PORT: '0', RUST_LOG: 'warn' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    service.stderr.on('data', (chunk) => {
      const text = String(chunk);
      if (text.includes('ERROR')) process.stderr.write(`[service] ${text}`);
    });
    handshake = await waitForHandshake(path.join(dataDir, 'runtime.json'), since);
  }

  async function stopService() {
    if (!service) return;
    const ended = new Promise((resolve) => service.once('exit', resolve));
    service.kill('SIGTERM');
    await Promise.race([ended, new Promise((resolve) => setTimeout(resolve, 3000))]);
    service = null;
  }

  await startService();

  const web = http.createServer((request, response) => {
    if (request.url.startsWith('/__harness')) {
      return harnessControl(request, response);
    }

    // Proxy the API, attaching the credential the browser cannot hold.
    if (request.url.startsWith('/api') || request.url === '/health') {
      const target = `http://${handshake.address}:${handshake.port}`;
      const proxied = http.request(
        `${target}${request.url}`,
        {
          method: request.method,
          headers: {
            ...request.headers,
            host: `${handshake.address}:${handshake.port}`,
            authorization: `Bearer ${handshake.token}`,
          },
        },
        (upstream) => {
          response.writeHead(upstream.statusCode ?? 500, upstream.headers);
          upstream.pipe(response);
        },
      );
      proxied.on('error', (error) => {
        response.writeHead(502, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ error: { code: 'proxy', message: String(error) } }));
      });
      request.pipe(proxied);
      return;
    }

    // Otherwise serve the built application, falling back to index.html so
    // client-side routes work on a direct load.
    const requested = decodeURIComponent(request.url.split('?')[0]);
    let file = path.join(distDir, requested === '/' ? 'index.html' : requested);
    if (!file.startsWith(distDir) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      file = path.join(distDir, 'index.html');
    }
    const body = fs.readFileSync(file);
    response.writeHead(200, {
      'content-type': MIME[path.extname(file)] ?? 'application/octet-stream',
      'content-length': body.length,
    });
    response.end(body);
  });

  async function harnessControl(request, response) {
    const route = request.url.split('?')[0];
    try {
      if (route === '/__harness/reset') {
        // A brand new machine: a data directory the service has never seen.
        await stopService();
        dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'otwono-e2e-'));
        createdDirs.push(dataDir);
        await startService();
      } else if (route === '/__harness/restart') {
        // The same machine, restarted: an upgrade must find its data intact.
        await stopService();
        await startService();
      } else if (route !== '/__harness/state') {
        response.writeHead(404, { 'content-type': 'application/json' });
        return response.end(JSON.stringify({ error: 'unknown harness route' }));
      }
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ dataDir, port: handshake.port }));
    } catch (error) {
      response.writeHead(500, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: String(error) }));
    }
  }

  await new Promise((resolve) => web.listen(servicePort, '127.0.0.1', resolve));

  return {
    url: `http://127.0.0.1:${servicePort}`,
    get serviceUrl() {
      return `http://${handshake.address}:${handshake.port}`;
    },
    get dataDir() {
      return dataDir;
    },
    ollamaUrl: ollama.url,
    async stop() {
      web.close();
      ollama.close();
      await stopService();
      for (const directory of createdDirs) {
        fs.rmSync(directory, { recursive: true, force: true });
      }
    },
  };
}

// Run directly: start and stay up, printing where things are, so Playwright's
// `webServer` can wait on the URL.
if (import.meta.url === `file://${process.argv[1]}`) {
  const harness = await startHarness();
  fs.writeFileSync(
    path.join(os.tmpdir(), 'otwono-e2e.json'),
    JSON.stringify({ url: harness.url, ollamaUrl: harness.ollamaUrl }),
  );
  console.log(`web        ${harness.url}`);
  console.log(`service    ${harness.serviceUrl}`);
  console.log(`ai runtime ${harness.ollamaUrl}`);
  const stop = async () => {
    await harness.stop();
    process.exit(0);
  };
  process.on('SIGTERM', stop);
  process.on('SIGINT', stop);
}
