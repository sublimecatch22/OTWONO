/** Transient messages. Announced to assistive technology, dismissible, and
 *  never the only place an outcome is reported. */

import { useEffect } from 'react';

import { useUi } from '../state/ui';

export function Toasts() {
  const { toasts, dismissToast } = useUi();

  useEffect(() => {
    if (toasts.length === 0) return;
    const timers = toasts.map((toast) =>
      window.setTimeout(() => dismissToast(toast.id), toast.tone === 'negative' ? 12_000 : 6_000),
    );
    return () => timers.forEach(window.clearTimeout);
  }, [toasts, dismissToast]);

  if (toasts.length === 0) return null;

  return (
    <div className="toasts" aria-live="polite" aria-atomic="false">
      {toasts.map((toast) => (
        <div key={toast.id} className={`toast toast--${toast.tone}`}>
          <div>
            {toast.title && <strong>{toast.title}</strong>}
            <p>{toast.body}</p>
          </div>
          <button
            type="button"
            className="iconbutton iconbutton--small"
            onClick={() => dismissToast(toast.id)}
          >
            <span aria-hidden="true">×</span>
            <span className="visually-hidden">Dismiss</span>
          </button>
        </div>
      ))}
    </div>
  );
}
