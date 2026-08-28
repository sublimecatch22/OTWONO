/** Appearance, permissions, the account link, privacy and data. */

import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import type { Preferences } from '@otwono/ui';
import { api, ApiError } from '../api/client';
import type {
  AccountStatus,
  Grant,
  PairingCode,
  PermissionsResponse,
  SyncResult,
} from '../api/types';
import { Badge, Button, Card, DetailList, Field, Notice, Spinner } from '../components/primitives';
import { ALL_TABS } from '../components/AppShell';
import { usePreferences, useResetPreferences, useSavePreferences } from '../state/preferences';
import { useSystemStatus } from '../state/system';
import { useUi } from '../state/ui';

export function SettingsScreen() {
  const preferences = usePreferences();
  const status = useSystemStatus();

  if (preferences.isLoading || status.isLoading) {
    return (
      <div className="screen">
        <Spinner label="Loading settings" />
      </div>
    );
  }

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Settings</h1>
          <p className="screen__lede">
            How OTWONO looks, what it is allowed to do, and where your data lives.
          </p>
        </div>
      </header>

      <AppearanceSettings />
      <PermissionSettings />
      <AccountSettings />
      <DataSettings />
    </div>
  );
}

function AppearanceSettings() {
  const preferences = usePreferences();
  const save = useSavePreferences();
  const reset = useResetPreferences();
  const toast = useUi((state) => state.toast);

  if (!preferences.data) return null;
  const { preferences: current, options } = preferences.data;

  const update = (patch: Partial<Preferences>) => save.mutate({ ...current, ...patch });

  return (
    <Card
      title="Appearance"
      description="Changes apply immediately and are kept between launches."
      actions={
        <>
          <Button
            size="sm"
            onClick={async () => {
              const exported = await api.get('/api/settings/export');
              const blob = new Blob([JSON.stringify(exported, null, 2)], {
                type: 'application/json',
              });
              const url = URL.createObjectURL(blob);
              const anchor = document.createElement('a');
              anchor.href = url;
              anchor.download = 'otwono-settings.json';
              anchor.click();
              URL.revokeObjectURL(url);
            }}
          >
            Export
          </Button>
          <label className="btn btn--secondary btn--sm">
            <span>Import</span>
            <input
              type="file"
              accept="application/json,.json"
              className="visually-hidden"
              onChange={async (event) => {
                const file = event.target.files?.[0];
                if (!file) return;
                try {
                  await api.post('/api/settings/import', JSON.parse(await file.text()));
                  await preferences.refetch();
                  toast({ tone: 'positive', body: 'Settings imported.' });
                } catch (error) {
                  toast({
                    tone: 'negative',
                    body:
                      error instanceof ApiError ? error.message : 'That file could not be read.',
                  });
                }
                event.target.value = '';
              }}
            />
          </label>
          <Button size="sm" variant="danger" busy={reset.isPending} onClick={() => reset.mutate()}>
            Reset to default
          </Button>
        </>
      }
    >
      <div className="grid grid--three">
        <Field label="Theme">
          {({ id }) => (
            <select
              id={id}
              className="select"
              value={current.theme}
              onChange={(event) => update({ theme: event.target.value as Preferences['theme'] })}
            >
              {options.themes.map((theme) => (
                <option key={theme} value={theme}>
                  {theme.replace('-', ' ')}
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field label="Accent">
          {({ id }) => (
            <select
              id={id}
              className="select"
              value={current.accent}
              onChange={(event) => update({ accent: event.target.value })}
            >
              {options.accents.map((accent) => (
                <option key={accent} value={accent}>
                  {accent}
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field label="Background">
          {({ id }) => (
            <select
              id={id}
              className="select"
              value={current.background}
              onChange={(event) => update({ background: event.target.value })}
            >
              {options.backgrounds.map((background) => (
                <option key={background} value={background}>
                  {background}
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field label="Font">
          {({ id }) => (
            <select
              id={id}
              className="select"
              value={current.font_family}
              onChange={(event) =>
                update({ font_family: event.target.value as Preferences['font_family'] })
              }
            >
              {options.fonts.map((font) => (
                <option key={font} value={font}>
                  {font}
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field label={`Text size (${current.font_size_px}px)`}>
          {({ id }) => (
            <input
              id={id}
              className="range"
              type="range"
              min={options.font_size_range[0]}
              max={options.font_size_range[1]}
              value={current.font_size_px}
              onChange={(event) => update({ font_size_px: Number(event.target.value) })}
            />
          )}
        </Field>

        <Field label="Density">
          {({ id }) => (
            <select
              id={id}
              className="select"
              value={current.density}
              onChange={(event) =>
                update({ density: event.target.value as Preferences['density'] })
              }
            >
              {options.densities.map((density) => (
                <option key={density} value={density}>
                  {density}
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field label="Sidebar position">
          {({ id }) => (
            <select
              id={id}
              className="select"
              value={current.sidebar_position}
              onChange={(event) =>
                update({
                  sidebar_position: event.target.value as Preferences['sidebar_position'],
                })
              }
            >
              <option value="left">Left</option>
              <option value="right">Right</option>
            </select>
          )}
        </Field>

        <Field label={`Sidebar width (${current.sidebar_width_px}px)`}>
          {({ id }) => (
            <input
              id={id}
              className="range"
              type="range"
              min={options.sidebar_width_range[0]}
              max={options.sidebar_width_range[1]}
              value={current.sidebar_width_px}
              onChange={(event) => update({ sidebar_width_px: Number(event.target.value) })}
            />
          )}
        </Field>

        <Field label={`Chat width (${current.chat_max_width_px}px)`}>
          {({ id }) => (
            <input
              id={id}
              className="range"
              type="range"
              min={options.chat_width_range[0]}
              max={options.chat_width_range[1]}
              value={current.chat_max_width_px}
              onChange={(event) => update({ chat_max_width_px: Number(event.target.value) })}
            />
          )}
        </Field>
      </div>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={current.reduced_motion}
          onChange={(event) => update({ reduced_motion: event.target.checked })}
        />
        <span>Reduce motion</span>
      </label>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={current.sidebar_collapsed}
          onChange={(event) => update({ sidebar_collapsed: event.target.checked })}
        />
        <span>Start with the sidebar collapsed</span>
      </label>

      <fieldset className="fieldset">
        <legend>Visible tabs</legend>
        <p className="muted">Chat and Settings always stay, so you cannot hide the way back.</p>
        {ALL_TABS.map((tab) => {
          const required = options.required_tabs.includes(tab.key);
          return (
            <label className="checkbox" key={tab.key}>
              <input
                type="checkbox"
                disabled={required}
                checked={current.visible_tabs.includes(tab.key)}
                onChange={(event) => {
                  const next = event.target.checked
                    ? [...current.visible_tabs, tab.key]
                    : current.visible_tabs.filter((key) => key !== tab.key);
                  update({ visible_tabs: next });
                }}
              />
              <span>
                {tab.label}
                {required && (
                  <>
                    {' '}
                    <Badge tone="neutral">always shown</Badge>
                  </>
                )}
              </span>
            </label>
          );
        })}
      </fieldset>
    </Card>
  );
}

function PermissionSettings() {
  const client = useQueryClient();
  const permissions = useQuery({
    queryKey: ['permissions'],
    queryFn: () => api.get<PermissionsResponse>('/api/permissions'),
  });

  const revoke = useMutation({
    mutationFn: (id: string) => api.post(`/api/permissions/grants/${id}/revoke`),
    onSuccess: () => client.invalidateQueries({ queryKey: ['permissions'] }),
  });
  const revokeAll = useMutation({
    mutationFn: () => api.post<{ message: string }>('/api/permissions/revoke-all'),
    onSuccess: () => client.invalidateQueries({ queryKey: ['permissions'] }),
  });
  const resolve = useMutation({
    mutationFn: (input: { id: string; decision: string }) =>
      api.post(`/api/permissions/requests/${input.id}/resolve`, { decision: input.decision }),
    onSuccess: () => client.invalidateQueries({ queryKey: ['permissions'] }),
  });

  return (
    <Card
      id="permissions"
      title="Permissions"
      description="OTWONO refuses anything it has not been given permission for. Nothing is granted until you say so."
      actions={
        (permissions.data?.grants.length ?? 0) > 0 && (
          <Button
            size="sm"
            variant="danger"
            busy={revokeAll.isPending}
            onClick={() => revokeAll.mutate()}
          >
            Revoke everything
          </Button>
        )
      }
    >
      {permissions.isLoading && <Spinner label="Loading permissions" />}

      {(permissions.data?.open_requests.length ?? 0) > 0 && (
        <Notice tone="caution" title="Waiting for your answer">
          <ul className="stack">
            {(permissions.data?.open_requests ?? []).map((request) => (
              <li key={request.id} className="row">
                <span>{request.summary}</span>
                <span className="row row--tight">
                  <Button
                    size="sm"
                    onClick={() => resolve.mutate({ id: request.id, decision: 'allow_once' })}
                  >
                    Allow once
                  </Button>
                  <Button
                    size="sm"
                    variant="primary"
                    onClick={() => resolve.mutate({ id: request.id, decision: 'allow' })}
                  >
                    Always allow
                  </Button>
                  <Button
                    size="sm"
                    variant="danger"
                    onClick={() => resolve.mutate({ id: request.id, decision: 'deny' })}
                  >
                    Refuse
                  </Button>
                </span>
              </li>
            ))}
          </ul>
        </Notice>
      )}

      {permissions.data?.grants.length === 0 ? (
        <p className="muted">
          Nothing is granted. Agents will ask the first time they need something.
        </p>
      ) : (
        <ul className="stack">
          {(permissions.data?.grants ?? []).map((grant: Grant) => (
            <li key={grant.id} className="row">
              <div>
                <strong>{grant.capability.replace(/_/g, ' ')}</strong>{' '}
                <Badge tone={grant.decision === 'deny' ? 'negative' : 'positive'}>
                  {grant.decision.replace('_', ' ')}
                </Badge>
                <p className="muted">
                  {grant.scopes.length === 0
                    ? 'Everywhere'
                    : grant.scopes
                        .map((scope) =>
                          'path' in scope ? scope.path : 'host' in scope ? scope.host : scope.type,
                        )
                        .join(', ')}
                  {grant.expires_at && ` · expires ${new Date(grant.expires_at).toLocaleString()}`}
                </p>
              </div>
              <Button size="sm" variant="danger" onClick={() => revoke.mutate(grant.id)}>
                Revoke
              </Button>
            </li>
          ))}
        </ul>
      )}

      <details>
        <summary>What each permission means</summary>
        <ul className="stack">
          {(permissions.data?.capabilities ?? []).map((capability) => (
            <li key={capability.capability}>
              <strong>{capability.capability.replace(/_/g, ' ')}</strong> —{' '}
              {capability.human_request}
              {capability.leaves_device && (
                <>
                  {' '}
                  <Badge tone="caution">can leave this device</Badge>
                </>
              )}
            </li>
          ))}
        </ul>
      </details>
    </Card>
  );
}

function AccountSettings() {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [code, setCode] = useState<PairingCode | null>(null);

  const account = useQuery({
    queryKey: ['account'],
    queryFn: () => api.get<AccountStatus>('/api/account'),
  });

  const pair = useMutation({
    mutationFn: () =>
      api.post<PairingCode>('/api/account/pairing-code', {
        scopes: ['profile.read', 'profile.write', 'projects.read', 'marketplace.read'],
      }),
    onSuccess: setCode,
  });

  const unlink = useMutation({
    mutationFn: () => api.post('/api/account/unlink'),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['account'] });
      toast({ tone: 'positive', body: 'Account unlinked and its token deleted.' });
    },
  });

  const [sent, setSent] = useState<SyncResult | null>(null);
  const sync = useMutation({
    mutationFn: () => api.post<SyncResult>('/api/account/sync'),
    onSuccess: (result) => {
      setSent(result);
      toast({
        tone: 'positive',
        body: `Sent the metadata of ${result.synchronised} project(s).`,
      });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  return (
    <Card
      title="OTWONO account"
      description="Optional. Everything works without one."
      actions={
        account.data?.linked ? (
          <>
            <Button size="sm" busy={sync.isPending} onClick={() => sync.mutate()}>
              Send project metadata
            </Button>
            <Button size="sm" variant="danger" onClick={() => unlink.mutate()}>
              Unlink
            </Button>
          </>
        ) : (
          <Button size="sm" variant="primary" busy={pair.isPending} onClick={() => pair.mutate()}>
            Show a pairing code
          </Button>
        )
      }
    >
      <Notice tone="info">{account.data?.privacy_notice}</Notice>

      {code && (
        <div className="pairing">
          <p>{code.instructions}</p>
          <output className="pairing__code">{code.code}</output>
          <p className="muted">
            Scopes: {code.scopes.join(', ')} · expires{' '}
            {new Date(code.expires_at).toLocaleTimeString()}
          </p>
        </div>
      )}

      {account.data?.linked && account.data.link && (
        <DetailList
          items={[
            { label: 'Relay', value: account.data.link.relay_base_url },
            { label: 'Account', value: account.data.link.account_email ?? '—' },
            { label: 'Scopes', value: account.data.link.scopes.join(', ') || 'None' },
          ]}
        />
      )}

      {sent && (
        <Notice tone="positive" title="What was sent">
          <p>{sent.what_was_sent}</p>
          {sent.titles.length === 0 ? (
            <p>
              No project is marked for synchronisation, so nothing left this machine. Tick the box
              on a project to include it.
            </p>
          ) : (
            <ul>
              {sent.titles.map((title) => (
                <li key={title}>{title}</li>
              ))}
            </ul>
          )}
        </Notice>
      )}
    </Card>
  );
}

function DataSettings() {
  const status = useSystemStatus();
  const toast = useUi((state) => state.toast);

  const backup = useMutation({
    mutationFn: () => api.post<{ path: string; message: string }>('/api/system/backup'),
    onSuccess: (result) => toast({ tone: 'positive', title: 'Backup saved', body: result.message }),
  });

  return (
    <Card
      title="Your data"
      description="Everything OTWONO knows lives in one folder on this machine."
      actions={
        <Button size="sm" busy={backup.isPending} onClick={() => backup.mutate()}>
          Back up now
        </Button>
      }
    >
      {status.data && (
        <DetailList
          items={[
            { label: 'Version', value: status.data.version },
            { label: 'Database version', value: String(status.data.schema_version) },
            { label: 'Data folder', value: <code>{status.data.data_directory}</code> },
            {
              label: 'Secret storage',
              value: (
                <>
                  <Badge
                    tone={
                      status.data.secret_backend === 'operating_system' ? 'positive' : 'caution'
                    }
                  >
                    {status.data.secret_backend.replace(/_/g, ' ')}
                  </Badge>
                  <p className="muted">{status.data.secret_backend_detail}</p>
                </>
              ),
            },
            {
              label: 'Analytics',
              value: status.data.telemetry_opt_in
                ? 'On'
                : 'Off. OTWONO collects nothing about how you use it.',
            },
          ]}
        />
      )}
    </Card>
  );
}
