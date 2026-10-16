import { useId, useRef, type ReactNode } from "react";
import { secondaryButton, useFocusTrap } from "./ConfirmDialog";

/** A panel sliding in from the right: traps focus and closes on Escape. */
export function Drawer({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();
  useFocusTrap(ref, onClose);
  return (
    <div className="fixed inset-0 z-30 flex justify-end bg-slate-900/40" onClick={onClose}>
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className="flex h-full w-full max-w-xl flex-col bg-white text-slate-900 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b border-slate-200 px-6 py-4">
          <h2 id={titleId} className="text-lg font-semibold">
            {title}
          </h2>
          <button type="button" className={secondaryButton} onClick={onClose}>
            Close
          </button>
        </div>
        {/* Focusable, so keyboard users can scroll long content. */}
        <div tabIndex={0} className="flex-1 overflow-y-auto p-6">
          {children}
        </div>
      </div>
    </div>
  );
}
