/**
 * The small set of controls the whole interface is built from.
 *
 * Every one of them is keyboard reachable, carries a visible focus ring from
 * the base stylesheet, and states its meaning in text rather than colour alone.
 */

import type { ButtonHTMLAttributes, HTMLAttributes, ReactNode } from 'react';
import { forwardRef, useId } from 'react';

type Tone = 'neutral' | 'accent' | 'positive' | 'caution' | 'negative' | 'info';

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: 'primary' | 'secondary' | 'ghost' | 'danger';
  size?: 'sm' | 'md';
  busy?: boolean;
  /** Shown beside the label; decorative, so it is hidden from assistive tech. */
  icon?: ReactNode;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { variant = 'secondary', size = 'md', busy = false, icon, children, disabled, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type="button"
      className={`btn btn--${variant} btn--${size}`}
      disabled={disabled || busy}
      aria-busy={busy || undefined}
      {...rest}
    >
      {busy ? <Spinner size={14} /> : icon ? <span aria-hidden="true">{icon}</span> : null}
      <span>{children}</span>
    </button>
  );
});

export function Spinner({ size = 16, label }: { size?: number; label?: string }) {
  return (
    <span
      className="spinner"
      style={{ width: size, height: size }}
      role={label ? 'status' : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : 'true'}
    />
  );
}

export function Badge({ tone = 'neutral', children }: { tone?: Tone; children: ReactNode }) {
  return <span className={`badge badge--${tone}`}>{children}</span>;
}

export function Card({
  title,
  description,
  actions,
  children,
  ...rest
}: {
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
} & HTMLAttributes<HTMLDivElement>) {
  return (
    <section className="card" {...rest}>
      {(title || actions) && (
        <header className="card__head">
          <div>
            {title && <h2 className="card__title">{title}</h2>}
            {description && <p className="card__description">{description}</p>}
          </div>
          {actions && <div className="card__actions">{actions}</div>}
        </header>
      )}
      <div className="card__body">{children}</div>
    </section>
  );
}

export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  children: (ids: { id: string; describedBy: string | undefined }) => ReactNode;
}) {
  const id = useId();
  const hintId = hint ? `${id}-hint` : undefined;
  const errorId = error ? `${id}-error` : undefined;
  const describedBy = [hintId, errorId].filter(Boolean).join(' ') || undefined;

  return (
    <div className="field">
      <label className="field__label" htmlFor={id}>
        {label}
      </label>
      {children({ id, describedBy })}
      {hint && (
        <p className="field__hint" id={hintId}>
          {hint}
        </p>
      )}
      {error && (
        <p className="field__error" id={errorId} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

export function EmptyState({
  title,
  description,
  action,
}: {
  title: string;
  description: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <h2 className="empty__title">{title}</h2>
      <p className="empty__description">{description}</p>
      {action && <div className="empty__action">{action}</div>}
    </div>
  );
}

/**
 * A message the user needs to read. `tone` sets the colour, but the heading
 * always names the kind too, so the meaning survives without colour.
 */
export function Notice({
  tone = 'info',
  title,
  children,
  action,
}: {
  tone?: Tone;
  title?: string;
  children: ReactNode;
  action?: ReactNode;
}) {
  const role = tone === 'negative' ? 'alert' : 'status';
  return (
    <div className={`notice notice--${tone}`} role={role}>
      <div className="notice__body">
        {title && <strong className="notice__title">{title}</strong>}
        <div>{children}</div>
      </div>
      {action && <div className="notice__action">{action}</div>}
    </div>
  );
}

export function Toolbar({ children }: { children: ReactNode }) {
  return <div className="toolbar">{children}</div>;
}

/** A definition list used for metadata panels. */
export function DetailList({ items }: { items: { label: string; value: ReactNode }[] }) {
  return (
    <dl className="details">
      {items.map((item) => (
        <div className="details__row" key={item.label}>
          <dt>{item.label}</dt>
          <dd>{item.value}</dd>
        </div>
      ))}
    </dl>
  );
}

/** Relative time with the exact value available on hover and to readers. */
export function TimeAgo({ value }: { value: string }) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return <span>—</span>;

  const seconds = Math.round((Date.now() - date.getTime()) / 1000);
  const label =
    seconds < 60
      ? 'just now'
      : seconds < 3600
        ? `${Math.floor(seconds / 60)} min ago`
        : seconds < 86400
          ? `${Math.floor(seconds / 3600)} h ago`
          : `${Math.floor(seconds / 86400)} d ago`;

  return (
    <time dateTime={value} title={date.toLocaleString()}>
      {label}
    </time>
  );
}

/** Money, always labelled as simulated by the caller's surrounding copy. */
export function Money({ minor, currency }: { minor: number; currency: string }) {
  const formatted = new Intl.NumberFormat(undefined, {
    style: 'currency',
    currency: currency || 'USD',
  }).format(minor / 100);
  return <span className="money">{formatted}</span>;
}
