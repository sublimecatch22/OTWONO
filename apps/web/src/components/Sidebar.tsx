/**
 * The sidebar.
 *
 * It separates the things the specification names — chats, the four workspace
 * kinds, projects, favourites and archived items — into collapsible sections
 * with a single search across all of them.
 */

import { useMemo, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { NavLink, useNavigate } from 'react-router-dom';

import { api } from '../api/client';
import type { Conversation, ProjectSummary, WorkspaceSummary } from '../api/types';
import { useUi } from '../state/ui';
import { Button, Spinner } from './primitives';

interface SidebarItem {
  id: string;
  label: string;
  to: string;
  meta?: string;
  favorite?: boolean;
  archived?: boolean;
}

const SECTION_LABELS: Record<string, string> = {
  chats: 'Chats',
  offices: 'Offices',
  labs: 'Labs',
  boardrooms: 'Boardrooms',
  'think-tanks': 'Think Tanks',
  projects: 'Saved projects',
  favorites: 'Favourites',
  archived: 'Archived',
};

export function Sidebar({ id }: { id: string }) {
  const navigate = useNavigate();
  const { sidebarQuery, setSidebarQuery } = useUi();
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({ archived: true });

  const conversations = useQuery({
    queryKey: ['conversations', 'sidebar'],
    queryFn: () => api.get<Conversation[]>('/api/conversations?include_archived=true'),
  });
  const workspaces = useQuery({
    queryKey: ['workspaces', 'sidebar'],
    queryFn: () => api.get<WorkspaceSummary[]>('/api/workspaces?include_archived=true'),
  });
  const projects = useQuery({
    queryKey: ['projects', 'sidebar'],
    queryFn: () => api.get<ProjectSummary[]>('/api/projects'),
  });

  const sections = useMemo(() => {
    const chats: SidebarItem[] = (conversations.data ?? []).map((conversation) => ({
      id: conversation.id,
      label: conversation.title,
      to: `/chat/${conversation.id}`,
      favorite: conversation.pinned,
      archived: conversation.archived,
    }));

    const byKind = (kind: string): SidebarItem[] =>
      (workspaces.data ?? [])
        .filter((workspace) => workspace.kind === kind)
        .map((workspace) => ({
          id: workspace.id,
          label: workspace.name,
          to: `/workspaces/${workspace.id}`,
          meta: `${workspace.member_count} agent${workspace.member_count === 1 ? '' : 's'}`,
          favorite: workspace.favorite,
          archived: workspace.archived,
        }));

    const projectItems: SidebarItem[] = (projects.data ?? []).map((project) => ({
      id: project.id,
      label: project.title,
      to: `/projects/${project.id}`,
      meta: `${project.completed_tasks}/${project.task_count} done`,
    }));

    const all = [
      ...chats,
      ...byKind('office'),
      ...byKind('lab'),
      ...byKind('boardroom'),
      ...byKind('think_tank'),
      ...projectItems,
    ];

    return {
      chats: chats.filter((item) => !item.archived),
      offices: byKind('office').filter((item) => !item.archived),
      labs: byKind('lab').filter((item) => !item.archived),
      boardrooms: byKind('boardroom').filter((item) => !item.archived),
      'think-tanks': byKind('think_tank').filter((item) => !item.archived),
      projects: projectItems,
      favorites: all.filter((item) => item.favorite && !item.archived),
      archived: all.filter((item) => item.archived),
    } satisfies Record<string, SidebarItem[]>;
  }, [conversations.data, workspaces.data, projects.data]);

  const query = sidebarQuery.trim().toLowerCase();
  const loading = conversations.isLoading || workspaces.isLoading || projects.isLoading;

  return (
    <aside id={id} className="sidebar" aria-label="Workspaces and conversations">
      <div className="sidebar__head">
        <label className="visually-hidden" htmlFor="sidebar-search">
          Search chats, workspaces and projects
        </label>
        <input
          id="sidebar-search"
          className="input input--search"
          type="search"
          placeholder="Search…"
          value={sidebarQuery}
          onChange={(event) => setSidebarQuery(event.target.value)}
        />
        <Button
          size="sm"
          variant="primary"
          onClick={async () => {
            const conversation = await api.post<Conversation>('/api/conversations', {});
            await conversations.refetch();
            navigate(`/chat/${conversation.id}`);
          }}
        >
          New chat
        </Button>
      </div>

      <div className="sidebar__body">
        {loading && (
          <p className="sidebar__loading">
            <Spinner label="Loading your workspaces" /> Loading…
          </p>
        )}

        {Object.entries(sections).map(([key, items]) => {
          const filtered = query
            ? items.filter((item) => item.label.toLowerCase().includes(query))
            : items;
          if (query && filtered.length === 0) return null;

          const isCollapsed = collapsed[key] ?? false;
          return (
            <section className="sidebar__section" key={key}>
              <h2>
                <button
                  type="button"
                  className="sidebar__sectionToggle"
                  aria-expanded={!isCollapsed}
                  onClick={() => setCollapsed((prev) => ({ ...prev, [key]: !isCollapsed }))}
                >
                  <span aria-hidden="true" className="sidebar__chevron">
                    {isCollapsed ? '▸' : '▾'}
                  </span>
                  {SECTION_LABELS[key] ?? key}
                  <span className="sidebar__count">{filtered.length}</span>
                </button>
              </h2>

              {!isCollapsed && (
                <ul className="sidebar__list">
                  {filtered.length === 0 && (
                    <li className="sidebar__empty">
                      {key === 'chats' ? 'No chats yet.' : `Nothing here yet.`}
                    </li>
                  )}
                  {filtered.map((item) => (
                    <li key={`${key}-${item.id}`}>
                      <NavLink
                        to={item.to}
                        className={({ isActive }) =>
                          `sidebar__item${isActive ? ' sidebar__item--active' : ''}`
                        }
                      >
                        <span className="sidebar__itemLabel">{item.label}</span>
                        {item.meta && <span className="sidebar__itemMeta">{item.meta}</span>}
                      </NavLink>
                    </li>
                  ))}
                </ul>
              )}
            </section>
          );
        })}
      </div>
    </aside>
  );
}
