import { afterEach, describe, expect, it, vi } from 'vitest';

import { ApiError, streamEvents, type StreamEvent } from '../api/client';

function sseResponse(chunks: string[]): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
  return new Response(stream, {
    status: 200,
    headers: { 'content-type': 'text/event-stream' },
  });
}

afterEach(() => vi.unstubAllGlobals());

describe('streamEvents', () => {
  it('reads frames in order', async () => {
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValue(
          sseResponse([
            'data: {"type":"start","message_id":"m1","model":"llama","provider":"Ollama"}\n\n',
            'data: {"type":"delta","text":"Hello"}\n\n',
            'data: {"type":"delta","text":", world"}\n\n',
            'data: {"type":"done","message_id":"m1","finish_reason":"stop","token_estimate":9}\n\n',
          ]),
        ),
    );

    const events: StreamEvent[] = [];
    await streamEvents('/api/test', {}, (event) => events.push(event));

    expect(events.map((event) => event.type)).toEqual(['start', 'delta', 'delta', 'done']);
    const text = events
      .filter((event): event is Extract<StreamEvent, { type: 'delta' }> => event.type === 'delta')
      .map((event) => event.text)
      .join('');
    expect(text).toBe('Hello, world');
  });

  it('reassembles a frame split across network reads', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(sseResponse(['data: {"type":"del', 'ta","text":"split"}', '\n\n'])),
    );

    const events: StreamEvent[] = [];
    await streamEvents('/api/test', {}, (event) => events.push(event));

    expect(events).toEqual([{ type: 'delta', text: 'split' }]);
  });

  it('ignores keep-alive comments and unreadable frames without ending the stream', async () => {
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValue(
          sseResponse([
            ': keep-alive\n\n',
            'data: {not json}\n\n',
            'data: {"type":"delta","text":"survived"}\n\n',
          ]),
        ),
    );

    const events: StreamEvent[] = [];
    await streamEvents('/api/test', {}, (event) => events.push(event));

    expect(events).toEqual([{ type: 'delta', text: 'survived' }]);
  });

  it('surfaces a refusal as a typed error rather than an empty stream', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        new Response(
          JSON.stringify({
            error: { code: 'forbidden', message: 'Not allowed.', retryable: false },
          }),
          { status: 403, headers: { 'content-type': 'application/json' } },
        ),
      ),
    );

    await expect(streamEvents('/api/test', {}, () => {})).rejects.toMatchObject({
      code: 'forbidden',
      message: 'Not allowed.',
      retryable: false,
    });
  });
});

describe('ApiError', () => {
  it('reads the service error body', async () => {
    const error = await ApiError.fromResponse(
      new Response(
        JSON.stringify({
          error: { code: 'not_found', message: 'That agent was not found.', retryable: false },
        }),
        { status: 404, headers: { 'content-type': 'application/json' } },
      ),
    );

    expect(error.code).toBe('not_found');
    expect(error.message).toBe('That agent was not found.');
    expect(error.status).toBe(404);
    expect(error.retryable).toBe(false);
  });

  it('falls back to a readable message when the body is not JSON', async () => {
    const error = await ApiError.fromResponse(new Response('gateway down', { status: 502 }));
    expect(error.code).toBe('unknown');
    expect(error.message).toContain('502');
    expect(error.retryable).toBe(true);
  });
});
