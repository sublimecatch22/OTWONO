import { describe, expect, it, vi } from 'vitest';

import { applyPreferences, clamp, resolveTheme, type Preferences } from '../theme';

function preferences(overrides: Partial<Preferences> = {}): Preferences {
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
    visible_tabs: ['chat', 'settings'],
    chat_max_width_px: 820,
    message_presentation: 'bubbles',
    reduced_motion: false,
    show_inspector: false,
    dashboard_widgets: [],
    saved_layouts: [],
    sidebar_section_order: [],
    ...overrides,
  };
}

describe('theme', () => {
  it('writes the chosen theme and accent onto the document', () => {
    const root = document.createElement('html');
    applyPreferences(preferences({ theme: 'high-contrast', accent: 'ember' }), root);

    expect(root.getAttribute('data-theme')).toBe('high-contrast');
    expect(root.getAttribute('data-accent')).toBe('ember');
  });

  it('refuses a value that is not one of the shipped tokens', () => {
    const root = document.createElement('html');
    applyPreferences(
      preferences({
        accent: 'url(javascript:alert(1))',
        background: '../../etc/passwd',
        font_family: 'comic' as Preferences['font_family'],
      }),
      root,
    );

    expect(root.getAttribute('data-accent')).toBe('signal');
    expect(root.getAttribute('data-background')).toBe('depth');
    expect(root.getAttribute('data-font')).toBe('sans');
  });

  it('clamps sizes so a hand-edited file cannot make the interface unusable', () => {
    const root = document.createElement('html');
    applyPreferences(
      preferences({ font_size_px: 900, sidebar_width_px: 1, chat_max_width_px: 100000 }),
      root,
    );

    expect(root.style.getPropertyValue('--font-size-base')).toBe('24px');
    expect(root.style.getPropertyValue('--sidebar-width')).toBe('200px');
    expect(root.style.getPropertyValue('--chat-max-width')).toBe('1600px');
  });

  it('resolves "system" from what the browser reports', () => {
    const original = window.matchMedia;
    window.matchMedia = vi.fn().mockReturnValue({ matches: true }) as never;
    expect(resolveTheme('system')).toBe('light');

    window.matchMedia = vi.fn().mockReturnValue({ matches: false }) as never;
    expect(resolveTheme('system')).toBe('dark');
    window.matchMedia = original;
  });

  it('passes an explicit theme through unchanged', () => {
    expect(resolveTheme('dark')).toBe('dark');
    expect(resolveTheme('high-contrast')).toBe('high-contrast');
  });

  it('clamps safely, including a value that is not a number', () => {
    expect(clamp(5, 10, 20)).toBe(10);
    expect(clamp(50, 10, 20)).toBe(20);
    expect(clamp(Number.NaN, 10, 20)).toBe(10);
  });

  it('reduced motion is recorded so the stylesheet can honour it', () => {
    const root = document.createElement('html');
    applyPreferences(preferences({ reduced_motion: true }), root);
    expect(root.getAttribute('data-reduced-motion')).toBe('true');
  });
});
