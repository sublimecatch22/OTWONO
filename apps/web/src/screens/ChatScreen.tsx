/**
 * The chat workspace — the default home screen.
 *
 * Streaming, stop, retry, edit-and-resend, model and agent selection,
 * knowledge sources, citations, and a run drawer showing what happened.
 */

import { useEffect, useMemo, useRef, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useNavigate, useParams } from 'react-router-dom';

import { api, streamEvents, type Citation, type StreamEvent } from '../api/client';
import type {
  Agent,
  ConnectionsResponse,
  ConversationDetail,
  Message,
  SourcesResponse,
} from '../api/types';
import { Markdown } from '../components/Markdown';
import { Badge, Button, EmptyState, Notice, Spinner, TimeAgo } from '../components/primitives';
import { useUi } from '../state/ui';

interface RunEvent {
  at: string;
  label: string;
  detail: string;
  tone: 'info' | 'caution' | 'negative' | 'positive';
}

export function ChatScreen() {
  const { conversationId } = useParams<{ conversationId?: string }>();
  const navigate = useNavigate();
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);

  const [draft, setDraft] = useState('');
  const [streamingText, setStreamingText] = useState('');
  const [streamCitations, setStreamCitations] = useState<Citation[]>([]);
  const [runEvents, setRunEvents] = useState<RunEvent[]>([]);
  const [runDrawerOpen, setRunDrawerOpen] = useState(false);
  const [streamError, setStreamError] = useState<{ message: string; retryable: boolean } | null>(
    null,
  );
  const [editing, setEditing] = useState<{ id: string; text: string } | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const transcriptEnd = useRef<HTMLDivElement>(null);

  const connections = useQuery({
    queryKey: ['connections'],
    queryFn: () => api.get<ConnectionsResponse>('/api/connections'),
  });
  const agents = useQuery({
    queryKey: ['agents'],
    queryFn: () => api.get<Agent[]>('/api/agents'),
  });
  const sources = useQuery({
    queryKey: ['knowledge', 'sources'],
    queryFn: () => api.get<SourcesResponse>('/api/knowledge/sources'),
  });

  const conversation = useQuery({
    queryKey: ['conversation', conversationId],
    queryFn: () => api.get<ConversationDetail>(`/api/conversations/${conversationId}`),
    enabled: Boolean(conversationId),
  });

  const createConversation = useMutation({
    mutationFn: () => api.post<{ id: string }>('/api/conversations', {}),
    onSuccess: (created) => {
      client.invalidateQueries({ queryKey: ['conversations', 'sidebar'] });
      navigate(`/chat/${created.id}`);
    },
  });

  const updateConversation = useMutation({
    mutationFn: (patch: Record<string, unknown>) =>
      api.put(`/api/conversations/${conversationId}`, patch),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['conversation', conversationId] });
      client.invalidateQueries({ queryKey: ['conversations', 'sidebar'] });
    },
  });

  const messages = conversation.data?.messages ?? [];
  const streaming = abortRef.current !== null;

  useEffect(() => {
    transcriptEnd.current?.scrollIntoView({ behavior: 'smooth', block: 'end' });
  }, [messages.length, streamingText]);

  const selectedSources = conversation.data?.knowledge_source_ids ?? [];
  const availableModels = useMemo(() => {
    const connection = connections.data?.connections.find(
      (candidate) => candidate.id === conversation.data?.provider_connection_id,
    );
    return connection?.default_model ? [connection.default_model] : [];
  }, [connections.data, conversation.data?.provider_connection_id]);

  async function send(text: string) {
    if (!conversationId || !text.trim()) return;
    setStreamError(null);
    setStreamingText('');
    setStreamCitations([]);
    setRunEvents([]);

    const controller = new AbortController();
    abortRef.current = controller;

    const note = (event: RunEvent) => setRunEvents((prev) => [...prev, event]);

    try {
      await streamEvents(
        `/api/conversations/${conversationId}/messages`,
        { message: text },
        (event: StreamEvent) => {
          const at = new Date().toLocaleTimeString();
          switch (event.type) {
            case 'start':
              note({
                at,
                label: 'Started',
                detail: `${event.provider} · ${event.model}`,
                tone: 'info',
              });
              break;
            case 'delta':
              setStreamingText((prev) => prev + event.text);
              break;
            case 'citations':
              setStreamCitations(event.citations);
              note({
                at,
                label: 'Knowledge used',
                detail: `${event.citations.length} passage(s) from your files`,
                tone: 'info',
              });
              break;
            case 'tool_call':
              note({
                at,
                label: event.tool,
                detail: event.summary,
                tone: event.status === 'warning' ? 'caution' : 'info',
              });
              if (event.status === 'warning') setRunDrawerOpen(true);
              break;
            case 'approval_required':
              note({ at, label: 'Approval needed', detail: event.summary, tone: 'caution' });
              setRunDrawerOpen(true);
              break;
            case 'error':
              setStreamError({ message: event.message, retryable: event.retryable });
              note({ at, label: 'Failed', detail: event.message, tone: 'negative' });
              setRunDrawerOpen(true);
              break;
            case 'done':
              note({
                at,
                label: 'Finished',
                detail:
                  event.finish_reason === 'stop'
                    ? 'Complete'
                    : `Stopped: ${event.finish_reason}`,
                tone: event.finish_reason === 'stop' ? 'positive' : 'caution',
              });
              break;
          }
        },
        controller.signal,
      );
    } catch (error) {
      if (!controller.signal.aborted) {
        const message = error instanceof Error ? error.message : String(error);
        setStreamError({ message, retryable: true });
      }
    } finally {
      abortRef.current = null;
      setStreamingText('');
      setStreamCitations([]);
      await conversation.refetch();
      client.invalidateQueries({ queryKey: ['conversations', 'sidebar'] });
    }
  }

  function stop() {
    abortRef.current?.abort();
    abortRef.current = null;
    toast({ tone: 'info', body: 'Generation stopped. What arrived so far was kept.' });
  }

  async function retryFrom(message: Message) {
    // Remove the assistant turn and everything after it, then resend the user
    // message that produced it.
    const index = messages.findIndex((candidate) => candidate.id === message.id);
    const previousUser = [...messages.slice(0, index)].reverse().find((m) => m.role === 'user');
    if (!previousUser) return;
    await api.post(`/api/conversations/${conversationId}/truncate`, {
      from_message_id: previousUser.id,
    });
    await conversation.refetch();
    await send(previousUser.content);
  }

  async function resendEdited() {
    if (!editing) return;
    await api.post(`/api/conversations/${conversationId}/truncate`, {
      from_message_id: editing.id,
    });
    const text = editing.text;
    setEditing(null);
    await conversation.refetch();
    await send(text);
  }

  if (!conversationId) {
    return (
      <div className="screen screen--centered">
        <EmptyState
          title="Start a conversation"
          description="Ask a question, or point OTWONO at your own files first from the Knowledge screen."
          action={
            <Button
              variant="primary"
              busy={createConversation.isPending}
              onClick={() => createConversation.mutate()}
            >
              New chat
            </Button>
          }
        />
      </div>
    );
  }

  const notReady = connections.data && !connections.data.ready_for_chat;

  return (
    <div className="chat">
      <header className="chat__head">
        <div className="chat__title">
          <h1>{conversation.data?.title ?? 'Chat'}</h1>
          {conversation.data && (
            <span className="chat__meta">
              Updated <TimeAgo value={conversation.data.updated_at} />
            </span>
          )}
        </div>

        <div className="chat__controls">
          <label className="control">
            <span className="control__label">Agent</span>
            <select
              className="select"
              value={conversation.data?.agent_id ?? ''}
              onChange={(event) =>
                updateConversation.mutate({ agent_id: event.target.value || null })
              }
            >
              <option value="">No agent — plain chat</option>
              {(agents.data ?? []).map((agent) => (
                <option key={agent.id} value={agent.id}>
                  {agent.name}
                </option>
              ))}
            </select>
          </label>

          <label className="control">
            <span className="control__label">Connection</span>
            <select
              className="select"
              value={conversation.data?.provider_connection_id ?? ''}
              onChange={(event) =>
                updateConversation.mutate({
                  provider_connection_id: event.target.value || null,
                })
              }
            >
              <option value="">Default</option>
              {(connections.data?.connections ?? [])
                .filter((connection) => connection.enabled)
                .map((connection) => (
                  <option key={connection.id} value={connection.id}>
                    {connection.label}
                  </option>
                ))}
            </select>
          </label>

          {availableModels.length > 0 && (
            <label className="control">
              <span className="control__label">Model</span>
              <select
                className="select"
                value={conversation.data?.model ?? ''}
                onChange={(event) =>
                  updateConversation.mutate({ model: event.target.value || null })
                }
              >
                <option value="">Connection default</option>
                {availableModels.map((model) => (
                  <option key={model} value={model}>
                    {model}
                  </option>
                ))}
              </select>
            </label>
          )}

          <Button
            size="sm"
            onClick={() => setRunDrawerOpen((open) => !open)}
            aria-expanded={runDrawerOpen}
          >
            Run details{runEvents.length > 0 ? ` (${runEvents.length})` : ''}
          </Button>
        </div>

        {(sources.data?.sources ?? []).length > 0 && (
          <fieldset className="chat__sources">
            <legend>Knowledge for this chat</legend>
            {(sources.data?.sources ?? [])
              .filter((source) => source.authorised)
              .map((source) => (
                <label className="chip" key={source.id}>
                  <input
                    type="checkbox"
                    checked={selectedSources.includes(source.id)}
                    onChange={(event) => {
                      const next = event.target.checked
                        ? [...selectedSources, source.id]
                        : selectedSources.filter((id) => id !== source.id);
                      updateConversation.mutate({ knowledge_source_ids: next });
                    }}
                  />
                  <span>{source.label}</span>
                  {source.embedding_is_fallback && (
                    <Badge tone="caution">word match only</Badge>
                  )}
                </label>
              ))}
          </fieldset>
        )}
      </header>

      {notReady && (
        <div className="chat__notice">
          <Notice
            tone="caution"
            title="No model is connected"
            action={
              <Button variant="primary" size="sm" onClick={() => navigate('/connections')}>
                Set up a connection
              </Button>
            }
          >
            {connections.data?.guidance}
          </Notice>
        </div>
      )}

      <div className="chat__transcript" role="log" aria-live="polite" aria-label="Conversation">
        <div className="chat__transcriptInner">
          {conversation.isLoading && <Spinner label="Loading the conversation" />}

          {messages.length === 0 && !conversation.isLoading && (
            <EmptyState
              title="Nothing here yet"
              description="Type a message below. Anything OTWONO reads from your files will be cited."
            />
          )}

          {messages.map((message) => (
            <MessageBubble
              key={message.id}
              message={message}
              onRetry={() => retryFrom(message)}
              onEdit={() => setEditing({ id: message.id, text: message.content })}
              editing={editing?.id === message.id ? editing.text : null}
              onEditChange={(text) => setEditing({ id: message.id, text })}
              onEditCancel={() => setEditing(null)}
              onEditSend={resendEdited}
            />
          ))}

          {streamingText && (
            <article className="message message--assistant message--streaming">
              <header className="message__head">
                <span className="message__who">OTWONO</span>
                <Spinner label="Generating a reply" />
              </header>
              <Markdown source={streamingText} />
              {streamCitations.length > 0 && <Citations citations={streamCitations} />}
            </article>
          )}

          {streamError && (
            <Notice
              tone="negative"
              title="That did not work"
              action={
                streamError.retryable ? (
                  <Button
                    size="sm"
                    onClick={() => {
                      const lastUser = [...messages].reverse().find((m) => m.role === 'user');
                      if (lastUser) void retryFrom(messages[messages.length - 1] ?? lastUser);
                    }}
                  >
                    Try again
                  </Button>
                ) : undefined
              }
            >
              {streamError.message}
            </Notice>
          )}

          <div ref={transcriptEnd} />
        </div>
      </div>

      {runDrawerOpen && (
        <aside className="rundrawer" aria-label="Run details">
          <header className="rundrawer__head">
            <h2>Run details</h2>
            <button
              type="button"
              className="iconbutton iconbutton--small"
              onClick={() => setRunDrawerOpen(false)}
            >
              <span aria-hidden="true">×</span>
              <span className="visually-hidden">Close run details</span>
            </button>
          </header>
          {runEvents.length === 0 ? (
            <p className="rundrawer__empty">
              Nothing has run yet. Send a message and each step appears here.
            </p>
          ) : (
            <ol className="rundrawer__list">
              {runEvents.map((event, index) => (
                <li key={index} className={`rundrawer__item rundrawer__item--${event.tone}`}>
                  <span className="rundrawer__time">{event.at}</span>
                  <strong>{event.label}</strong>
                  <span>{event.detail}</span>
                </li>
              ))}
            </ol>
          )}
        </aside>
      )}

      <form
        className="composer"
        onSubmit={(event) => {
          event.preventDefault();
          const text = draft;
          setDraft('');
          void send(text);
        }}
      >
        <label className="visually-hidden" htmlFor="composer-input">
          Your message
        </label>
        <textarea
          id="composer-input"
          className="composer__input"
          rows={3}
          value={draft}
          placeholder="Ask something, or describe what you want done…"
          disabled={streaming}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
              event.preventDefault();
              const text = draft;
              setDraft('');
              void send(text);
            }
          }}
        />
        <div className="composer__actions">
          <span className="composer__hint">Ctrl or ⌘ + Enter to send</span>
          {streaming ? (
            <Button variant="danger" onClick={stop}>
              Stop
            </Button>
          ) : (
            <Button type="submit" variant="primary" disabled={!draft.trim()}>
              Send
            </Button>
          )}
        </div>
      </form>
    </div>
  );
}

function MessageBubble({
  message,
  onRetry,
  onEdit,
  editing,
  onEditChange,
  onEditCancel,
  onEditSend,
}: {
  message: Message;
  onRetry: () => void;
  onEdit: () => void;
  editing: string | null;
  onEditChange: (text: string) => void;
  onEditCancel: () => void;
  onEditSend: () => void;
}) {
  const isUser = message.role === 'user';

  if (editing !== null) {
    return (
      <article className="message message--user message--editing">
        <label className="visually-hidden" htmlFor={`edit-${message.id}`}>
          Edit your message
        </label>
        <textarea
          id={`edit-${message.id}`}
          className="composer__input"
          rows={3}
          value={editing}
          onChange={(event) => onEditChange(event.target.value)}
        />
        <div className="message__actions">
          <Button size="sm" onClick={onEditCancel}>
            Cancel
          </Button>
          <Button size="sm" variant="primary" onClick={onEditSend}>
            Send again
          </Button>
        </div>
      </article>
    );
  }

  return (
    <article className={`message message--${isUser ? 'user' : 'assistant'}`}>
      <header className="message__head">
        <span className="message__who">{isUser ? 'You' : 'OTWONO'}</span>
        {message.model && <span className="message__model">{message.model}</span>}
        <TimeAgo value={message.created_at} />
      </header>

      <Markdown source={message.content} />

      {message.stopped_reason && (
        <p className="message__stopped">
          {message.stopped_reason === 'stopped_by_user'
            ? 'You stopped this reply before it finished.'
            : `This reply did not finish: ${message.stopped_reason}`}
        </p>
      )}

      {message.citations.length > 0 && <Citations citations={message.citations} />}

      <div className="message__actions">
        <Button
          size="sm"
          variant="ghost"
          onClick={() => navigator.clipboard?.writeText(message.content)}
        >
          Copy
        </Button>
        {isUser ? (
          <Button size="sm" variant="ghost" onClick={onEdit}>
            Edit and resend
          </Button>
        ) : (
          <Button size="sm" variant="ghost" onClick={onRetry}>
            Retry
          </Button>
        )}
      </div>
    </article>
  );
}

export function Citations({ citations }: { citations: Citation[] }) {
  return (
    <details className="citations" open>
      <summary>
        {citations.length} source{citations.length === 1 ? '' : 's'} from your files
      </summary>
      <ol>
        {citations.map((citation, index) => (
          <li key={`${citation.document_id}-${citation.chunk_index}-${index}`}>
            <strong>
              {citation.file_name}
              {citation.locator ? ` (${citation.locator})` : ''}
            </strong>
            <p className="citations__excerpt">{citation.excerpt}</p>
            <span className="citations__path">{citation.file_path}</span>
          </li>
        ))}
      </ol>
    </details>
  );
}
