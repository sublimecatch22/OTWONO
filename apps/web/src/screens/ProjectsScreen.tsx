/** Projects: objective, plan, approvals, execution and the completion report. */

import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useNavigate, useParams } from 'react-router-dom';

import { api, ApiError } from '../api/client';
import type {
  Agent,
  ProjectDetail,
  ProjectSummary,
  RunReport,
  Task,
  WorkspaceSummary,
} from '../api/types';
import { Markdown } from '../components/Markdown';
import {
  Button,
  Card,
  DetailList,
  EmptyState,
  Field,
  Notice,
  Spinner,
  TimeAgo,
} from '../components/primitives';
import { ProjectStateBadge, TaskStateBadge } from '../components/StateBadge';
import { useUi } from '../state/ui';

export function ProjectsScreen() {
  const navigate = useNavigate();
  const client = useQueryClient();
  const [title, setTitle] = useState('');
  const [objective, setObjective] = useState('');
  const [criteria, setCriteria] = useState('');
  const [workspaceId, setWorkspaceId] = useState('');

  const projects = useQuery({
    queryKey: ['projects'],
    queryFn: () => api.get<ProjectSummary[]>('/api/projects'),
  });
  const workspaces = useQuery({
    queryKey: ['workspaces'],
    queryFn: () => api.get<WorkspaceSummary[]>('/api/workspaces'),
  });

  const create = useMutation({
    mutationFn: () =>
      api.post<ProjectDetail>('/api/projects', {
        title,
        objective,
        acceptance_criteria: criteria
          .split('\n')
          .map((line) => line.trim())
          .filter(Boolean),
        workspace_id: workspaceId || null,
      }),
    onSuccess: (project) => {
      client.invalidateQueries({ queryKey: ['projects'] });
      client.invalidateQueries({ queryKey: ['projects', 'sidebar'] });
      navigate(`/projects/${project.id}`);
    },
  });

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Projects</h1>
          <p className="screen__lede">
            Describe an outcome. OTWONO turns it into a plan you can read and approve before
            anything runs.
          </p>
        </div>
      </header>

      <Card title="Start a project">
        <form
          className="stack"
          onSubmit={(event) => {
            event.preventDefault();
            create.mutate();
          }}
        >
          <Field label="What are you trying to achieve?">
            {({ id }) => (
              <input
                id={id}
                className="input"
                value={title}
                placeholder="Quarterly report for Q3"
                onChange={(event) => setTitle(event.target.value)}
              />
            )}
          </Field>
          <Field label="Say more about it">
            {({ id }) => (
              <textarea
                id={id}
                className="textarea"
                rows={3}
                value={objective}
                placeholder="Summarise the Q3 numbers for the board, with the risks called out."
                onChange={(event) => setObjective(event.target.value)}
              />
            )}
          </Field>
          <Field
            label="How will you know it is done?"
            hint="One criterion per line. These are what the verifier checks against."
          >
            {({ id, describedBy }) => (
              <textarea
                id={id}
                aria-describedby={describedBy}
                className="textarea"
                rows={3}
                value={criteria}
                placeholder={'Includes revenue for all three months\nUnder 800 words'}
                onChange={(event) => setCriteria(event.target.value)}
              />
            )}
          </Field>
          <Field
            label="Where this belongs"
            hint="An office or lab whose team should own the work. Optional."
          >
            {({ id, describedBy }) => (
              <select
                id={id}
                aria-describedby={describedBy}
                className="select"
                value={workspaceId}
                onChange={(event) => setWorkspaceId(event.target.value)}
              >
                <option value="">Nowhere in particular</option>
                {(workspaces.data ?? []).map((workspace) => (
                  <option key={workspace.id} value={workspace.id}>
                    {workspace.name}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Button type="submit" variant="primary" busy={create.isPending} disabled={!title.trim()}>
            Create project
          </Button>
        </form>
      </Card>

      {projects.isLoading && <Spinner label="Loading projects" />}

      {projects.data?.length === 0 && (
        <EmptyState title="No projects yet" description="Start one above." />
      )}

      <ul className="stack">
        {(projects.data ?? []).map((project) => (
          <li key={project.id}>
            <button
              type="button"
              className="listbutton"
              onClick={() => navigate(`/projects/${project.id}`)}
            >
              <span className="row row--between">
                <strong>{project.title}</strong>
                <ProjectStateBadge state={project.state} />
              </span>
              <span className="muted">{project.objective}</span>
              <span className="muted">
                {project.completed_tasks}/{project.task_count} tasks done
                {project.awaiting_approval > 0 && ` · ${project.awaiting_approval} awaiting you`}
                {' · updated '}
                <TimeAgo value={project.updated_at} />
              </span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

export function ProjectDetailScreen() {
  const { projectId } = useParams<{ projectId: string }>();
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [report, setReport] = useState<string | null>(null);
  const [lastRun, setLastRun] = useState<RunReport | null>(null);

  const project = useQuery({
    queryKey: ['project', projectId],
    queryFn: () => api.get<ProjectDetail>(`/api/projects/${projectId}`),
    enabled: Boolean(projectId),
  });
  const agents = useQuery({ queryKey: ['agents'], queryFn: () => api.get<Agent[]>('/api/agents') });
  const workspaces = useQuery({
    queryKey: ['workspaces'],
    queryFn: () => api.get<WorkspaceSummary[]>('/api/workspaces'),
  });

  const invalidate = () => {
    client.invalidateQueries({ queryKey: ['project', projectId] });
    client.invalidateQueries({ queryKey: ['projects'] });
  };

  const plan = useMutation({
    mutationFn: () => api.post<Task[]>(`/api/projects/${projectId}/plan`),
    onSuccess: (tasks) => {
      invalidate();
      toast({ tone: 'positive', body: `Planned ${tasks.length} task(s). Read it before running.` });
    },
    onError: (error) =>
      toast({
        tone: 'negative',
        title: 'Planning failed',
        body: error instanceof ApiError ? error.message : String(error),
      }),
  });

  const run = useMutation({
    mutationFn: () => api.post<RunReport>(`/api/projects/${projectId}/run`),
    onSuccess: (result) => {
      invalidate();
      setLastRun(result);
      toast({
        tone: result.tasks_failed > 0 ? 'caution' : 'positive',
        title: `Run finished (${result.final_state})`,
        body: result.stopped_because,
      });
    },
    onError: (error) =>
      toast({
        tone: 'negative',
        title: 'The run could not start',
        body: error instanceof ApiError ? error.message : String(error),
      }),
  });

  const decide = useMutation({
    mutationFn: (input: { taskId: string; approve: boolean; reason?: string }) =>
      api.post(`/api/projects/${projectId}/tasks/${input.taskId}/decision`, {
        approve: input.approve,
        reason: input.reason,
      }),
    onSuccess: invalidate,
  });

  const updateProject = useMutation({
    mutationFn: (patch: Record<string, unknown>) => api.put(`/api/projects/${projectId}`, patch),
    onSuccess: invalidate,
  });

  if (project.isLoading)
    return (
      <div className="screen">
        <Spinner label="Loading the project" />
      </div>
    );
  if (!project.data)
    return (
      <div className="screen">
        <Notice tone="negative">That project could not be loaded.</Notice>
      </div>
    );

  const data = project.data;
  const awaiting = data.tasks.filter((task) => task.state === 'awaiting_approval');

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>{data.title}</h1>
          <p className="screen__lede">{data.objective}</p>
        </div>
        <div className="row row--tight">
          <ProjectStateBadge state={data.state} />
          {data.state === 'draft' && (
            <Button variant="primary" busy={plan.isPending} onClick={() => plan.mutate()}>
              Plan the work
            </Button>
          )}
          {['planned', 'awaiting_approval', 'blocked', 'running'].includes(data.state) && (
            <Button variant="primary" busy={run.isPending} onClick={() => run.mutate()}>
              {data.state === 'planned' ? 'Approve and run' : 'Continue running'}
            </Button>
          )}
          <Button
            onClick={async () => setReport(await api.text(`/api/projects/${projectId}/report`))}
          >
            Completion report
          </Button>
        </div>
      </header>

      {awaiting.length > 0 && (
        <Notice tone="caution" title="Waiting for your decision">
          <ul className="stack">
            {awaiting.map((task) => (
              <li key={task.id} className="row">
                <div>
                  <strong>{task.title}</strong>
                  <p className="muted">{task.instructions}</p>
                </div>
                <div className="row row--tight">
                  <Button
                    size="sm"
                    variant="primary"
                    onClick={() => decide.mutate({ taskId: task.id, approve: true })}
                  >
                    Approve
                  </Button>
                  <Button
                    size="sm"
                    variant="danger"
                    onClick={() =>
                      decide.mutate({
                        taskId: task.id,
                        approve: false,
                        reason: 'Declined from the project screen',
                      })
                    }
                  >
                    Decline
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        </Notice>
      )}

      {lastRun && (
        <Notice tone={lastRun.tasks_failed > 0 ? 'caution' : 'info'} title="Last run">
          {lastRun.stopped_because} — {lastRun.steps_used} step(s), {lastRun.tasks_completed}{' '}
          completed, {lastRun.tasks_reworked} reworked, {lastRun.tasks_failed} failed.
        </Notice>
      )}

      <Card title="Settings">
        <div className="grid grid--two">
          <Field label="Orchestrator">
            {({ id }) => (
              <select
                id={id}
                className="select"
                value={data.orchestrator_agent_id ?? ''}
                onChange={(event) =>
                  updateProject.mutate({ orchestrator_agent_id: event.target.value || null })
                }
              >
                <option value="">Not chosen</option>
                {(agents.data ?? []).map((agent) => (
                  <option key={agent.id} value={agent.id}>
                    {agent.name}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field
            label="Verifier"
            hint="Without one, finished work is reported as unchecked rather than passed."
          >
            {({ id, describedBy }) => (
              <select
                id={id}
                aria-describedby={describedBy}
                className="select"
                value={data.verifier_agent_id ?? ''}
                onChange={(event) =>
                  updateProject.mutate({ verifier_agent_id: event.target.value || null })
                }
              >
                <option value="">Not chosen</option>
                {(agents.data ?? []).map((agent) => (
                  <option key={agent.id} value={agent.id}>
                    {agent.name}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field label="Workspace" hint="The office or lab this project belongs to.">
            {({ id, describedBy }) => (
              <select
                id={id}
                aria-describedby={describedBy}
                className="select"
                value={data.workspace_id ?? ''}
                onChange={(event) =>
                  updateProject.mutate({ workspace_id: event.target.value || null })
                }
              >
                <option value="">Nowhere in particular</option>
                {(workspaces.data ?? []).map((workspace) => (
                  <option key={workspace.id} value={workspace.id}>
                    {workspace.name}
                  </option>
                ))}
              </select>
            )}
          </Field>
        </div>
        <DetailList
          items={[
            { label: 'Step budget', value: `${data.max_steps} steps` },
            { label: 'Retries per task', value: String(data.max_task_retries) },
            {
              label: 'Acceptance criteria',
              value:
                data.acceptance_criteria.length === 0 ? (
                  <span className="muted">None stated</span>
                ) : (
                  <ul>
                    {data.acceptance_criteria.map((criterion) => (
                      <li key={criterion}>{criterion}</li>
                    ))}
                  </ul>
                ),
            },
            {
              label: 'Synchronise to my account',
              value: (
                <label className="checkbox">
                  <input
                    type="checkbox"
                    checked={data.sync_enabled}
                    onChange={(event) =>
                      updateProject.mutate({ sync_enabled: event.target.checked })
                    }
                  />
                  <span>
                    Send this project's title and task states to a linked account. Never its
                    content.
                  </span>
                </label>
              ),
            },
          ]}
        />
      </Card>

      <Card title={`Tasks (${data.tasks.length})`}>
        {data.tasks.length === 0 ? (
          <p className="muted">No tasks yet. Plan the work to create them.</p>
        ) : (
          <ol className="tasks">
            {data.tasks.map((task) => (
              <li key={task.id} className="task">
                <div className="row row--between">
                  <strong>
                    {task.ordinal + 1}. {task.title}
                  </strong>
                  <TaskStateBadge state={task.state} />
                </div>
                {task.instructions && <p className="muted">{task.instructions}</p>}
                {task.acceptance_criteria.length > 0 && (
                  <ul className="muted">
                    {task.acceptance_criteria.map((criterion) => (
                      <li key={criterion}>{criterion}</li>
                    ))}
                  </ul>
                )}
                {task.attempt > 1 && (
                  <p className="muted">
                    Attempt {task.attempt} of {task.max_attempts}
                  </p>
                )}
                {task.failure_reason && (
                  <Notice tone="caution" title="What needs to change">
                    {task.failure_reason}
                  </Notice>
                )}
                {task.output && (
                  <details>
                    <summary>Output</summary>
                    <Markdown source={task.output} />
                  </details>
                )}
                {task.verification_notes && (
                  <details>
                    <summary>Verification</summary>
                    <Markdown source={task.verification_notes} />
                  </details>
                )}
              </li>
            ))}
          </ol>
        )}
      </Card>

      {data.artifacts.length > 0 && (
        <Card title="Deliverables">
          <ul className="stack">
            {data.artifacts.map((artifact) => (
              <li key={artifact.id}>
                <strong>{artifact.name}</strong>{' '}
                <span className="muted">
                  {artifact.byte_size} bytes · {artifact.path}
                </span>
              </li>
            ))}
          </ul>
        </Card>
      )}

      {report && (
        <Card
          title="Completion report"
          actions={
            <Button
              size="sm"
              onClick={() => {
                const blob = new Blob([report], { type: 'text/markdown' });
                const url = URL.createObjectURL(blob);
                const anchor = document.createElement('a');
                anchor.href = url;
                anchor.download = `${data.title.replace(/\W+/g, '-').toLowerCase()}-report.md`;
                anchor.click();
                URL.revokeObjectURL(url);
              }}
            >
              Download
            </Button>
          }
        >
          <Markdown source={report} />
        </Card>
      )}
    </div>
  );
}
