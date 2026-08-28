/**
 * Badges for the lifecycle states.
 *
 * The label is the state's own name in plain words, so the meaning does not
 * depend on the colour.
 */

import { Badge } from './primitives';
import type { ListingState, ProjectState, TaskState } from '../api/types';

type Tone = 'neutral' | 'accent' | 'positive' | 'caution' | 'negative' | 'info';

const PROJECT: Record<ProjectState, { label: string; tone: Tone }> = {
  draft: { label: 'Draft', tone: 'neutral' },
  planned: { label: 'Planned', tone: 'info' },
  awaiting_approval: { label: 'Waiting for you', tone: 'caution' },
  running: { label: 'Running', tone: 'accent' },
  blocked: { label: 'Blocked', tone: 'caution' },
  verifying: { label: 'Verifying', tone: 'info' },
  completed: { label: 'Completed', tone: 'positive' },
  failed: { label: 'Failed', tone: 'negative' },
  cancelled: { label: 'Cancelled', tone: 'neutral' },
  archived: { label: 'Archived', tone: 'neutral' },
};

const TASK: Record<TaskState, { label: string; tone: Tone }> = {
  queued: { label: 'Queued', tone: 'neutral' },
  ready: { label: 'Ready', tone: 'info' },
  running: { label: 'Running', tone: 'accent' },
  awaiting_approval: { label: 'Waiting for you', tone: 'caution' },
  blocked: { label: 'Blocked', tone: 'caution' },
  verifying: { label: 'Verifying', tone: 'info' },
  completed: { label: 'Completed', tone: 'positive' },
  failed: { label: 'Failed', tone: 'negative' },
  cancelled: { label: 'Cancelled', tone: 'neutral' },
};

const LISTING: Record<ListingState, { label: string; tone: Tone }> = {
  draft: { label: 'Draft', tone: 'neutral' },
  awaiting_creator_approval: { label: 'Ready to publish', tone: 'info' },
  published: { label: 'Published', tone: 'accent' },
  assigned: { label: 'Assigned', tone: 'info' },
  submitted: { label: 'Submitted', tone: 'info' },
  revision_requested: { label: 'Revision requested', tone: 'caution' },
  accepted: { label: 'Accepted', tone: 'positive' },
  disputed: { label: 'Disputed', tone: 'negative' },
  cancelled: { label: 'Cancelled', tone: 'neutral' },
  rejected: { label: 'Refused by moderation', tone: 'negative' },
};

export function ProjectStateBadge({ state }: { state: ProjectState }) {
  const entry = PROJECT[state];
  return <Badge tone={entry.tone}>{entry.label}</Badge>;
}

export function TaskStateBadge({ state }: { state: TaskState }) {
  const entry = TASK[state];
  return <Badge tone={entry.tone}>{entry.label}</Badge>;
}

export function ListingStateBadge({ state }: { state: ListingState }) {
  const entry = LISTING[state];
  return <Badge tone={entry.tone}>{entry.label}</Badge>;
}

export const stateLabels = { PROJECT, TASK, LISTING };
