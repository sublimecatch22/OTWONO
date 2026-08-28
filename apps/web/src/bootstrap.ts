/**
 * Start-up handshake with the desktop shell.
 *
 * When running inside Tauri, the shell holds the service's address and bearer
 * token. The page asks for them once, before the first request. Outside Tauri
 * (development in a browser) there is nothing to do: the Vite proxy attaches
 * the credential instead.
 */

const READY_TIMEOUT_MS = 15_000;

function insideTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

export async function bootstrapRuntime(): Promise<void> {
  if (!insideTauri()) return;

  const { invoke } = await import('@tauri-apps/api/core');
  const started = Date.now();

  // The service starts in parallel with the window; poll briefly rather than
  // failing on the first attempt.
  for (;;) {
    try {
      const info = await invoke<{ base_url: string; token: string }>('runtime_info');
      window.__OTWONO_RUNTIME__ = { baseUrl: info.base_url, token: info.token };
      return;
    } catch (error) {
      if (Date.now() - started > READY_TIMEOUT_MS) {
        throw new Error(
          `OTWONO's local service did not start within ${READY_TIMEOUT_MS / 1000} seconds. ` +
            `The details are in the application log. (${String(error)})`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 150));
    }
  }
}
