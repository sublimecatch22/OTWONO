/**
 * Preferences: fetched from the service, applied to the document, and saved
 * back. The service is the source of truth — it sanitises every value — so the
 * client never keeps its own copy of the rules.
 */

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect } from 'react';
import { applyPreferences, watchSystemTheme, type Preferences } from '@otwono/ui';

import { api } from '../api/client';
import type { PreferencesResponse } from '../api/types';

export const PREFERENCES_KEY = ['settings', 'preferences'];

export function usePreferences() {
  return useQuery({
    queryKey: PREFERENCES_KEY,
    queryFn: () => api.get<PreferencesResponse>('/api/settings/preferences'),
    staleTime: 30_000,
  });
}

export function useSavePreferences() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (preferences: Preferences) =>
      api.put<PreferencesResponse>('/api/settings/preferences', preferences),
    onSuccess: (response) => {
      // Use the sanitised values the service returned, not what we sent.
      client.setQueryData(PREFERENCES_KEY, response);
    },
  });
}

export function useResetPreferences() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: () => api.post<PreferencesResponse>('/api/settings/preferences/reset'),
    onSuccess: (response) => client.setQueryData(PREFERENCES_KEY, response),
  });
}

/** Keep the document in step with the current preferences. */
export function useApplyPreferences(preferences: Preferences | undefined): void {
  useEffect(() => {
    if (!preferences) return;
    applyPreferences(preferences, document.documentElement);
  }, [preferences]);

  useEffect(() => {
    if (!preferences || preferences.theme !== 'system') return;
    return watchSystemTheme(() => applyPreferences(preferences, document.documentElement));
  }, [preferences]);
}
