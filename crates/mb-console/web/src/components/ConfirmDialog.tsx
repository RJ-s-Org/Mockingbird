import { useEffect, useId, useRef, type ReactNode, type RefObject } from "react";

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/**
 * Keeps keyboard focus inside `ref` while mounted, closes on Escape, and
 * gives focus back to whatever had it before.
 */
export function useFocusTrap(ref: RefObject<HTMLElement | null>, onClose: () => void): void {
  const close = useRef(onClose);
  useEffect(() => {
    close.current = onClose;
  });
  useEffect(() => {
    const root = ref.current;
    if (!root) return;
    const previous = document.activeElement as HTMLElement | null;
    const focusables = () => Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE));
    (focusables()[0] ?? root).focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close.current();
        return;
      }
      if (e.key !== "Tab") return;
      const items = focusables();
      const first = items[0];
      const last = items[items.length - 1];
      if (!first || !last) {
        e.preventDefault();
        return;
      }
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      previous?.focus();
    };
  }, [ref]);
}

export const buttonClass =
  "rounded-md px-3 py-1.5 text-sm font-medium focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700 disabled:opacity-60";
export const primaryButton = `${buttonClass} bg-indigo-700 text-white hover:bg-indigo-800`;
export const secondaryButton = `${buttonClass} border border-slate-400 bg-white text-slate-900 hover:bg-slate-100`;
export const inputClass =
  "w-full rounded-md border border-slate-400 bg-white px-2 py-1.5 text-sm text-slate-900 focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-indigo-700 disabled:bg-slate-100 disabled:text-slate-700";
export const dangerButton = `${buttonClass} bg-red-700 text-white hover:bg-red-800`;

/** A modal dialog: traps focus, closes on Escape or a click on the backdrop. */
export function Dialog({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();
  useFocusTrap(ref, onClose);
  return (
    <div className="fixed inset-0 z-40 flex items-center justify-center bg-slate-900/50 p-4" onClick={onClose}>
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className="w-full max-w-md rounded-lg bg-white p-6 text-slate-900 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id={titleId} className="mb-4 text-lg font-semibold">
          {title}
        </h2>
        {children}
      </div>
    </div>
  );
}

/** Asks before a destructive action. */
export function ConfirmDialog({
  title,
  message,
  confirmLabel = "Confirm",
  busy = false,
  onConfirm,
  onCancel,
}: {
  title: string;
  message: ReactNode;
  confirmLabel?: string;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <Dialog title={title} onClose={onCancel}>
      <div className="mb-6 text-sm text-slate-700">{message}</div>
      <div className="flex justify-end gap-2">
        <button type="button" className={secondaryButton} onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className={dangerButton} onClick={onConfirm} disabled={busy}>
          {confirmLabel}
        </button>
      </div>
    </Dialog>
  );
}
