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
      className="rounded-md border border-red-300 dark:border-red-800 bg-red-50 dark:bg-red-950/60 p-3 text-sm text-red-900 dark:text-red-200 focus:outline-none focus-visible:ring-2 focus-visible:ring-red-500"
    >
      <p className="font-semibold">{title}</p>
      {detail && detail !== title ? <p className="mt-1">{detail}</p> : null}
    </div>
  );
}
