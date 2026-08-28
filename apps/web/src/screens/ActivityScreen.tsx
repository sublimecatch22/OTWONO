/** The audit log. */

import { useState } from 'react';
import { useQuery } from '@tanstack/react-query';

import { api } from '../api/client';
import type { ActivityEntry } from '../api/types';
import { Badge, Button, Card, EmptyState, Field, Spinner } from '../components/primitives';

export function ActivityScreen() {
  const [actor, setActor] = useState('');
  const [prefix, setPrefix] = useState('');

  const log = useQuery({
    queryKey: ['activity', actor, prefix],
    queryFn: () => {
      const params = new URLSearchParams({ limit: '200' });
      if (actor) params.set('actor_type', actor);
      if (prefix) params.set('action_prefix', prefix);
      return api.get<{ entries: ActivityEntry[]; total: number }>(`/api/activity?${params}`);
    },
    refetchInterval: 10_000,
  });

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Activity</h1>
          <p className="screen__lede">
            Everything OTWONO did, with a timestamp and who asked for it. Values that could be a
            secret were removed before the entry was written, not when it is shown.
          </p>
        </div>
        <Button
          onClick={async () => {
            const params = new URLSearchParams({ limit: '1000' });
            if (actor) params.set('actor_type', actor);
            if (prefix) params.set('action_prefix', prefix);
            const report = await api.text(`/api/activity/export?${params}`);
            const blob = new Blob([report], { type: 'text/plain' });
            const url = URL.createObjectURL(blob);
            const anchor = document.createElement('a');
            anchor.href = url;
            anchor.download = 'otwono-activity.txt';
            anchor.click();
            URL.revokeObjectURL(url);
          }}
        >
          Export report
        </Button>
      </header>

      <Card>
        <div className="row row--tight">
          <Field label="Who">
            {({ id }) => (
              <select
                id={id}
                className="select"
                value={actor}
                onChange={(event) => setActor(event.target.value)}
              >
                <option value="">Everyone</option>
                <option value="user">You</option>
                <option value="agent">Agents</option>
                <option value="system">OTWONO</option>
                <option value="relay">A paired site</option>
              </select>
            )}
          </Field>
          <Field label="Action starts with">
            {({ id }) => (
              <input
                id={id}
                className="input"
                value={prefix}
                placeholder="tool."
                onChange={(event) => setPrefix(event.target.value)}
              />
            )}
          </Field>
        </div>
      </Card>

      {log.isLoading && <Spinner label="Loading the activity log" />}

      {log.data?.entries.length === 0 && (
        <EmptyState title="Nothing recorded yet" description="Activity appears here as you work." />
      )}

      {(log.data?.entries.length ?? 0) > 0 && (
        <Card title={`${log.data?.entries.length} of ${log.data?.total} entries`}>
          <ol className="log">
            {(log.data?.entries ?? []).map((entry) => (
              <li key={entry.id} className="log__row">
                <time dateTime={entry.created_at}>
                  {new Date(entry.created_at).toLocaleString()}
                </time>
                <Badge
                  tone={
                    entry.outcome === 'ok'
                      ? 'positive'
                      : entry.outcome === 'denied'
                        ? 'caution'
                        : entry.outcome === 'failed'
                          ? 'negative'
                          : 'info'
                  }
                >
                  {entry.outcome}
                </Badge>
                <code>{entry.action}</code>
                <span className="muted">
                  {entry.actor_name ?? entry.actor_type}
                </span>
                {Object.keys(entry.detail ?? {}).length > 0 && (
                  <details>
                    <summary>Detail</summary>
                    <pre className="output">{JSON.stringify(entry.detail, null, 2)}</pre>
                  </details>
                )}
              </li>
            ))}
          </ol>
        </Card>
      )}
    </div>
  );
}
