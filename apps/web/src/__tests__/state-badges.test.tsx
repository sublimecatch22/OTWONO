import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { ListingStateBadge, ProjectStateBadge, TaskStateBadge } from '../components/StateBadge';
import type { ListingState, ProjectState, TaskState } from '../api/types';

describe('state badges', () => {
  it('names every project state in plain words', () => {
    const states: ProjectState[] = [
      'draft',
      'planned',
      'awaiting_approval',
      'running',
      'blocked',
      'verifying',
      'completed',
      'failed',
      'cancelled',
      'archived',
    ];
    for (const state of states) {
      const { unmount } = render(<ProjectStateBadge state={state} />);
      const text = screen.getByText(/./).textContent ?? '';
      expect(text.length).toBeGreaterThan(0);
      expect(text).not.toContain('_');
      unmount();
    }
  });

  it('says who is being waited on rather than showing a raw state name', () => {
    render(<TaskStateBadge state="awaiting_approval" />);
    expect(screen.getByText('Waiting for you')).toBeInTheDocument();
  });

  it('explains a moderation refusal rather than saying "rejected"', () => {
    render(<ListingStateBadge state={'rejected' as ListingState} />);
    expect(screen.getByText('Refused by moderation')).toBeInTheDocument();
  });

  it('covers every task state', () => {
    const states: TaskState[] = [
      'queued',
      'ready',
      'running',
      'awaiting_approval',
      'blocked',
      'verifying',
      'completed',
      'failed',
      'cancelled',
    ];
    for (const state of states) {
      const { unmount } = render(<TaskStateBadge state={state} />);
      expect(screen.getByText(/./)).toBeInTheDocument();
      unmount();
    }
  });
});
