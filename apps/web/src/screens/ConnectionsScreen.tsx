/** Local AI runtimes: detection, testing, models and credentials. */

import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { api, ApiError } from '../api/client';
import type {
  ConnectionsResponse,
  ConnectionTest,
  DetectionResponse,
  ModelInfo,
  ProviderConnection,
} from '../api/types';
import { Badge, Button, Card, EmptyState, Field, Notice, Spinner } from '../components/primitives';
import { useUi } from '../state/ui';

export function ConnectionsScreen() {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [tests, setTests] = useState<Record<string, ConnectionTest>>({});
  const [showManual, setShowManual] = useState(false);

  const connections = useQuery({
    queryKey: ['connections'],
    queryFn: () => api.get<ConnectionsResponse>('/api/connections'),
  });

  const detect = useMutation({
    mutationFn: () => api.post<DetectionResponse>('/api/connections/detect'),
  });

  const createFromDetection = useMutation({
    mutationFn: (input: { kind: string; label: string; endpoint: string; model: string | null }) =>
      api.post<ProviderConnection>('/api/connections', {
        kind: input.kind,
        label: input.label,
        endpoint: input.endpoint,
        default_model: input.model,
        enabled: true,
      }),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['connections'] });
      toast({ tone: 'positive', body: 'Connection added.' });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  const test = useMutation({
    mutationFn: (id: string) => api.post<ConnectionTest>(`/api/connections/${id}/test`),
    onSuccess: (result, id) => setTests((prev) => ({ ...prev, [id]: result })),
  });

  const update = useMutation({
    mutationFn: (input: { id: string; patch: Record<string, unknown> }) =>
      api.put<ProviderConnection>(`/api/connections/${input.id}`, input.patch),
    // The switches and dropdowns here are controlled by the query cache, so
    // without this they snap back to their old value until the refetch lands.
    // The change is shown at once and rolled back if the service refuses it.
    onMutate: async (input) => {
      await client.cancelQueries({ queryKey: ['connections'] });
      const previous = client.getQueryData<ConnectionsResponse>(['connections']);
      client.setQueryData<ConnectionsResponse>(['connections'], (current) =>
        current
          ? {
              ...current,
              connections: current.connections.map((connection) =>
                connection.id === input.id
                  ? { ...connection, ...displayableFields(input.patch) }
                  : connection,
              ),
            }
          : current,
      );
      return { previous };
    },
    onError: (error, _input, context) => {
      if (context?.previous) client.setQueryData(['connections'], context.previous);
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) });
    },
    onSettled: () => client.invalidateQueries({ queryKey: ['connections'] }),
  });

  const remove = useMutation({
    mutationFn: (id: string) => api.delete(`/api/connections/${id}`),
    onSuccess: () => client.invalidateQueries({ queryKey: ['connections'] }),
  });

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Connections</h1>
          <p className="screen__lede">
            OTWONO talks to AI models running on this machine. Nothing is sent anywhere else unless
            you add a connection that points off the device and give it a key.
          </p>
        </div>
        <Button variant="primary" busy={detect.isPending} onClick={() => detect.mutate()}>
          Find local runtimes
        </Button>
      </header>

      {connections.data && !connections.data.ready_for_chat && (
        <Notice tone="info" title="No model connected yet">
          {connections.data.guidance}
        </Notice>
      )}

      {detect.data && (
        <Card title="Detected on this machine" description={detect.data.guidance}>
          <ul className="stack">
            {detect.data.found.map((runtime) => (
              <li key={runtime.endpoint} className="row">
                <div>
                  <strong>{runtime.display_name}</strong>{' '}
                  <span className="muted">{runtime.endpoint}</span>
                  <p className="muted">{runtime.test.detail}</p>
                </div>
                <div className="row__actions">
                  {runtime.existing_connection_id ? (
                    <Badge tone="positive">Already added</Badge>
                  ) : runtime.usable ? (
                    <Button
                      variant="primary"
                      size="sm"
                      busy={createFromDetection.isPending}
                      onClick={() =>
                        createFromDetection.mutate({
                          kind: runtime.kind,
                          label: runtime.display_name,
                          endpoint: runtime.endpoint,
                          model: runtime.test.models[0]?.id ?? null,
                        })
                      }
                    >
                      Use this
                    </Button>
                  ) : (
                    <Badge tone="neutral">Not available</Badge>
                  )}
                </div>
              </li>
            ))}
          </ul>
        </Card>
      )}

      {connections.isLoading && <Spinner label="Loading connections" />}

      {connections.data?.connections.length === 0 && !detect.data && (
        <EmptyState
          title="No connections yet"
          description="Find a runtime running on this machine, or add an OpenAI-compatible endpoint by hand."
          action={
            <Button variant="primary" onClick={() => detect.mutate()} busy={detect.isPending}>
              Find local runtimes
            </Button>
          }
        />
      )}

      {(connections.data?.connections ?? []).map((connection) => {
        const result = tests[connection.id];
        return (
          <Card
            key={connection.id}
            title={connection.label}
            description={connection.endpoint}
            actions={
              <>
                <Button size="sm" busy={test.isPending} onClick={() => test.mutate(connection.id)}>
                  Test
                </Button>
                <Button
                  size="sm"
                  variant="danger"
                  onClick={() => remove.mutate(connection.id)}
                >
                  Remove
                </Button>
              </>
            }
          >
            <div className="grid grid--two">
              <Field label="Default model">
                {({ id }) => (
                  <select
                    id={id}
                    className="select"
                    value={connection.default_model ?? ''}
                    onChange={(event) =>
                      update.mutate({
                        id: connection.id,
                        patch: { default_model: event.target.value || null },
                      })
                    }
                  >
                    <option value="">Not chosen</option>
                    {(result?.models ?? []).map((model) => (
                      <option key={model.id} value={model.id}>
                        {model.id}
                      </option>
                    ))}
                    {connection.default_model &&
                      !(result?.models ?? []).some((m) => m.id === connection.default_model) && (
                        <option value={connection.default_model}>{connection.default_model}</option>
                      )}
                  </select>
                )}
              </Field>

              <Field
                label="Embedding model"
                hint="Used to index your files. Without one, search matches words rather than meaning."
              >
                {({ id, describedBy }) => (
                  <select
                    id={id}
                    aria-describedby={describedBy}
                    className="select"
                    value={connection.default_embedding_model ?? ''}
                    onChange={(event) =>
                      update.mutate({
                        id: connection.id,
                        patch: { default_embedding_model: event.target.value || null },
                      })
                    }
                  >
                    <option value="">None</option>
                    {(result?.models ?? [])
                      .filter((model) => model.capabilities.embeddings)
                      .map((model) => (
                        <option key={model.id} value={model.id}>
                          {model.id}
                        </option>
                      ))}
                    {connection.default_embedding_model && (
                      <option value={connection.default_embedding_model}>
                        {connection.default_embedding_model}
                      </option>
                    )}
                  </select>
                )}
              </Field>
            </div>

            <label className="checkbox">
              <input
                type="checkbox"
                checked={connection.enabled}
                onChange={(event) =>
                  update.mutate({ id: connection.id, patch: { enabled: event.target.checked } })
                }
              />
              <span>Use this connection</span>
            </label>

            {connection.kind === 'openai_compatible' && (
              <ApiKeyField
                connection={connection}
                onSave={(key) =>
                  update.mutate({ id: connection.id, patch: { api_key: key } })
                }
              />
            )}

            {result && (
              <div className="testresult">
                <Badge
                  tone={
                    result.health === 'reachable'
                      ? 'positive'
                      : result.health === 'authentication_required'
                        ? 'caution'
                        : 'negative'
                  }
                >
                  {result.health.replace(/_/g, ' ')}
                </Badge>
                <p>{result.detail}</p>
                {result.models.length > 0 && <ModelTable models={result.models} />}
              </div>
            )}
          </Card>
        );
      })}

      <Card
        title="Add a connection by hand"
        description="A runtime on a port other than the usual one, or any OpenAI-compatible endpoint: llama.cpp's server, vLLM, LocalAI, or a hosted gateway."
        actions={
          <Button size="sm" onClick={() => setShowManual((open) => !open)}>
            {showManual ? 'Hide' : 'Show'}
          </Button>
        }
      >
        {showManual && <ManualConnectionForm />}
      </Card>
    </div>
  );
}

/**
 * The parts of a patch that are safe to show before the service has agreed.
 * `api_key` is deliberately absent: the key itself is never held in the cache,
 * and whether one is stored is the service's answer to give.
 */
function displayableFields(patch: Record<string, unknown>): Partial<ProviderConnection> {
  const allowed = ['label', 'endpoint', 'default_model', 'default_embedding_model', 'enabled'];
  return Object.fromEntries(
    Object.entries(patch).filter(([key]) => allowed.includes(key)),
  ) as Partial<ProviderConnection>;
}

function ApiKeyField({
  connection,
  onSave,
}: {
  connection: ProviderConnection;
  onSave: (key: string | null) => void;
}) {
  const [value, setValue] = useState('');
  return (
    <Field
      label="API key"
      hint="Stored in your operating system's credential manager, never in the OTWONO database."
    >
      {({ id, describedBy }) => (
        <div className="row row--tight">
          <input
            id={id}
            aria-describedby={describedBy}
            className="input"
            type="password"
            autoComplete="off"
            placeholder={connection.has_credential ? '•••••••• (saved)' : 'Not set'}
            value={value}
            onChange={(event) => setValue(event.target.value)}
          />
          <Button
            size="sm"
            disabled={!value.trim()}
            onClick={() => {
              onSave(value.trim());
              setValue('');
            }}
          >
            Save
          </Button>
          {connection.has_credential && (
            <Button size="sm" variant="danger" onClick={() => onSave(null)}>
              Remove
            </Button>
          )}
        </div>
      )}
    </Field>
  );
}

function ModelTable({ models }: { models: ModelInfo[] }) {
  return (
    <div className="tablewrap">
      <table className="table">
        <caption className="visually-hidden">Models this connection can serve</caption>
        <thead>
          <tr>
            <th scope="col">Model</th>
            <th scope="col">Chat</th>
            <th scope="col">Tools</th>
            <th scope="col">Vision</th>
            <th scope="col">Embeddings</th>
            <th scope="col">Context</th>
            <th scope="col">How we know</th>
          </tr>
        </thead>
        <tbody>
          {models.map((model) => (
            <tr key={model.id}>
              <th scope="row">{model.id}</th>
              <td>{model.capabilities.chat ? 'Yes' : 'No'}</td>
              <td>{model.capabilities.tool_calling ? 'Yes' : 'No'}</td>
              <td>{model.capabilities.vision ? 'Yes' : 'No'}</td>
              <td>{model.capabilities.embeddings ? 'Yes' : 'No'}</td>
              <td>{model.capabilities.context_length?.toLocaleString() ?? '—'}</td>
              <td>
                <Badge
                  tone={
                    model.capability_source === 'reported'
                      ? 'positive'
                      : model.capability_source === 'probed'
                        ? 'info'
                        : 'caution'
                  }
                >
                  {model.capability_source === 'inferred' ? 'guessed from the name' : model.capability_source}
                </Badge>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/**
 * The kinds a person can add by hand, with the address each one usually
 * listens on. Ollama and LM Studio are here because a runtime moved to a
 * different port is invisible to detection but perfectly usable.
 */
const MANUAL_KINDS = [
  { id: 'ollama', name: 'Ollama', endpoint: 'http://127.0.0.1:11434' },
  { id: 'lmstudio', name: 'LM Studio', endpoint: 'http://127.0.0.1:1234' },
  { id: 'openai_compatible', name: 'OpenAI-compatible endpoint', endpoint: 'http://127.0.0.1:8080' },
] as const;

function ManualConnectionForm() {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [kind, setKind] = useState<string>('ollama');
  const [label, setLabel] = useState('');
  const [endpoint, setEndpoint] = useState<string>(MANUAL_KINDS[0].endpoint);
  const [apiKey, setApiKey] = useState('');
  const [error, setError] = useState<string | null>(null);

  const chosen = MANUAL_KINDS.find((entry) => entry.id === kind) ?? MANUAL_KINDS[2];

  const create = useMutation({
    mutationFn: () =>
      api.post<ProviderConnection>('/api/connections', {
        kind,
        label: label || chosen.name,
        endpoint,
        api_key: apiKey || null,
        enabled: false,
      }),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['connections'] });
      toast({
        tone: 'positive',
        body: 'Connection added. Test it, choose a model, then switch it on.',
      });
      setLabel('');
      setApiKey('');
      setError(null);
    },
    onError: (caught) =>
      setError(caught instanceof ApiError ? caught.message : String(caught)),
  });

  return (
    <form
      className="stack"
      onSubmit={(event) => {
        event.preventDefault();
        create.mutate();
      }}
    >
      <Field
        label="Runtime"
        hint="Pick the software that is listening. The wrong choice will fail the connection test rather than misbehave later."
      >
        {({ id, describedBy }) => (
          <select
            id={id}
            aria-describedby={describedBy}
            className="select"
            value={kind}
            onChange={(event) => {
              const next =
                MANUAL_KINDS.find((entry) => entry.id === event.target.value) ?? MANUAL_KINDS[2];
              setKind(next.id);
              // Only move the address if it is still one of our suggestions,
              // so a typed address is never thrown away.
              setEndpoint((current) =>
                MANUAL_KINDS.some((entry) => entry.endpoint === current) ? next.endpoint : current,
              );
            }}
          >
            {MANUAL_KINDS.map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.name}
              </option>
            ))}
          </select>
        )}
      </Field>
      <Field label="Name">
        {({ id }) => (
          <input
            id={id}
            className="input"
            value={label}
            placeholder="My local server"
            onChange={(event) => setLabel(event.target.value)}
          />
        )}
      </Field>
      <Field label="Address" error={error}>
        {({ id, describedBy }) => (
          <input
            id={id}
            aria-describedby={describedBy}
            className="input"
            value={endpoint}
            onChange={(event) => setEndpoint(event.target.value)}
          />
        )}
      </Field>
      <Field
        label="API key (optional)"
        hint="Needed only for an endpoint that requires one. Stored in your OS credential manager."
      >
        {({ id, describedBy }) => (
          <input
            id={id}
            aria-describedby={describedBy}
            className="input"
            type="password"
            autoComplete="off"
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
          />
        )}
      </Field>
      <Button type="submit" variant="primary" busy={create.isPending}>
        Add connection
      </Button>
    </form>
  );
}
