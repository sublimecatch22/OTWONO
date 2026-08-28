/**
 * View state that is not worth a round trip: which panel is open, what the
 * sidebar is filtered to, and any transient message.
 *
 * Anything the user would expect to survive a restart lives in preferences on
 * the service instead.
 */

import { create } from 'zustand';

export interface ToastMessage {
  id: string;
  tone: 'info' | 'positive' | 'caution' | 'negative';
  title?: string;
  body: string;
}

interface UiState {
  sidebarOpen: boolean;
  inspectorOpen: boolean;
  sidebarQuery: string;
  runDrawerOpen: boolean;
  toasts: ToastMessage[];

  toggleSidebar: () => void;
  setSidebarOpen: (open: boolean) => void;
  toggleInspector: () => void;
  setInspectorOpen: (open: boolean) => void;
  setSidebarQuery: (query: string) => void;
  setRunDrawerOpen: (open: boolean) => void;

  toast: (message: Omit<ToastMessage, 'id'>) => void;
  dismissToast: (id: string) => void;
}

export const useUi = create<UiState>((set) => ({
  sidebarOpen: true,
  inspectorOpen: false,
  sidebarQuery: '',
  runDrawerOpen: false,
  toasts: [],

  toggleSidebar: () => set((state) => ({ sidebarOpen: !state.sidebarOpen })),
  setSidebarOpen: (open) => set({ sidebarOpen: open }),
  toggleInspector: () => set((state) => ({ inspectorOpen: !state.inspectorOpen })),
  setInspectorOpen: (open) => set({ inspectorOpen: open }),
  setSidebarQuery: (query) => set({ sidebarQuery: query }),
  setRunDrawerOpen: (open) => set({ runDrawerOpen: open }),

  toast: (message) =>
    set((state) => ({
      toasts: [...state.toasts, { ...message, id: Math.random().toString(36).slice(2) }],
    })),
  dismissToast: (id) => set((state) => ({ toasts: state.toasts.filter((t) => t.id !== id) })),
}));
