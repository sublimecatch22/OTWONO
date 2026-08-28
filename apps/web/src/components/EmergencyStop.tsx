/**
 * The emergency stop.
 *
 * Engaging it is one click, because that is the point. Releasing it needs a
 * confirmation, because releasing by accident is the mistake that matters.
 */

import * as Dialog from '@radix-ui/react-dialog';
import { useState } from 'react';

import { useEmergencyStop, useSystemStatus } from '../state/system';
import { useUi } from '../state/ui';
import { Button } from './primitives';

export function EmergencyStopControl() {
  const status = useSystemStatus();
  const stop = useEmergencyStop();
  const toast = useUi((state) => state.toast);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [revokeAll, setRevokeAll] = useState(false);

  const engaged = status.data?.emergency_stop ?? false;

  if (engaged) {
    return (
      <Dialog.Root open={confirmOpen} onOpenChange={setConfirmOpen}>
        <Dialog.Trigger asChild>
          <Button variant="primary" size="sm">
            Release stop
          </Button>
        </Dialog.Trigger>
        <Dialog.Portal>
          <Dialog.Overlay className="overlay" />
          <Dialog.Content className="dialog" aria-describedby="release-stop-description">
            <Dialog.Title className="dialog__title">Release the emergency stop?</Dialog.Title>
            <p id="release-stop-description">
              Agents will be able to act again, using whatever permissions are still granted.
              Permissions you revoked while stopped are <strong>not</strong> restored.
            </p>
            <div className="dialog__actions">
              <Dialog.Close asChild>
                <Button>Keep everything stopped</Button>
              </Dialog.Close>
              <Button
                variant="primary"
                busy={stop.isPending}
                onClick={() =>
                  stop.mutate(
                    { engaged: false, revoke_all_permissions: false },
                    {
                      onSuccess: (result) => {
                        setConfirmOpen(false);
                        toast({ tone: 'positive', body: result.message });
                      },
                    },
                  )
                }
              >
                Release
              </Button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    );
  }

  return (
    <Dialog.Root open={confirmOpen} onOpenChange={setConfirmOpen}>
      <Dialog.Trigger asChild>
        <Button variant="danger" size="sm" icon="■">
          Stop everything
        </Button>
      </Dialog.Trigger>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog" aria-describedby="engage-stop-description">
          <Dialog.Title className="dialog__title">Stop everything now?</Dialog.Title>
          <p id="engage-stop-description">
            Every agent stops immediately. Nothing runs — no file is read, no page is fetched, no
            project continues — until you release the stop.
          </p>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={revokeAll}
              onChange={(event) => setRevokeAll(event.target.checked)}
            />
            <span>
              Also revoke every permission I have granted. Agents will ask again next time.
            </span>
          </label>
          <div className="dialog__actions">
            <Dialog.Close asChild>
              <Button>Cancel</Button>
            </Dialog.Close>
            <Button
              variant="danger"
              busy={stop.isPending}
              onClick={() =>
                stop.mutate(
                  { engaged: true, revoke_all_permissions: revokeAll },
                  {
                    onSuccess: (result) => {
                      setConfirmOpen(false);
                      toast({ tone: 'caution', title: 'Stopped', body: result.message });
                    },
                  },
                )
              }
            >
              Stop everything
            </Button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
