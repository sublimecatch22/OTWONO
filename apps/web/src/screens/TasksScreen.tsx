/** Every task across every project, with the ones waiting on you first. */

import { useQuery } from '@tanstack/react-query';
import { useNavigate } from 'react-router-dom';

import { api } from '../api/client';
import type { ProjectDetail, ProjectSummary, Task } from '../api/types';
import { Button, Card, EmptyState, Notice, Spinner, TimeAgo } from '../components/primitives';
import { TaskStateBadge } from '../components/StateBadge';

interface TaskRow {
  task: Task;
  projectTitle: string;
}

export function TasksScreen() {
  const navigate = useNavigate();

  const projects = useQuery({
    queryKey: ['projects'],
    queryFn: () => api.get<ProjectSummary[]>('/api/projects'),
  });

  const details = useQuery({
    queryKey: ['projects', 'all-tasks', (projects.data ?? []).map((p) => p.id).join(',')],
    enabled: Boolean(projects.data?.length),
    queryFn: async () => {
      const loaded = await Promise.all(
        (projects.data ?? []).map((project) =>
          api.get<ProjectDetail>(`/api/projects/${project.id}`),
        ),
      );
      const rows: TaskRow[] = [];
      for (const project of loaded) {
        for (const task of project.tasks) {
          rows.push({ task, projectTitle: project.title });
        }
      }
      return rows;
    },
  });

  const rows = details.data ?? [];
  const waiting = rows.filter((row) => row.task.state === 'awaiting_approval');
  const active = rows.filter((row) =>
    ['ready', 'running', 'verifying', 'blocked'].includes(row.task.state),
  );
  const finished = rows.filter((row) =>
    ['completed', 'failed', 'cancelled'].includes(row.task.state),
  );

  if (projects.isLoading || details.isLoading) {
    return (
      <div className="screen">
        <Spinner label="Loading tasks" />
      </div>
    );
  }

  if (rows.length === 0) {
    return (
      <div className="screen">
        <EmptyState
          title="No tasks yet"
          description="Tasks appear here once a project has been planned."
          action={
            <Button variant="primary" onClick={() => navigate('/projects')}>
              Go to projects
            </Button>
          }
        />
      </div>
    );
  }

  const section = (title: string, items: TaskRow[]) =>
    items.length > 0 && (
      <Card title={`${title} (${items.length})`}>
        <ul className="stack">
          {items.map(({ task, projectTitle }) => (
            <li key={task.id}>
              <button
                type="button"
                className="listbutton"
                onClick={() => navigate(`/projects/${task.project_id}`)}
              >
                <span className="row row--between">
                  <strong>{task.title}</strong>
                  <TaskStateBadge state={task.state} />
                </span>
                <span className="muted">
                  {projectTitle} · updated <TimeAgo value={task.updated_at} />
                </span>
                {task.failure_reason && (
                  <span className="muted">Needs: {task.failure_reason}</span>
                )}
              </button>
            </li>
          ))}
        </ul>
      </Card>
    );

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Tasks</h1>
          <p className="screen__lede">Everything in flight, across every project.</p>
        </div>
      </header>

      {waiting.length > 0 && (
        <Notice tone="caution" title="Waiting for you">
          {waiting.length} task{waiting.length === 1 ? '' : 's'} cannot go further until you decide.
        </Notice>
      )}

      {section('Waiting for you', waiting)}
      {section('In progress', active)}
      {section('Finished', finished)}
    </div>
  );
}
