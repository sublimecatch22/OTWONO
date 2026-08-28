/** The router and the outermost error boundary. */

import { Component, type ErrorInfo, type ReactNode } from 'react';
import { Navigate, Route, Routes } from 'react-router-dom';

import { AppShell } from './components/AppShell';
import { Button, Notice } from './components/primitives';
import { ActivityScreen } from './screens/ActivityScreen';
import { AgentsScreen } from './screens/AgentsScreen';
import { ChatScreen } from './screens/ChatScreen';
import { ConnectionsScreen } from './screens/ConnectionsScreen';
import { KnowledgeScreen } from './screens/KnowledgeScreen';
import { MarketplaceScreen } from './screens/MarketplaceScreen';
import { ProjectDetailScreen, ProjectsScreen } from './screens/ProjectsScreen';
import { SettingsScreen } from './screens/SettingsScreen';
import { TasksScreen } from './screens/TasksScreen';
import { WorkspaceDetailScreen, WorkspacesScreen } from './screens/WorkspacesScreen';
import { useApplyPreferences, usePreferences } from './state/preferences';

/**
 * A failure in one screen must not take the whole application with it — the
 * user still needs the sidebar, the emergency stop and a way out.
 */
class ScreenBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('A screen failed to render', error, info);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="screen">
          <Notice
            tone="negative"
            title="This screen could not be shown"
            action={
              <Button variant="primary" onClick={() => this.setState({ error: null })}>
                Try again
              </Button>
            }
          >
            {this.state.error.message}. Your data is unaffected — nothing was written.
          </Notice>
        </div>
      );
    }
    return this.props.children;
  }
}

export function App() {
  const preferences = usePreferences();
  useApplyPreferences(preferences.data?.preferences);

  return (
    <AppShell preferences={preferences.data?.preferences}>
      <ScreenBoundary>
        <Routes>
          <Route path="/" element={<Navigate to="/chat" replace />} />
          <Route path="/chat" element={<ChatScreen />} />
          <Route path="/chat/:conversationId" element={<ChatScreen />} />
          <Route path="/projects" element={<ProjectsScreen />} />
          <Route path="/projects/:projectId" element={<ProjectDetailScreen />} />
          <Route path="/agents" element={<AgentsScreen />} />
          <Route path="/tasks" element={<TasksScreen />} />
          <Route path="/knowledge" element={<KnowledgeScreen />} />
          <Route path="/connections" element={<ConnectionsScreen />} />
          <Route path="/workspaces" element={<WorkspacesScreen />} />
          <Route path="/workspaces/:workspaceId" element={<WorkspaceDetailScreen />} />
          <Route path="/marketplace" element={<MarketplaceScreen />} />
          <Route path="/activity" element={<ActivityScreen />} />
          <Route path="/settings" element={<SettingsScreen />} />
          <Route
            path="*"
            element={
              <div className="screen">
                <Notice tone="info" title="That screen does not exist">
                  Use the tabs above to get back.
                </Notice>
              </div>
            }
          />
        </Routes>
      </ScreenBoundary>
    </AppShell>
  );
}
