import { ApiError } from "../api/client";

/** Shows a failed call: the problem's title and detail, or the error message. */
export function ProblemAlert({ error }: { error: unknown }) {
  if (!error) return null;
  let title = "Something went wrong";
  let detail = error instanceof Error ? error.message : String(error);
  if (error instanceof ApiError && error.problem) {
    title = error.problem.title || title;
    detail = error.problem.detail;
  }
  return (
    <div role="alert" className="rounded-md border border-red-300 dark:border-red-800 bg-red-50 dark:bg-red-950/60 p-3 text-sm text-red-900 dark:text-red-200">
      <p className="font-semibold">{title}</p>
      {detail && detail !== title ? <p className="mt-1">{detail}</p> : null}
    </div>
  );
}
