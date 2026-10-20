import { useEffect, useRef } from "react";
import { ApiError } from "../api/client";

/**
 * Shows a failed call: the problem's title and detail, or the error message.
 *
 * With `focus`, a new error is scrolled into view and focused, so one that
 * appears below a long form (a drawer or dialog) is seen and announced.
 */
export function ProblemAlert({ error, focus = false }: { error: unknown; focus?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!focus || !error || !el) return;
    el.scrollIntoView?.({ block: "nearest" });
    el.focus({ preventScroll: true });
  }, [error, focus]);
  if (!error) return null;
  let title = "Something went wrong";
  let detail = error instanceof Error ? error.message : String(error);
  if (error instanceof ApiError && error.problem) {
    title = error.problem.title || title;
    detail = error.problem.detail;
  }
  return (
    <div
      ref={ref}
      role="alert"
      tabIndex={focus ? -1 : undefined}
      className="flex gap-2.5 rounded-md border border-bad-line bg-bad-bg px-3 py-2.5 text-sm text-bad-fg focus:outline-none focus-visible:ring-2 focus-visible:ring-bad-solid"
    >
      <svg aria-hidden="true" viewBox="0 0 20 20" className="mt-0.5 size-4 shrink-0 fill-current">
        <path d="M10 1.5a8.5 8.5 0 1 0 0 17 8.5 8.5 0 0 0 0-17ZM9 5.5h2v6H9v-6Zm0 7.5h2v2H9v-2Z" />
      </svg>
      <div className="min-w-0 break-words">
        <p className="font-semibold">{title}</p>
        {detail && detail !== title ? <p className="mt-0.5">{detail}</p> : null}
      </div>
    </div>
  );
}
