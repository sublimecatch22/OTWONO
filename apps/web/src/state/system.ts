/** System status, refreshed often enough that the emergency stop is honest. */

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { api } from '../api/client';
import type { SystemStatus } from '../api/types';

export const SYSTEM_KEY = ['system', 'status'];

export function useSystemStatus() {
  return useQuery({
    queryKey: SYSTEM_KEY,
    queryFn: () => api.get<SystemStatus>('/api/system/status'),
    refetchInterval: 15_000,
    refetchOnWindowFocus: true,
  });
}

export function useEmergencyStop() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (input: { engaged: boolean; revoke_all_permissions: boolean }) =>
      api.post<{ engaged: boolean; revoked_grants: number; message: string }>(
        '/api/system/emergency-stop',
        input,
      ),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: SYSTEM_KEY });
      client.invalidateQueries({ queryKey: ['permissions'] });
    },
  });
}
