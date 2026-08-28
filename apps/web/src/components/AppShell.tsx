/**
 * The application frame: top navigation, a collapsible sidebar, the main
 * region, and an optional inspector.
 *
 * The order in the document is skip link → navigation → main, so a keyboard
 * user reaches the content in one press.
 */

import type { ReactNode } from 'react';
import { useEffect } from 'react';
import { NavLink, useLocation } from 'react-router-dom';

import type { Preferences } from '@otwono/ui';
import { useUi } from '../state/ui';
import { useSystemStatus } from '../state/system';
import { EmergencyStopControl } from './EmergencyStop';
import { Sidebar } from './Sidebar';
import { Toasts } from './Toasts';

export interface TabDefinition {
  key: string;
  label: string;
  path: string;
  icon: string;
}

export const ALL_TABS: TabDefinition[] = [
  { key: 'chat', label: 'Chat', path: '/chat', icon: '💬' },
  { key: 'projects', label: 'Projects', path: '/projects', icon: '📁' },
  { key: 'agents', label: 'Agents', path: '/agents', icon: '🧠' },
  { key: 'tasks', label: 'Tasks', path: '/tasks', icon: '✓' },
  { key: 'knowledge', label: 'Knowledge', path: '/knowledge', icon: '📚' },
  { key: 'connections', label: 'Connections', path: '/connections', icon: '🔌' },
  { key: 'marketplace', label: 'Marketplace', path: '/marketplace', icon: '🤝' },
  { key: 'activity', label: 'Activity', path: '/activity', icon: '📜' },
  { key: 'settings', label: 'Settings', path: '/settings', icon: '⚙' },
];

export function visibleTabs(preferences: Preferences | undefined): TabDefinition[] {
  if (!preferences) return ALL_TABS;
  const chosen = new Set(preferences.visible_tabs);
  const tabs = ALL_TABS.filter((tab) => chosen.has(tab.key));
  // The service guarantees chat and settings survive; belt and braces here so
  // a stale cached value cannot strand the user.
  return tabs.length > 0 ? tabs : ALL_TABS;
}

export function AppShell({
  preferences,
  children,
}: {
  preferences: Preferences | undefined;
  children: ReactNode;
}) {
  const { sidebarOpen, setSidebarOpen, inspectorOpen } = useUi();
  const status = useSystemStatus();
  const location = useLocation();
  const tabs = visibleTabs(preferences);

  // The saved collapsed state applies on load; after that the session's own
  // toggling wins until the preference changes again.
  useEffect(() => {
    if (preferences) setSidebarOpen(!preferences.sidebar_collapsed);
  }, [preferences?.sidebar_collapsed, setSidebarOpen, preferences]);

  const sidebarRight = preferences?.sidebar_position === 'right';

  return (
    <div
      className="shell"
      data-sidebar={sidebarOpen ? 'open' : 'closed'}
      data-sidebar-side={sidebarRight ? 'right' : 'left'}
      data-inspector={inspectorOpen ? 'open' : 'closed'}
    >
      <a className="skip-link" href="#main">
        Skip to the main content
      </a>

      <header className="topbar">
        <div className="topbar__lead">
          <button
            type="button"
            className="iconbutton"
            onClick={() => setSidebarOpen(!sidebarOpen)}
            aria-expanded={sidebarOpen}
            aria-controls="sidebar"
          >
            <span aria-hidden="true">☰</span>
            <span className="visually-hidden">
              {sidebarOpen ? 'Hide the sidebar' : 'Show the sidebar'}
            </span>
          </button>
          <span className="wordmark">
            OTWONO<span className="wordmark__mark"> AI</span>
          </span>
        </div>

        <nav className="tabs" aria-label="Main">
          <ul>
            {tabs.map((tab) => (
              <li key={tab.key}>
                <NavLink
                  to={tab.path}
                  className={({ isActive }) => `tab${isActive ? ' tab--active' : ''}`}
                  aria-current={location.pathname.startsWith(tab.path) ? 'page' : undefined}
                >
                  <span className="tab__icon" aria-hidden="true">
                    {tab.icon}
                  </span>
                  <span className="tab__label">{tab.label}</span>
                </NavLink>
              </li>
            ))}
          </ul>
        </nav>

        <div className="topbar__trail">
          {status.data && status.data.open_permission_requests > 0 && (
            <NavLink to="/settings#permissions" className="pill pill--caution">
              {status.data.open_permission_requests} awaiting approval
            </NavLink>
          )}
          <EmergencyStopControl />
        </div>
      </header>

      {status.data?.emergency_stop && (
        <div className="stopbanner" role="alert">
          <strong>Emergency stop is engaged.</strong> No agent can act until you release it.
        </div>
      )}

      <Sidebar id="sidebar" />

      <main id="main" className="main" tabIndex={-1}>
        {children}
      </main>

      <Toasts />
    </div>
  );
}
