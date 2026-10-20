import { useId, useRef, type ReactNode } from "react";
import { useFocusTrap } from "./ConfirmDialog";
import { secondaryButton } from "./ui";

/** A panel sliding in from the right: traps focus and closes on Escape. */
export function Drawer({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();
  useFocusTrap(ref, onClose);
  return (
    <div className="fixed inset-0 z-30 flex justify-end bg-slate-950/40 backdrop-blur-[2px] dark:bg-black/60" onClick={onClose}>
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className="flex h-full w-full max-w-xl flex-col border-l border-line bg-surface text-fg shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between gap-3 border-b border-line px-4 py-3.5 sm:px-6">
          <h2 id={titleId} className="min-w-0 text-lg font-semibold tracking-tight break-words">
            {title}
          </h2>
          <button type="button" className={secondaryButton} onClick={onClose}>
            Close
          </button>
        </div>
        {/* Focusable, so keyboard users can scroll long content. */}
        <div tabIndex={0} className="flex-1 overflow-y-auto p-4 sm:p-6">
          {children}
        </div>
      </div>
    </div>
  );
}
