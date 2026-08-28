/**
 * The human task marketplace, in its development preview.
 *
 * Every figure on this screen is simulated, and the screen says so wherever a
 * number appears.
 */

import { useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { api, ApiError } from '../api/client';
import type {
  LedgerEntry,
  Listing,
  ListingDetail,
  ModerationFinding,
} from '../api/types';
import {
  Badge,
  Button,
  Card,
  EmptyState,
  Field,
  Money,
  Notice,
  Spinner,
} from '../components/primitives';
import { ListingStateBadge } from '../components/StateBadge';
import { useUi } from '../state/ui';

type Path = 'creator' | 'worker';

export function MarketplaceScreen() {
  const [path, setPath] = useState<Path>('creator');

  return (
    <div className="screen">
      <header className="screen__head">
        <div>
          <h1>Marketplace</h1>
          <p className="screen__lede">
            Post work for a person to do, or take work on. This is a development preview.
          </p>
        </div>
        <div className="segmented" role="group" aria-label="Choose your path">
          <button
            type="button"
            className={path === 'creator' ? 'segmented__item segmented__item--active' : 'segmented__item'}
            aria-pressed={path === 'creator'}
            onClick={() => setPath('creator')}
          >
            I need something done
          </button>
          <button
            type="button"
            className={path === 'worker' ? 'segmented__item segmented__item--active' : 'segmented__item'}
            aria-pressed={path === 'worker'}
            onClick={() => setPath('worker')}
          >
            I want to do work
          </button>
        </div>
      </header>

      <Notice tone="caution" title="Payments here are simulated">
        No money moves and no worker is really paid. Every amount below is a record of intent,
        kept so the flow can be tested end to end.
      </Notice>

      {path === 'creator' ? <CreatorPath /> : <WorkerPath />}
    </div>
  );
}

function CreatorPath() {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [draft, setDraft] = useState({
    title: '',
    description: '',
    category: 'general',
    work_mode: 'remote',
    location_hint: '',
    deliverables: '',
    acceptance_criteria: '',
    evidence_required: '',
    compensation_minor: 0,
    expenses_minor: 0,
    safety_class: 'standard',
  });
  const [findings, setFindings] = useState<ModerationFinding[] | null>(null);
  const [openListing, setOpenListing] = useState<string | null>(null);

  const listings = useQuery({
    queryKey: ['marketplace', 'mine'],
    queryFn: () =>
      api.get<{ listings: (Listing & { moderation_findings: ModerationFinding[]; applications: number })[] }>(
        '/api/marketplace/my-listings',
      ),
  });

  const create = useMutation({
    mutationFn: () =>
      api.post<{ listing: Listing; moderation: unknown }>('/api/marketplace/listings', {
        ...draft,
        deliverables: splitLines(draft.deliverables),
        acceptance_criteria: splitLines(draft.acceptance_criteria),
        evidence_required: splitLines(draft.evidence_required),
      }),
    onSuccess: (result) => {
      client.invalidateQueries({ queryKey: ['marketplace'] });
      if (result.listing.state === 'rejected') {
        toast({
          tone: 'negative',
          title: 'That listing was refused',
          body: 'Moderation found something not permitted. The reasons are on the listing.',
        });
      } else {
        setFindings(null);
        toast({ tone: 'positive', body: 'Saved as a draft. Review it, then publish.' });
      }
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  const transition = useMutation({
    mutationFn: (input: { id: string; state: string }) =>
      api.post(`/api/marketplace/listings/${input.id}/state`, { state: input.state }),
    onSuccess: () => client.invalidateQueries({ queryKey: ['marketplace'] }),
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  return (
    <>
      <Card
        title="Describe the work"
        description="Write it so someone could do it without asking you a question."
      >
        <form
          className="stack"
          onSubmit={(event) => {
            event.preventDefault();
            create.mutate();
          }}
        >
          <Field label="Title">
            {({ id }) => (
              <input
                id={id}
                className="input"
                value={draft.title}
                onChange={(event) => setDraft({ ...draft, title: event.target.value })}
              />
            )}
          </Field>
          <Field label="What needs doing">
            {({ id }) => (
              <textarea
                id={id}
                className="textarea"
                rows={4}
                value={draft.description}
                onChange={(event) => setDraft({ ...draft, description: event.target.value })}
              />
            )}
          </Field>
          <div className="grid grid--two">
            <Field label="Deliverables" hint="One per line.">
              {({ id, describedBy }) => (
                <textarea
                  id={id}
                  aria-describedby={describedBy}
                  className="textarea"
                  rows={3}
                  value={draft.deliverables}
                  onChange={(event) => setDraft({ ...draft, deliverables: event.target.value })}
                />
              )}
            </Field>
            <Field label="How it will be judged" hint="One criterion per line.">
              {({ id, describedBy }) => (
                <textarea
                  id={id}
                  aria-describedby={describedBy}
                  className="textarea"
                  rows={3}
                  value={draft.acceptance_criteria}
                  onChange={(event) =>
                    setDraft({ ...draft, acceptance_criteria: event.target.value })
                  }
                />
              )}
            </Field>
          </div>
          <Field label="Evidence required" hint="One per line.">
            {({ id, describedBy }) => (
              <textarea
                id={id}
                aria-describedby={describedBy}
                className="textarea"
                rows={2}
                value={draft.evidence_required}
                onChange={(event) => setDraft({ ...draft, evidence_required: event.target.value })}
              />
            )}
          </Field>
          <div className="grid grid--three">
            <Field label="Where">
              {({ id }) => (
                <select
                  id={id}
                  className="select"
                  value={draft.work_mode}
                  onChange={(event) => setDraft({ ...draft, work_mode: event.target.value })}
                >
                  <option value="remote">Remote</option>
                  <option value="on_site">On site</option>
                </select>
              )}
            </Field>
            <Field label="Simulated compensation" hint="In whole units.">
              {({ id, describedBy }) => (
                <input
                  id={id}
                  aria-describedby={describedBy}
                  className="input"
                  type="number"
                  min={0}
                  value={draft.compensation_minor / 100}
                  onChange={(event) =>
                    setDraft({ ...draft, compensation_minor: Number(event.target.value) * 100 })
                  }
                />
              )}
            </Field>
            <Field label="Safety">
              {({ id }) => (
                <select
                  id={id}
                  className="select"
                  value={draft.safety_class}
                  onChange={(event) => setDraft({ ...draft, safety_class: event.target.value })}
                >
                  <option value="standard">Standard desk work</option>
                  <option value="physical_on_site">Involves travel or equipment</option>
                  <option value="handles_personal_data">Touches personal data</option>
                </select>
              )}
            </Field>
          </div>

          {findings && findings.length > 0 && (
            <Notice tone="negative" title="Moderation refused this">
              <ul>
                {findings.map((finding) => (
                  <li key={finding.matched}>
                    {finding.explanation} (matched “{finding.matched}”)
                  </li>
                ))}
              </ul>
            </Notice>
          )}

          <Button type="submit" variant="primary" busy={create.isPending} disabled={!draft.title.trim()}>
            Save as a draft
          </Button>
        </form>
      </Card>

      {listings.isLoading && <Spinner label="Loading your listings" />}

      {(listings.data?.listings ?? []).map((listing) => (
        <Card
          key={listing.id}
          title={listing.title}
          description={listing.description}
          actions={
            <>
              <ListingStateBadge state={listing.state} />
              {listing.state === 'draft' && (
                <Button
                  size="sm"
                  onClick={() =>
                    transition.mutate({ id: listing.id, state: 'awaiting_creator_approval' })
                  }
                >
                  Review for publishing
                </Button>
              )}
              {listing.state === 'awaiting_creator_approval' && (
                <Button
                  size="sm"
                  variant="primary"
                  onClick={() => transition.mutate({ id: listing.id, state: 'published' })}
                >
                  Publish
                </Button>
              )}
              <Button
                size="sm"
                onClick={() => setOpenListing(openListing === listing.id ? null : listing.id)}
              >
                {openListing === listing.id ? 'Hide' : 'Open'}
              </Button>
            </>
          }
        >
          <div className="row row--wrap">
            <Badge tone="neutral">{listing.work_mode.replace('_', ' ')}</Badge>
            <span className="muted">
              Simulated: <Money minor={listing.compensation_minor} currency={listing.currency} />
            </span>
            <span className="muted">{listing.applications} application(s)</span>
          </div>

          {listing.moderation_findings.length > 0 && (
            <Notice tone="negative" title="Why this was refused">
              <ul>
                {listing.moderation_findings.map((finding) => (
                  <li key={finding.matched}>
                    {finding.explanation} (matched “{finding.matched}”)
                  </li>
                ))}
              </ul>
              Change the wording and save a new listing, or ask for human review.
            </Notice>
          )}

          {openListing === listing.id && <ListingWorkspace listingId={listing.id} />}
        </Card>
      ))}
    </>
  );
}

function ListingWorkspace({ listingId }: { listingId: string }) {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const listing = useQuery({
    queryKey: ['marketplace', 'listing', listingId],
    queryFn: () => api.get<ListingDetail>(`/api/marketplace/listings/${listingId}`),
  });

  const assign = useMutation({
    mutationFn: (applicationId: string) =>
      api.post(`/api/marketplace/listings/${listingId}/assign`, {
        application_id: applicationId,
      }),
    onSuccess: () => client.invalidateQueries({ queryKey: ['marketplace'] }),
  });

  const review = useMutation({
    mutationFn: (decision: string) =>
      api.post<{ ledger_entry: LedgerEntry | null }>(
        `/api/marketplace/listings/${listingId}/review`,
        { decision },
      ),
    onSuccess: (result) => {
      client.invalidateQueries({ queryKey: ['marketplace'] });
      toast({
        tone: 'positive',
        body: result.ledger_entry
          ? 'Accepted. A simulated payout was recorded — no money moved.'
          : 'Recorded.',
      });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  if (listing.isLoading) return <Spinner label="Loading the task" />;
  if (!listing.data) return null;

  return (
    <div className="stack">
      <h3>Applications</h3>
      {listing.data.applications.length === 0 ? (
        <p className="muted">Nobody has applied yet.</p>
      ) : (
        <ul className="stack">
          {listing.data.applications.map((application) => (
            <li key={application.id} className="row">
              <div>
                <strong>{application.worker_account_id}</strong>
                <p className="muted">{application.proposal}</p>
                <p className="muted">
                  Quoted (simulated):{' '}
                  <Money minor={application.quoted_minor} currency={listing.data!.currency} />
                </p>
              </div>
              <div className="row row--tight">
                <Badge tone={application.state === 'assigned' ? 'positive' : 'neutral'}>
                  {application.state}
                </Badge>
                {listing.data!.state === 'published' && (
                  <Button size="sm" variant="primary" onClick={() => assign.mutate(application.id)}>
                    Assign
                  </Button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}

      {listing.data.state === 'submitted' && (
        <div className="row row--tight">
          <Button variant="primary" onClick={() => review.mutate('accept')}>
            Accept the work
          </Button>
          <Button onClick={() => review.mutate('request_revision')}>Ask for a revision</Button>
          <Button variant="danger" onClick={() => review.mutate('dispute')}>
            Raise a dispute
          </Button>
        </div>
      )}

      {listing.data.messages.length > 0 && (
        <>
          <h3>Messages</h3>
          <ul className="stack">
            {listing.data.messages.map((message) => (
              <li key={message.id}>
                <strong>{message.sender_account_id}</strong>
                <p>{message.body}</p>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

function WorkerPath() {
  const client = useQueryClient();
  const toast = useUi((state) => state.toast);
  const [proposal, setProposal] = useState('');
  const [applyingTo, setApplyingTo] = useState<string | null>(null);

  const listings = useQuery({
    queryKey: ['marketplace', 'browse'],
    queryFn: () => api.get<{ listings: Listing[] }>('/api/marketplace/listings'),
  });
  const ledger = useQuery({
    queryKey: ['marketplace', 'ledger'],
    queryFn: () =>
      api.get<{ entries: LedgerEntry[]; total_minor: number }>('/api/marketplace/ledger'),
  });

  const apply = useMutation({
    mutationFn: (input: { id: string; proposal: string }) =>
      api.post(`/api/marketplace/listings/${input.id}/apply`, {
        proposal: input.proposal,
        worker_account_id: 'demo-worker',
      }),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['marketplace'] });
      setApplyingTo(null);
      setProposal('');
      toast({ tone: 'positive', body: 'Application sent.' });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  const submit = useMutation({
    mutationFn: (id: string) =>
      api.post(`/api/marketplace/listings/${id}/submit`, {
        summary: 'Work submitted from the worker path.',
        deliverable_links: [],
        evidence_notes: '',
        worker_account_id: 'demo-worker',
      }),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ['marketplace'] });
      toast({ tone: 'positive', body: 'Submitted for review.' });
    },
    onError: (error) =>
      toast({ tone: 'negative', body: error instanceof ApiError ? error.message : String(error) }),
  });

  return (
    <>
      <Card title="Open tasks">
        {listings.isLoading && <Spinner label="Loading tasks" />}
        {listings.data?.listings.length === 0 && (
          <EmptyState
            title="Nothing published yet"
            description="Published tasks appear here. Drafts and refused listings never do."
          />
        )}
        <ul className="stack">
          {(listings.data?.listings ?? []).map((listing) => (
            <li key={listing.id} className="stack">
              <div className="row row--between">
                <div>
                  <strong>{listing.title}</strong>
                  <p className="muted">{listing.description}</p>
                  <p className="muted">
                    Simulated pay:{' '}
                    <Money minor={listing.compensation_minor} currency={listing.currency} /> ·{' '}
                    {listing.work_mode.replace('_', ' ')}
                  </p>
                </div>
                <div className="row row--tight">
                  <ListingStateBadge state={listing.state} />
                  <Button
                    size="sm"
                    onClick={() => setApplyingTo(applyingTo === listing.id ? null : listing.id)}
                  >
                    Apply
                  </Button>
                  <Button size="sm" onClick={() => submit.mutate(listing.id)}>
                    Submit work
                  </Button>
                </div>
              </div>
              {applyingTo === listing.id && (
                <form
                  className="row row--tight"
                  onSubmit={(event) => {
                    event.preventDefault();
                    apply.mutate({ id: listing.id, proposal });
                  }}
                >
                  <label className="visually-hidden" htmlFor={`proposal-${listing.id}`}>
                    Your proposal
                  </label>
                  <input
                    id={`proposal-${listing.id}`}
                    className="input"
                    value={proposal}
                    placeholder="Why you, and when you could do it"
                    onChange={(event) => setProposal(event.target.value)}
                  />
                  <Button type="submit" variant="primary" busy={apply.isPending}>
                    Send
                  </Button>
                </form>
              )}
            </li>
          ))}
        </ul>
      </Card>

      <Card title="Simulated earnings" description="A record of intent. No money has moved.">
        {ledger.data?.entries.length === 0 ? (
          <p className="muted">Nothing yet.</p>
        ) : (
          <ul className="stack">
            {(ledger.data?.entries ?? []).map((entry) => (
              <li key={entry.id} className="row row--between">
                <span>{entry.note}</span>
                <span>
                  <Money minor={entry.amount_minor} currency={entry.currency} />{' '}
                  <Badge tone="caution">simulated</Badge>
                </span>
              </li>
            ))}
          </ul>
        )}
      </Card>
    </>
  );
}

function splitLines(value: string): string[] {
  return value
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean);
}
