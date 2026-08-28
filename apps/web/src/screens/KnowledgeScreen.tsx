/** Local knowledge: authorising folders, indexing, searching and revoking. */

import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { api, ApiError } from '../api/client';
import type {
  BrowseResponse,
  IndexResponse,
  KnowledgeDocument,
  SearchResponse,
  SourcesResponse,
} from '../api/types';
import {
  Badge,
  Button,
  Card,
  EmptyState,
  Field,
  Notice,
  Spinner,
  TimeAgo,
} from '../components/primitives';
import { useUi } from '../state/ui';

export function KnowledgeScreen() {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [picking, setPicking] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<SearchResponse | null>(null);

  const sources = useQuery({
    queryKey: ['knowledge', 'sources'],
    queryFn: () => api.get<SourcesResponse>('/api/knowledge/sources'),
  });

  const authorise = useMutation({
    mutationFn: (path: string) => api.post('/api/knowledge/sources', { path }),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['knowledge', 'sources'] });
      setPicking(false);
      toast({ tone: 'positive', body: 'Folder authorised. Index it to make it searchable.' });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  const index = useMutation({
    mutationFn: (id: string) => api.post<IndexResponse>(`/api/knowledge/sources/${id}/index`),
    onSuccess: (report) => {
      client.invalidateQueries({ queryKey: ['knowledge'] });
      toast({
        tone: report.failed > 0 ? 'caution' : 'positive',
        title: 'Indexing finished',
        body: report.message,
      });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  const setAuthorisation = useMutation({
    mutationFn: (input: { id: string; authorised: boolean }) =>
      api.put(`/api/knowledge/sources/${input.id}/authorisation`, {
        authorised: input.authorised,
      }),
    onSuccess: (_result, input) => {
      client.invalidateQueries({ queryKey: ['knowledge'] });
      toast({
        tone: input.authorised ? 'positive' : 'caution',
        body: input.authorised
          ? 'Access restored. Index the folder again to make it searchable.'
          : 'Access revoked. Everything indexed from that folder was deleted straight away.',
      });
    },
  });

  const remove = useMutation({
    mutationFn: (id: string) => api.delete(`/api/knowledge/sources/${id}`),
    onSuccess: () => client.invalidateQueries({ queryKey: ['knowledge'] }),
  });

  const search = useMutation({
    mutationFn: (input: { query: string; source_ids: string[] }) =>
      api.post<SearchResponse>('/api/knowledge/search', input),
    onSuccess: setResults,
  });

  const authorisedSources = (sources.data?.sources ?? []).filter((source) => source.authorised);

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Knowledge</h1>
          <p className="screen__lede">
            Choose folders OTWONO may read. Nothing is uploaded: files are indexed on this machine,
            and revoking a folder deletes everything indexed from it immediately.
          </p>
        </div>
        <Button variant="primary" onClick={() => setPicking((open) => !open)}>
          {picking ? 'Close' : 'Authorise a folder'}
        </Button>
      </header>

      {sources.data?.retrieval_notice && (
        <Notice tone="caution" title="Search is matching words, not meaning">
          {sources.data.retrieval_notice}
        </Notice>
      )}

      {picking && <FolderPicker onChoose={(path) => authorise.mutate(path)} />}

      {sources.isLoading && <Spinner label="Loading your sources" />}

      {sources.data?.sources.length === 0 && !picking && (
        <EmptyState
          title="No folders authorised"
          description="OTWONO cannot read anything on this machine until you say which folders it may use."
          action={
            <Button variant="primary" onClick={() => setPicking(true)}>
              Authorise a folder
            </Button>
          }
        />
      )}

      {(sources.data?.sources ?? []).map((source) => (
        <Card
          key={source.id}
          title={source.label}
          description={source.root_path}
          actions={
            <>
              {source.authorised ? (
                <>
                  <Button
                    size="sm"
                    variant="primary"
                    busy={index.isPending}
                    onClick={() => index.mutate(source.id)}
                  >
                    Index now
                  </Button>
                  <Button
                    size="sm"
                    onClick={() =>
                      setAuthorisation.mutate({ id: source.id, authorised: false })
                    }
                  >
                    Revoke access
                  </Button>
                </>
              ) : (
                <Button
                  size="sm"
                  onClick={() => setAuthorisation.mutate({ id: source.id, authorised: true })}
                >
                  Restore access
                </Button>
              )}
              <Button size="sm" variant="danger" onClick={() => remove.mutate(source.id)}>
                Remove
              </Button>
            </>
          }
        >
          <div className="row row--wrap">
            {source.authorised ? (
              <Badge tone="positive">Authorised</Badge>
            ) : (
              <Badge tone="caution">Revoked</Badge>
            )}
            {!source.exists_on_disk && <Badge tone="negative">Folder is missing</Badge>}
            <Badge tone="neutral">{source.document_count} file(s)</Badge>
            <Badge tone="neutral">{source.chunk_count} passage(s)</Badge>
            {source.embedding_is_fallback ? (
              <Badge tone="caution">word match only</Badge>
            ) : (
              <Badge tone="info">{source.embedding_model}</Badge>
            )}
            {source.last_indexed_at && (
              <span className="muted">
                Indexed <TimeAgo value={source.last_indexed_at} />
              </span>
            )}
          </div>

          <p className="muted">{source.embedding_detail}</p>

          <Button
            size="sm"
            variant="ghost"
            aria-expanded={expanded === source.id}
            onClick={() => setExpanded(expanded === source.id ? null : source.id)}
          >
            {expanded === source.id ? 'Hide files' : 'Show files'}
          </Button>

          {expanded === source.id && <DocumentList sourceId={source.id} />}
        </Card>
      ))}

      {authorisedSources.length > 0 && (
        <Card
          title="Try a search"
          description="See exactly what OTWONO would retrieve, and from where."
        >
          <form
            className="row row--tight"
            onSubmit={(event) => {
              event.preventDefault();
              search.mutate({
                query,
                source_ids: authorisedSources.map((source) => source.id),
              });
            }}
          >
            <label className="visually-hidden" htmlFor="knowledge-search">
              Search your indexed files
            </label>
            <input
              id="knowledge-search"
              className="input"
              value={query}
              placeholder="What does the handbook say about leave?"
              onChange={(event) => setQuery(event.target.value)}
            />
            <Button type="submit" variant="primary" busy={search.isPending} disabled={!query.trim()}>
              Search
            </Button>
          </form>

          {results && (
            <div className="stack">
              {results.hits.length === 0 ? (
                <Notice tone="info">
                  Nothing matched. OTWONO would tell you that rather than answering from memory.
                </Notice>
              ) : (
                <ol className="results">
                  {results.hits.map((hit, position) => (
                    <li key={`${hit.file_path}-${position}`}>
                      <strong>
                        {hit.file_name}
                        {hit.chunk.locator ? ` (${hit.chunk.locator})` : ''}
                      </strong>
                      <span className="muted"> · score {hit.score.toFixed(2)}</span>
                      <p>{hit.chunk.text.slice(0, 400)}</p>
                    </li>
                  ))}
                </ol>
              )}
            </div>
          )}
        </Card>
      )}
    </div>
  );
}

function DocumentList({ sourceId }: { sourceId: string }) {
  const documents = useQuery({
    queryKey: ['knowledge', 'documents', sourceId],
    queryFn: () => api.get<KnowledgeDocument[]>(`/api/knowledge/sources/${sourceId}/documents`),
  });

  if (documents.isLoading) return <Spinner label="Loading files" />;
  if (!documents.data?.length) return <p className="muted">No files have been indexed yet.</p>;

  return (
    <div className="tablewrap">
      <table className="table">
        <caption className="visually-hidden">Files in this source</caption>
        <thead>
          <tr>
            <th scope="col">File</th>
            <th scope="col">State</th>
            <th scope="col">Passages</th>
            <th scope="col">Notes</th>
          </tr>
        </thead>
        <tbody>
          {documents.data.map((document) => (
            <tr key={document.id}>
              <th scope="row">{document.file_name}</th>
              <td>
                <Badge
                  tone={
                    document.state === 'indexed'
                      ? 'positive'
                      : document.state === 'failed'
                        ? 'negative'
                        : 'neutral'
                  }
                >
                  {document.state}
                </Badge>
              </td>
              <td>{document.chunk_count}</td>
              <td className="muted">{document.error ?? '—'}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function FolderPicker({ onChoose }: { onChoose: (path: string) => void }) {
  const [path, setPath] = useState<string | undefined>(undefined);
  const listing = useQuery({
    queryKey: ['knowledge', 'browse', path],
    queryFn: () =>
      api.get<BrowseResponse>(
        `/api/knowledge/browse${path ? `?path=${encodeURIComponent(path)}` : ''}`,
      ),
  });

  return (
    <Card
      title="Choose a folder"
      description="OTWONO will index the files it can read inside the folder you pick."
    >
      {listing.isLoading && <Spinner label="Reading the folder" />}
      {listing.data && (
        <>
          <div className="row row--tight">
            <code className="path">{listing.data.path}</code>
            <Button
              size="sm"
              disabled={!listing.data.parent}
              onClick={() => setPath(listing.data?.parent ?? undefined)}
            >
              Up one level
            </Button>
            <Button size="sm" variant="primary" onClick={() => onChoose(listing.data!.path)}>
              Authorise this folder
            </Button>
          </div>
          <ul className="filelist">
            {listing.data.entries.map((entry) => (
              <li key={entry.path}>
                {entry.is_directory ? (
                  <button type="button" className="filelist__dir" onClick={() => setPath(entry.path)}>
                    📁 {entry.name}
                  </button>
                ) : (
                  <span className={entry.supported ? 'filelist__file' : 'filelist__file muted'}>
                    📄 {entry.name}
                    {!entry.supported && ' (not readable by OTWONO)'}
                  </span>
                )}
              </li>
            ))}
          </ul>
        </>
      )}
      <Field
        label="Or type a path"
        hint="Useful when the folder is somewhere the list above does not reach."
      >
        {({ id, describedBy }) => (
          <div className="row row--tight">
            <input
              id={id}
              aria-describedby={describedBy}
              className="input"
              placeholder="/home/you/documents"
              onKeyDown={(event) => {
                if (event.key === 'Enter') {
                  onChoose((event.target as HTMLInputElement).value);
                }
              }}
            />
          </div>
        )}
      </Field>
    </Card>
  );
}
