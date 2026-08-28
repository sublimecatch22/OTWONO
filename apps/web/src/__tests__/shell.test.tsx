import { describe, expect, it } from 'vitest';

import { ALL_TABS, visibleTabs } from '../components/AppShell';
import type { Preferences } from '@otwono/ui';

function preferences(visible: string[]): Preferences {
  return {
    theme: 'dark',
    accent: 'signal',
    background: 'depth',
    font_family: 'sans',
    font_size_px: 15,
    density: 'comfortable',
    sidebar_position: 'left',
    sidebar_width_px: 280,
    sidebar_collapsed: false,
    visible_tabs: visible,
    chat_max_width_px: 820,
    message_presentation: 'bubbles',
    reduced_motion: false,
    show_inspector: false,
    dashboard_widgets: [],
    saved_layouts: [],
    sidebar_section_order: [],
  };
}

describe('navigation', () => {
  it('every tab the specification names has a route', () => {
    const keys = ALL_TABS.map((tab) => tab.key);
    for (const expected of [
      'chat',
      'projects',
      'agents',
      'tasks',
      'knowledge',
      'connections',
      'marketplace',
      'activity',
      'settings',
    ]) {
      expect(keys).toContain(expected);
    }
    expect(ALL_TABS.every((tab) => tab.path.startsWith('/'))).toBe(true);
  });

  it('shows only the tabs the user chose', () => {
    const tabs = visibleTabs(preferences(['chat', 'settings', 'knowledge']));
    expect(tabs.map((tab) => tab.key)).toEqual(['chat', 'knowledge', 'settings']);
  });

  it('falls back to every tab rather than stranding the user with none', () => {
    expect(visibleTabs(preferences([])).length).toBe(ALL_TABS.length);
    expect(visibleTabs(undefined).length).toBe(ALL_TABS.length);
  });
});
