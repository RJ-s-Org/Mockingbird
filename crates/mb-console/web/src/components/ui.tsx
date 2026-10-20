import type { ReactNode } from "react";

/*
 * The console's shared styles and small building blocks. Colours come from
 * the semantic tokens in `index.css`, so nothing here needs a `dark:` variant.
 */

export const focusRing = "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring";

export const buttonClass = `inline-flex items-center justify-center gap-1.5 rounded-md px-3 py-1.5 text-sm font-medium whitespace-nowrap transition-colors ${focusRing} disabled:cursor-not-allowed disabled:opacity-50`;
export const primaryButton = `${buttonClass} bg-accent-solid text-white shadow-sm hover:bg-accent-solid-hover disabled:hover:bg-accent-solid`;
export const secondaryButton = `${buttonClass} border border-line-strong bg-surface text-fg shadow-xs hover:bg-surface-2 disabled:hover:bg-surface`;
export const dangerButton = `${buttonClass} bg-bad-solid text-white shadow-sm hover:bg-bad-solid-hover disabled:hover:bg-bad-solid`;
/** A quiet button for secondary row actions. */
export const ghostButton = `${buttonClass} text-fg-muted hover:bg-surface-2 hover:text-fg`;

export const inputClass = `block w-full rounded-md border border-line-strong bg-surface px-2.5 py-1.5 text-sm text-fg shadow-xs placeholder:text-fg-muted focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring disabled:bg-surface-2 disabled:text-fg-muted`;
export const labelClass = "block text-sm font-medium text-fg";
export const hintClass = "text-xs text-fg-muted";
export const errorTextClass = "text-sm text-bad-fg";
export const linkClass = `rounded-sm font-medium text-accent underline decoration-accent/40 underline-offset-2 hover:text-accent-hover hover:decoration-current ${focusRing}`;
/** A sunken block for JSON, YAML and other preformatted text. */
export const preClass = "overflow-x-auto rounded-md border border-line bg-surface-2 p-3 text-xs leading-relaxed text-fg";
/** The panel every table, form section and list sits on. */
export const cardClass = "rounded-lg border border-line bg-surface shadow-xs";

/** A page's title, an optional one-line description and its primary actions. */
export function PageHeader({
  title,
  description,
  actions,
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-3">
      <div className="min-w-0 space-y-1">
        <h1 className="text-2xl font-semibold tracking-tight break-words">{title}</h1>
        {description ? <p className="text-sm text-fg-muted">{description}</p> : null}
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
  );
}

/**
 * A titled panel: a form section, a settings group. Without a `title` it is
 * a plain panel. `id` names the heading, for `aria-labelledby`.
 */
export function Card({
  title,
  description,
  actions,
  id,
  as: Tag = "div",
  className = "",
  children,
}: {
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  id?: string;
  as?: "div" | "section";
  className?: string;
  children: ReactNode;
}) {
  return (
    <Tag className={`${cardClass} ${className}`} aria-labelledby={Tag === "section" && id ? id : undefined}>
      {title ? (
        <div className="flex flex-wrap items-start justify-between gap-3 border-b border-line px-4 py-3 sm:px-5">
          <div className="min-w-0 space-y-0.5">
            <h2 id={id} className="text-base font-semibold">
              {title}
            </h2>
            {description ? <p className="text-sm text-fg-muted">{description}</p> : null}
          </div>
          {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
        </div>
      ) : null}
      <div className="p-4 sm:p-5">{children}</div>
    </Tag>
  );
}

export type Tone = "neutral" | "accent" | "ok" | "warn" | "bad" | "info";

const NOTICE_TONE: Record<Exclude<Tone, "accent">, string> = {
  neutral: "border-line bg-surface text-fg",
  ok: "border-ok-line bg-ok-bg text-ok-fg",
  warn: "border-warn-line bg-warn-bg text-warn-fg",
  bad: "border-bad-line bg-bad-bg text-bad-fg",
  info: "border-info-line bg-info-bg text-info-fg",
};

/** A boxed message: a read-only reason, a warning, an outcome. */
export function Notice({
  tone = "neutral",
  role,
  className = "",
  children,
}: {
  tone?: Exclude<Tone, "accent">;
  role?: "status" | "alert";
  className?: string;
  children: ReactNode;
}) {
  return (
    <div role={role} className={`rounded-md border px-3 py-2.5 text-sm ${NOTICE_TONE[tone]} ${className}`}>
      {children}
    </div>
  );
}

const BADGE_TONE: Record<Tone, string> = {
  neutral: "border-line bg-surface-2 text-fg",
  accent: "border-transparent bg-accent-soft text-accent",
  ok: "border-ok-line bg-ok-bg text-ok-fg",
  warn: "border-warn-line bg-warn-bg text-warn-fg",
  bad: "border-bad-line bg-bad-bg text-bad-fg",
  info: "border-info-line bg-info-bg text-info-fg",
};

/** A small rounded label for a status or a kind. */
export function Badge({ tone = "neutral", title, children }: { tone?: Tone; title?: string; children: ReactNode }) {
  return (
    <span
      title={title}
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium whitespace-nowrap ${BADGE_TONE[tone]}`}
    >
      {children}
    </span>
  );
}

/** "Saved." after a save, announced to screen readers. */
export function SavedNote({ children = "Saved." }: { children?: ReactNode }) {
  return (
    <p role="status" className="inline-flex items-center gap-1.5 text-sm font-medium text-ok-fg">
      <svg aria-hidden="true" viewBox="0 0 16 16" className="size-4 fill-current">
        <path d="M6.2 11.3 2.9 8l-1 1 4.3 4.3 8-8-1-1z" />
      </svg>
      {children}
    </p>
  );
}

/** A spinner and "Loading…" (or `label`). */
export function Loading({ label = "Loading…" }: { label?: string }) {
  return (
    <p className="flex items-center gap-2 py-2 text-sm text-fg-muted">
      <svg aria-hidden="true" viewBox="0 0 24 24" className="size-4 animate-spin motion-reduce:animate-none">
        <circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" strokeOpacity="0.25" strokeWidth="3" />
        <path d="M21 12a9 9 0 0 0-9-9" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" />
      </svg>
      {label}
    </p>
  );
}

/** A labelled control with an optional hint or error below it; `children` is the control. */
export function Field({
  id,
  label,
  hint,
  error,
  className = "",
  children,
}: {
  id: string;
  label: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={`space-y-1.5 ${className}`}>
      <label htmlFor={id} className={labelClass}>
        {label}
      </label>
      {children}
      {error ? (
        <p id={`${id}-error`} className={errorTextClass}>
          {error}
        </p>
      ) : hint ? (
        <p id={`${id}-hint`} className={hintClass}>
          {hint}
        </p>
      ) : null}
    </div>
  );
}

/** A checkbox with its label on the right and an optional hint under it. */
export function Checkbox({
  id,
  label,
  hint,
  checked,
  disabled,
  onChange,
}: {
  id: string;
  label: ReactNode;
  hint?: ReactNode;
  checked: boolean;
  disabled?: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <div className="flex items-start gap-2.5">
      <input
        id={id}
        type="checkbox"
        className="mt-0.5 size-4 shrink-0 rounded border-line-strong"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        aria-describedby={hint ? `${id}-hint` : undefined}
      />
      <div className="space-y-0.5">
        <label htmlFor={id} className="text-sm font-medium">
          {label}
        </label>
        {hint ? (
          <p id={`${id}-hint`} className={hintClass}>
            {hint}
          </p>
        ) : null}
      </div>
    </div>
  );
}
