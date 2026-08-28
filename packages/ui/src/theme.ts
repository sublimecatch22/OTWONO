/**
 * The theme engine.
 *
 * Preferences arrive from the service as plain values; this turns them into
 * attributes on the document element, which the token stylesheet reads. Nothing
 * here injects CSS: a preferences file can only ever select from the shipped
 * enumerations, so a hand-edited file cannot introduce styling.
 */

export type ThemeMode = 'system' | 'light' | 'dark' | 'high-contrast';
export type Density = 'comfortable' | 'cosy' | 'compact';
export type FontFamily = 'sans' | 'serif' | 'mono' | 'humanist';
export type SidebarPosition = 'left' | 'right';
export type MessagePresentation = 'bubbles' | 'flat';

export interface Preferences {
  theme: ThemeMode;
  accent: string;
  background: string;
  font_family: FontFamily;
  font_size_px: number;
  density: Density;
  sidebar_position: SidebarPosition;
  sidebar_width_px: number;
  sidebar_collapsed: boolean;
  visible_tabs: string[];
  chat_max_width_px: number;
  message_presentation: MessagePresentation;
  reduced_motion: boolean;
  show_inspector: boolean;
  dashboard_widgets: string[];
  saved_layouts: { name: string; preferences: unknown }[];
  sidebar_section_order: string[];
}

/** What the browser reports when the theme is "system". */
export function systemTheme(): 'light' | 'dark' {
  if (typeof window === 'undefined' || !window.matchMedia) return 'dark';
  return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}

/** The theme actually in force, resolving "system". */
export function resolveTheme(theme: ThemeMode): 'light' | 'dark' | 'high-contrast' {
  return theme === 'system' ? systemTheme() : theme;
}

const ALLOWED_ACCENTS = ['signal', 'ember', 'verdant', 'violet', 'slate'];
const ALLOWED_BACKGROUNDS = ['depth', 'flat', 'grid'];
const ALLOWED_FONTS: FontFamily[] = ['sans', 'serif', 'mono', 'humanist'];

/** Apply preferences to a document. Values outside the shipped sets are
 *  replaced with the default rather than passed through. */
export function applyPreferences(preferences: Preferences, root: HTMLElement): void {
  const theme = resolveTheme(preferences.theme);
  root.setAttribute('data-theme', theme);

  root.setAttribute(
    'data-accent',
    ALLOWED_ACCENTS.includes(preferences.accent) ? preferences.accent : 'signal',
  );
  root.setAttribute(
    'data-background',
    ALLOWED_BACKGROUNDS.includes(preferences.background) ? preferences.background : 'depth',
  );
  root.setAttribute(
    'data-font',
    ALLOWED_FONTS.includes(preferences.font_family) ? preferences.font_family : 'sans',
  );
  root.setAttribute('data-density', preferences.density);
  root.setAttribute('data-reduced-motion', String(preferences.reduced_motion));

  const size = clamp(preferences.font_size_px, 12, 24);
  root.style.setProperty('--font-size-base', `${size}px`);
  root.style.setProperty('--sidebar-width', `${clamp(preferences.sidebar_width_px, 200, 520)}px`);
  root.style.setProperty(
    '--chat-max-width',
    `${clamp(preferences.chat_max_width_px, 560, 1600)}px`,
  );
}

export function clamp(value: number, low: number, high: number): number {
  if (Number.isNaN(value)) return low;
  return Math.min(high, Math.max(low, value));
}

/** Watch the system theme so "system" follows it without a reload. */
export function watchSystemTheme(onChange: () => void): () => void {
  if (typeof window === 'undefined' || !window.matchMedia) return () => {};
  const query = window.matchMedia('(prefers-color-scheme: light)');
  query.addEventListener('change', onChange);
  return () => query.removeEventListener('change', onChange);
}
