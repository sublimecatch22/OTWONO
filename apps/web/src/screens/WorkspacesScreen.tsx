/** Offices, Labs, Boardrooms and Think Tanks. */

import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useNavigate, useParams } from 'react-router-dom';

import { api, ApiError } from '../api/client';
import type {
  Agent,
  LabResult,
  SessionDetail,
  WorkspaceDetail,
  WorkspaceKind,
  WorkspaceKindDescription,
  WorkspaceSummary,
} from '../api/types';
import { Markdown } from '../components/Markdown';
import {
  Badge,
  Button,
  Card,
  EmptyState,
  Field,
  Notice,
  Spinner,
} from '../components/primitives';
import { useUi } from '../state/ui';

export function WorkspacesScreen() {
  const navigate = useNavigate();
  const client = useQueryClient();
  const [kind, setKind] = useState<WorkspaceKind>('office');
  const [name, setName] = useState('');

  const kinds = useQuery({
    queryKey: ['workspaces', 'kinds'],
    queryFn: () => api.get<WorkspaceKindDescription[]>('/api/workspaces/kinds'),
  });
  const workspaces = useQuery({
    queryKey: ['workspaces'],
    queryFn: () => api.get<WorkspaceSummary[]>('/api/workspaces'),
  });

  const create = useMutation({
    mutationFn: () => api.post<WorkspaceSummary>('/api/workspaces', { kind, name }),
    onSuccess: (workspace) => {
      client.invalidateQueries({ queryKey: ['workspaces'] });
      client.invalidateQueries({ queryKey: ['workspaces', 'sidebar'] });
      navigate(`/workspaces/${workspace.id}`);
    },
  });

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Workspaces</h1>
          <p className="screen__lede">
            Four kinds of place to work, each with a different shape. They are not folders: each one
            behaves differently.
          </p>
        </div>
      </header>

      <div className="grid grid--two">
        {(kinds.data ?? [])
          .filter((description) => description.kind !== 'chat')
          .map((description) => (
            <Card key={description.kind} title={description.display_name}>
              <p>{description.purpose}</p>
              <p className="muted">
                {description.runs_sessions
                  ? 'Runs a structured session: positions, then critique, then a synthesis.'
                  : 'A standing place, not a one-off session.'}
              </p>
            </Card>
          ))}
      </div>

      <Card title="Create a workspace">
        <form
          className="row row--tight"
          onSubmit={(event) => {
            event.preventDefault();
            create.mutate();
          }}
        >
          <Field label="Kind">
            {({ id }) => (
              <select
                id={id}
                className="select"
                value={kind}
                onChange={(event) => setKind(event.target.value as WorkspaceKind)}
              >
                {(kinds.data ?? [])
                  .filter((description) => description.kind !== 'chat')
                  .map((description) => (
                    <option key={description.kind} value={description.kind}>
                      {description.display_name}
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
                value={name}
                onChange={(event) => setName(event.target.value)}
              />
            )}
          </Field>
          <Button type="submit" variant="primary" busy={create.isPending} disabled={!name.trim()}>
            Create
          </Button>
        </form>
      </Card>

      {workspaces.isLoading && <Spinner label="Loading workspaces" />}
      {workspaces.data?.length === 0 && (
        <EmptyState title="No workspaces yet" description="Create one above." />
      )}

      <ul className="stack">
        {(workspaces.data ?? []).map((workspace) => (
          <li key={workspace.id}>
            <button
              type="button"
              className="listbutton"
              onClick={() => navigate(`/workspaces/${workspace.id}`)}
            >
              <span className="row row--between">
                <strong>{workspace.name}</strong>
                <Badge tone="info">{workspace.kind.replace('_', ' ')}</Badge>
              </span>
              <span className="muted">{workspace.purpose}</span>
              <span className="muted">{workspace.member_count} agent(s)</span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

export function WorkspaceDetailScreen() {
  const { workspaceId } = useParams<{ workspaceId: string }>();
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [question, setQuestion] = useState('');
  const [openSession, setOpenSession] = useState<string | null>(null);

  const workspace = useQuery({
    queryKey: ['workspace', workspaceId],
    queryFn: () => api.get<WorkspaceDetail>(`/api/workspaces/${workspaceId}`),
    enabled: Boolean(workspaceId),
  });
  const agents = useQuery({ queryKey: ['agents'], queryFn: () => api.get<Agent[]>('/api/agents') });

  const invalidate = () => client.invalidateQueries({ queryKey: ['workspace', workspaceId] });

  const addMember = useMutation({
    mutationFn: (input: { agent_id: string; is_coordinator: boolean }) =>
      api.post(`/api/workspaces/${workspaceId}/members`, input),
    onSuccess: invalidate,
  });
  const removeMember = useMutation({
    mutationFn: (agentId: string) =>
      api.delete(`/api/workspaces/${workspaceId}/members/${agentId}`),
    onSuccess: invalidate,
  });
  const updateWorkspace = useMutation({
    mutationFn: (patch: Record<string, unknown>) =>
      api.put(`/api/workspaces/${workspaceId}`, patch),
    onSuccess: invalidate,
  });
  const createSession = useMutation({
    mutationFn: () => api.post<SessionDetail>(`/api/workspaces/${workspaceId}/sessions`, { question }),
    onSuccess: (session) => {
      invalidate();
      setQuestion('');
      setOpenSession(session.id);
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });
  const runSession = useMutation({
    mutationFn: (sessionId: string) =>
      api.post<SessionDetail>(`/api/workspaces/${workspaceId}/sessions/${sessionId}/run`),
    onSuccess: (_result, sessionId) => {
      invalidate();
      // The open transcript is a query of its own, so without this the message
      // says the synthesis is below while the panel still shows the old state.
      client.invalidateQueries({ queryKey: ['session', sessionId] });
      setOpenSession(sessionId);
      toast({ tone: 'positive', body: 'The session finished. The synthesis is below.' });
    },
    onError: (error) =>
      toast({
        tone: 'negative',
        title: 'The session could not run',
        body: error instanceof ApiError ? error.message : String(error),
      }),
  });

  if (workspace.isLoading)
    return <div className="screen"><Spinner label="Loading the workspace" /></div>;
  if (!workspace.data)
    return (
      <div className="screen">
        <Notice tone="negative">That workspace could not be loaded.</Notice>
      </div>
    );

  const data = workspace.data;
  const memberIds = new Set(data.members.map((member) => member.agent.id));

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>{data.name}</h1>
          <p className="screen__lede">{data.purpose}</p>
        </div>
        <Badge tone="info">{data.kind.replace('_', ' ')}</Badge>
      </header>

      <Card title="Shared instructions" description="Given to every agent working here.">
        <Field label="Instructions">
          {({ id }) => (
            <textarea
              id={id}
              className="textarea"
              rows={4}
              defaultValue={data.shared_instructions}
              onBlur={(event) =>
                updateWorkspace.mutate({ shared_instructions: event.target.value })
              }
            />
          )}
        </Field>
      </Card>

      <Card title={`Team (${data.members.length})`}>
        {data.members.length === 0 ? (
          <p className="muted">No agents yet. Add at least two before running a session.</p>
        ) : (
          <ul className="stack">
            {data.members.map((member) => (
              <li key={member.agent.id} className="row">
                <div>
                  <strong>{member.agent.name}</strong>
                  <span className="muted"> · {member.job_role}</span>
                  {member.is_coordinator && <> <Badge tone="accent">coordinator</Badge></>}
                </div>
                <div className="row row--tight">
                  {!member.is_coordinator && (
                    <Button
                      size="sm"
                      onClick={() =>
                        addMember.mutate({ agent_id: member.agent.id, is_coordinator: true })
                      }
                    >
                      Make coordinator
                    </Button>
                  )}
                  <Button size="sm" variant="danger" onClick={() => removeMember.mutate(member.agent.id)}>
                    Remove
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        )}

        <Field label="Add an agent">
          {({ id }) => (
            <select
              id={id}
              className="select"
              value=""
              onChange={(event) => {
                if (event.target.value) {
                  addMember.mutate({ agent_id: event.target.value, is_coordinator: false });
                }
              }}
            >
              <option value="">Choose an agent…</option>
              {(agents.data ?? [])
                .filter((agent) => !memberIds.has(agent.id))
                .map((agent) => (
                  <option key={agent.id} value={agent.id}>
                    {agent.name} — {agent.role}
                  </option>
                ))}
            </select>
          )}
        </Field>
      </Card>

      {data.runs_sessions && (
        <Card
          title="Sessions"
          description={
            data.kind === 'boardroom'
              ? 'Each agent gives an independent position, then challenges the others, then the chair writes the synthesis and the dissent.'
              : 'Each agent proposes, then critiques, then the editor writes a research brief separating sourced findings from speculation.'
          }
        >
          <form
            className="row row--tight"
            onSubmit={(event) => {
              event.preventDefault();
              createSession.mutate();
            }}
          >
            <label className="visually-hidden" htmlFor="session-question">
              The question for this session
            </label>
            <input
              id="session-question"
              className="input"
              value={question}
              placeholder={
                data.kind === 'boardroom' ? 'Should we ship on Friday?' : 'What should we research next?'
              }
              onChange={(event) => setQuestion(event.target.value)}
            />
            <Button
              type="submit"
              variant="primary"
              busy={createSession.isPending}
              disabled={!question.trim()}
            >
              Start a session
            </Button>
          </form>

          <ul className="stack">
            {data.sessions.map((session) => (
              <li key={session.id} className="stack">
                <div className="row row--between">
                  <strong>{session.question}</strong>
                  <span className="row row--tight">
                    <Badge tone={session.stage === 'completed' ? 'positive' : 'info'}>
                      {session.stage}
                    </Badge>
                    {session.stage !== 'completed' && (
                      <Button
                        size="sm"
                        variant="primary"
                        busy={runSession.isPending}
                        onClick={() => runSession.mutate(session.id)}
                      >
                        Run
                      </Button>
                    )}
                    <Button
                      size="sm"
                      onClick={() => setOpenSession(openSession === session.id ? null : session.id)}
                    >
                      {openSession === session.id ? 'Hide' : 'Open'}
                    </Button>
                  </span>
                </div>
                {openSession === session.id && (
                  <SessionView workspaceId={data.id} sessionId={session.id} />
                )}
              </li>
            ))}
          </ul>
        </Card>
      )}

      {data.kind === 'lab' && <LabPanel workspaceId={data.id} agents={agents.data ?? []} />}
    </div>
  );
}

function SessionView({ workspaceId, sessionId }: { workspaceId: string; sessionId: string }) {
  const session = useQuery({
    queryKey: ['session', sessionId],
    queryFn: () =>
      api.get<SessionDetail>(`/api/workspaces/${workspaceId}/sessions/${sessionId}`),
  });

  if (session.isLoading) return <Spinner label="Loading the session" />;
  if (!session.data) return null;
  const data = session.data;

  return (
    <div className="stack session">
      {data.synthesis && (
        <Card title="Synthesis">
          <Markdown source={data.synthesis} />
          {data.dissent_summary && (
            <>
              <h3>Dissent</h3>
              <Markdown source={data.dissent_summary} />
            </>
          )}
          {data.unresolved_questions.length > 0 && (
            <>
              <h3>Unresolved</h3>
              <ul>
                {data.unresolved_questions.map((item) => (
                  <li key={item}>{item}</li>
                ))}
              </ul>
            </>
          )}
          {data.recommended_decision && (
            <>
              <h3>Recommended decision</h3>
              <p>{data.recommended_decision}</p>
            </>
          )}
        </Card>
      )}

      <Card title={`Transcript (${data.contributions.length})`}>
        <ol className="stack">
          {data.contributions.map((contribution) => (
            <li key={contribution.id}>
              <div className="row row--between">
                <strong>{contribution.agent_name}</strong>
                <span className="row row--tight">
                  <Badge tone="neutral">{contribution.stage}</Badge>
                  <Badge tone={contribution.claim_kind === 'sourced' ? 'positive' : 'caution'}>
                    {contribution.claim_kind}
                  </Badge>
                </span>
              </div>
              <Markdown source={contribution.content} />
            </li>
          ))}
        </ol>
      </Card>
    </div>
  );
}

function LabPanel({ workspaceId, agents }: { workspaceId: string; agents: Agent[] }) {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [name, setName] = useState('Prompt comparison');
  const [prompt, setPrompt] = useState('');
  const [variantA, setVariantA] = useState('Answer in one sentence.');
  const [variantB, setVariantB] = useState('Answer thoroughly, with reasoning.');
  const [results, setResults] = useState<LabResult[] | null>(null);
  const [experimentId, setExperimentId] = useState<string | null>(null);

  const workspace = useQuery({
    queryKey: ['workspace', workspaceId],
    queryFn: () => api.get<WorkspaceDetail>(`/api/workspaces/${workspaceId}`),
  });

  const createExperiment = useMutation({
    mutationFn: () =>
      api.post<{ id: string }>(`/api/workspaces/${workspaceId}/experiments`, {
        name,
        prompt,
        variants: [
          {
            id: 'a',
            label: 'A',
            agent_id: agents[0]?.id ?? null,
            provider_connection_id: null,
            model: agents[0]?.model ?? null,
            system_instructions: variantA,
            temperature: 0.2,
          },
          {
            id: 'b',
            label: 'B',
            agent_id: agents[0]?.id ?? null,
            provider_connection_id: null,
            model: agents[0]?.model ?? null,
            system_instructions: variantB,
            temperature: 0.8,
          },
        ],
      }),
    onSuccess: (experiment) => {
      setExperimentId(experiment.id);
      client.invalidateQueries({ queryKey: ['workspace', workspaceId] });
    },
  });

  const runExperiment = useMutation({
    mutationFn: (id: string) =>
      api.post<LabResult[]>(`/api/workspaces/${workspaceId}/experiments/${id}/run`),
    onSuccess: setResults,
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  const promote = useMutation({
    mutationFn: (input: { experimentId: string; variantId: string; agentId: string }) =>
      api.post(`/api/workspaces/${workspaceId}/experiments/${input.experimentId}/promote`, {
        variant_id: input.variantId,
        target_agent_id: input.agentId,
      }),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['agents'] });
      toast({
        tone: 'positive',
        body: 'Promoted. The agent kept its previous version in its history.',
      });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  return (
    <Card
      title="Experiments"
      description="Compare configurations here. Nothing in an Office changes until you promote a result."
    >
      <Field label="What to ask">
        {({ id }) => (
          <textarea
            id={id}
            className="textarea"
            rows={3}
            value={prompt}
            placeholder="Summarise this quarter's results for a non-specialist."
            onChange={(event) => setPrompt(event.target.value)}
          />
        )}
      </Field>
      <div className="grid grid--two">
        <Field label="Variant A instructions">
          {({ id }) => (
            <textarea
              id={id}
              className="textarea"
              rows={3}
              value={variantA}
              onChange={(event) => setVariantA(event.target.value)}
            />
          )}
        </Field>
        <Field label="Variant B instructions">
          {({ id }) => (
            <textarea
              id={id}
              className="textarea"
              rows={3}
              value={variantB}
              onChange={(event) => setVariantB(event.target.value)}
            />
          )}
        </Field>
      </div>
      <Field label="Experiment name">
        {({ id }) => (
          <input
            id={id}
            className="input"
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        )}
      </Field>

      <div className="row row--tight">
        <Button
          variant="primary"
          busy={createExperiment.isPending}
          disabled={!prompt.trim()}
          onClick={() => createExperiment.mutate()}
        >
          Create experiment
        </Button>
        {experimentId && (
          <Button busy={runExperiment.isPending} onClick={() => runExperiment.mutate(experimentId)}>
            Run both
          </Button>
        )}
      </div>

      {results && (
        <div className="grid grid--two">
          {results.map((result) => (
            <Card key={result.variant_id} title={`Variant ${result.variant_id.toUpperCase()}`}>
              {result.error ? (
                <Notice tone="negative">{result.error}</Notice>
              ) : (
                <>
                  <p className="muted">{result.latency_ms} ms</p>
                  <pre className="output">{result.output}</pre>
                  <Field label="Promote onto">
                    {({ id }) => (
                      <select
                        id={id}
                        className="select"
                        value=""
                        onChange={(event) => {
                          if (event.target.value && experimentId) {
                            promote.mutate({
                              experimentId,
                              variantId: result.variant_id,
                              agentId: event.target.value,
                            });
                          }
                        }}
                      >
                        <option value="">Choose an agent…</option>
                        {agents.map((agent) => (
                          <option key={agent.id} value={agent.id}>
                            {agent.name}
                          </option>
                        ))}
                      </select>
                    )}
                  </Field>
                </>
              )}
            </Card>
          ))}
        </div>
      )}

      {(workspace.data?.experiments.length ?? 0) > 0 && (
        <details>
          <summary>Earlier experiments</summary>
          <ul className="stack">
            {(workspace.data?.experiments ?? []).map((experiment) => (
              <li key={experiment.id} className="row">
                <div>
                  <strong>{experiment.name}</strong>
                  <p className="muted">{experiment.prompt.slice(0, 120)}</p>
                </div>
                <Button size="sm" onClick={() => setExperimentId(experiment.id)}>
                  Select
                </Button>
              </li>
            ))}
          </ul>
        </details>
      )}
    </Card>
  );
}
