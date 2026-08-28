/**
 * A fake local AI runtime for the end-to-end tests.
 *
 * It speaks Ollama's real wire protocol, so the application's own adapter is
 * exercised rather than replaced. Nothing like this ships in the product.
 */

import http from 'node:http';

const MODELS = ['llama3.1:8b', 'nomic-embed-text:latest'];

function json(response, status, body) {
  const payload = JSON.stringify(body);
  response.writeHead(status, {
    'content-type': 'application/json',
    'content-length': Buffer.byteLength(payload),
  });
  response.end(payload);
}

async function readBody(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  if (chunks.length === 0) return {};
  try {
    return JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    return {};
  }
}

/**
 * The reply is derived from the prompt so a test can assert the model was
 * actually given what the application claims it was given.
 */
function replyFor(body) {
  const text = (body.messages ?? []).map((message) => message.content).join('\n');

  if (text.includes('VERDICT: pass or fail')) {
    return 'VERDICT: pass\n1. Met — the output covers the criterion.';
  }
  if (text.includes('Produce a plan as a JSON array')) {
    return JSON.stringify([
      {
        title: 'Gather the figures',
        instructions: 'Collect the numbers for the report.',
        acceptance_criteria: ['All figures present'],
        depends_on: [],
        suggested_role: 'Research',
        requires_approval: false,
      },
      {
        title: 'Write the summary',
        instructions: 'Summarise the figures for the reader.',
        acceptance_criteria: ['Under 500 words'],
        depends_on: [1],
        suggested_role: 'Writing',
        requires_approval: false,
      },
    ]);
  }
  if (text.includes('You are chairing this session')) {
    return [
      '## Synthesis',
      '',
      'The group agreed to wait for the audit to close.',
      '',
      '## Dissent',
      '',
      'One participant preferred shipping on Friday.',
      '',
      '## Unresolved questions',
      '',
      '- Who signs off the audit?',
      '',
      '## Recommended decision',
      '',
      'Delay the release to Monday.',
    ].join('\n');
  }
  if (text.includes('OTWONO_UNTRUSTED_CONTENT')) {
    // Prove the retrieved passage reached the model and is cited back.
    return 'According to the handbook, staff receive 25 days of annual leave each year.';
  }
  return 'SOURCED: Hello from the test runtime.';
}

const server = http.createServer(async (request, response) => {
  const { url, method } = request;

  if (url === '/api/version') return json(response, 200, { version: '0.5.7' });

  if (url === '/api/tags') {
    return json(response, 200, {
      models: MODELS.map((name) => ({
        name,
        size: 4_000_000_000,
        details: { parameter_size: '8B', quantization_level: 'Q4_K_M' },
      })),
    });
  }

  if (url === '/api/show' && method === 'POST') {
    const body = await readBody(request);
    // Real Ollama answers per model: an embedding model reports `embedding`
    // and nothing else, which is what stops it being offered for chat.
    if (String(body.model ?? '').startsWith('nomic-embed-text')) {
      return json(response, 200, {
        capabilities: ['embedding'],
        model_info: { 'nomic-bert.context_length': 2048 },
      });
    }
    return json(response, 200, {
      capabilities: ['completion', 'tools'],
      model_info: { 'llama.context_length': 131072 },
    });
  }

  if (url === '/api/embeddings' && method === 'POST') {
    const body = await readBody(request);
    const input = String(body.prompt ?? '');
    const vector = new Array(8).fill(0);
    for (let index = 0; index < input.length; index += 1) {
      vector[index % 8] += input.charCodeAt(index) / 255;
    }
    return json(response, 200, { embedding: vector });
  }

  if (url === '/api/chat' && method === 'POST') {
    const body = await readBody(request);
    const model = body.model ?? '';
    if (!MODELS.includes(model)) {
      return json(response, 404, { error: `model '${model}' not found` });
    }

    response.writeHead(200, { 'content-type': 'application/x-ndjson' });
    const words = replyFor(body).split(' ');
    for (let index = 0; index < words.length; index += 1) {
      const chunk = index === 0 ? words[index] : ` ${words[index]}`;
      response.write(
        `${JSON.stringify({ model, message: { role: 'assistant', content: chunk }, done: false })}\n`,
      );
    }
    response.write(
      `${JSON.stringify({
        model,
        done: true,
        done_reason: 'stop',
        prompt_eval_count: 12,
        eval_count: words.length,
      })}\n`,
    );
    return response.end();
  }

  return json(response, 404, { error: 'not found' });
});

export function startFakeOllama() {
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({ url: `http://127.0.0.1:${port}`, close: () => server.close() });
    });
  });
}

if (import.meta.url === `file://${process.argv[1]}`) {
  startFakeOllama().then(({ url }) => console.log(url));
}
