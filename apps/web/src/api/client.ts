/**
 * The API client.
 *
 * In the packaged application the desktop shell injects the service address and
 * bearer token before the page loads. In development the Vite proxy attaches
 * the header instead, so the token never enters the page.
 */

export interface RuntimeConfig {
  baseUrl: string;
  token: string | null;
}

declare global {
  interface Window {
    __OTWONO_RUNTIME__?: { baseUrl: string; token: string };
  }
}

export function runtimeConfig(): RuntimeConfig {
  const injected = typeof window !== 'undefined' ? window.__OTWONO_RUNTIME__ : undefined;
  if (injected?.baseUrl && injected?.token) {
    return { baseUrl: injected.baseUrl.replace(/\/$/, ''), token: injected.token };
  }
  // Same origin: the development proxy adds the credential.
  return { baseUrl: '', token: null };
}

/** An error carrying the service's machine code and human message. */
export class ApiError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly status: number,
    readonly retryable: boolean,
  ) {
    super(message);
    this.name = 'ApiError';
  }

  static async fromResponse(response: Response): Promise<ApiError> {
    let code = 'unknown';
    let message = `The request failed (${response.status}).`;
    let retryable = response.status >= 500;
    try {
      const body = await response.json();
      if (body?.error) {
        code = body.error.code ?? code;
        message = body.error.message ?? message;
        retryable = Boolean(body.error.retryable);
      }
    } catch {
      // A non-JSON body: keep the generic message.
    }
    return new ApiError(code, message, response.status, retryable);
  }
}

function headers(extra?: HeadersInit): Headers {
  const built = new Headers(extra);
  built.set('content-type', 'application/json');
  const { token } = runtimeConfig();
  if (token) built.set('authorization', `Bearer ${token}`);
  return built;
}

async function parse<T>(response: Response): Promise<T> {
  if (!response.ok) throw await ApiError.fromResponse(response);
  if (response.status === 204) return undefined as T;
  const type = response.headers.get('content-type') ?? '';
  if (type.includes('application/json')) return (await response.json()) as T;
  return (await response.text()) as unknown as T;
}

function url(path: string): string {
  return `${runtimeConfig().baseUrl}${path}`;
}

export const api = {
  async get<T>(path: string, signal?: AbortSignal): Promise<T> {
    return parse<T>(await fetch(url(path), { headers: headers(), signal }));
  },

  async post<T>(path: string, body?: unknown, signal?: AbortSignal): Promise<T> {
    return parse<T>(
      await fetch(url(path), {
        method: 'POST',
        headers: headers(),
        body: JSON.stringify(body ?? {}),
        signal,
      }),
    );
  },

  async put<T>(path: string, body?: unknown, signal?: AbortSignal): Promise<T> {
    return parse<T>(
      await fetch(url(path), {
        method: 'PUT',
        headers: headers(),
        body: JSON.stringify(body ?? {}),
        signal,
      }),
    );
  },

  async delete<T>(path: string, signal?: AbortSignal): Promise<T> {
    return parse<T>(await fetch(url(path), { method: 'DELETE', headers: headers(), signal }));
  },

  /** Raw text, for report and transcript downloads. */
  async text(path: string, signal?: AbortSignal): Promise<string> {
    const response = await fetch(url(path), { headers: headers(), signal });
    if (!response.ok) throw await ApiError.fromResponse(response);
    return response.text();
  },
};

/** One frame of a streamed reply. Mirrors `StreamEvent` in the service. */
export type StreamEvent =
  | { type: 'start'; message_id: string; model: string; provider: string }
  | { type: 'delta'; text: string }
  | { type: 'tool_call'; tool: string; summary: string; status: string }
  | { type: 'citations'; citations: Citation[] }
  | { type: 'approval_required'; request_id: string; summary: string }
  | { type: 'done'; message_id: string; finish_reason: string; token_estimate: number | null }
  | { type: 'error'; message: string; retryable: boolean };

export interface Citation {
  source_id: string;
  document_id: string;
  file_name: string;
  file_path: string;
  chunk_index: number;
  locator: string | null;
  excerpt: string;
  score: number;
}

/**
 * Read a server-sent event stream, calling `onEvent` for each frame.
 *
 * `fetch` is used rather than `EventSource` because the request needs a
 * bearer header and a POST body, neither of which `EventSource` supports.
 * Aborting the signal is how "Stop generating" works: the service sees the
 * receiver close and stops reading from the model.
 */
export async function streamEvents(
  path: string,
  body: unknown,
  onEvent: (event: StreamEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const response = await fetch(url(path), {
    method: 'POST',
    headers: headers({ accept: 'text/event-stream' }),
    body: JSON.stringify(body),
    signal,
  });
  if (!response.ok) throw await ApiError.fromResponse(response);
  if (!response.body) throw new Error('This browser cannot read a streamed response.');

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';

  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      // Frames are separated by a blank line; a frame may span reads.
      let boundary = buffer.indexOf('\n\n');
      while (boundary !== -1) {
        const frame = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        emit(frame, onEvent);
        boundary = buffer.indexOf('\n\n');
      }
    }
    if (buffer.trim()) emit(buffer, onEvent);
  } finally {
    reader.cancel().catch(() => {});
  }
}

function emit(frame: string, onEvent: (event: StreamEvent) => void): void {
  const data = frame
    .split('\n')
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trim())
    .join('');
  if (!data) return;
  try {
    onEvent(JSON.parse(data) as StreamEvent);
  } catch {
    // A frame we cannot read is dropped rather than breaking the stream; the
    // service's own `done` or `error` frame still ends it.
  }
}
