import { useMetricsSummary, useReadiness } from "../api/queries";
import { ProblemAlert } from "../components/ProblemAlert";
import { sourceLabel } from "../lib/sources";
import { TableScroll } from "../components/DataTable";

const STATUS_STYLE: Record<string, string> = {
  ready: "border-green-300 dark:border-green-800 bg-green-50 dark:bg-green-950/60 text-green-900 dark:text-green-200",
  degraded: "border-amber-300 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/60 text-amber-900 dark:text-amber-200",
};

/** A source's human label; the tooltip explains it and names the raw value. */
function SourceName({ source }: { source: string }) {
  const { label, hint } = sourceLabel(source);
  if (label === source) return <>{source}</>;
  return (
    <span
      title={`${hint} (source "${source}")`}
      className="cursor-help underline decoration-slate-400 dark:decoration-slate-500 decoration-dotted underline-offset-2"
    >
      {label}
    </span>
  );
}

function formatCount(n: number): string {
  return String(Math.round(n * 1000) / 1000);
}

/** Server readiness (`/readyz`) and a request summary from `/metrics`. */
export function Status() {
  const ready = useReadiness();
  const metrics = useMetricsSummary();
  const summary = metrics.data === "private" ? undefined : metrics.data;

  return (
    <section className="space-y-6">
      <h1 className="text-2xl font-semibold">Status</h1>
      <div className="space-y-2">
        <h2 className="text-lg font-semibold">Readiness</h2>
        {ready.error ? <ProblemAlert error={ready.error} /> : null}
        {ready.data ? (
          <>
            <p>
              <span
                className={`inline-flex rounded-full border px-2 py-0.5 text-sm font-medium ${
                  STATUS_STYLE[ready.data.status] ?? "border-red-300 dark:border-red-800 bg-red-50 dark:bg-red-950/60 text-red-900 dark:text-red-200"
                }`}
              >
                {ready.data.status}
              </span>
            </p>
            {ready.data.reasons.length > 0 ? (
              <ul aria-label="Reasons" className="list-disc pl-5 text-sm">
                {ready.data.reasons.map((r) => (
                  <li key={r}>{r}</li>
                ))}
              </ul>
            ) : null}
          </>
        ) : ready.isPending ? (
          <p className="text-slate-700 dark:text-slate-300">Loading…</p>
        ) : null}
      </div>
      <div className="space-y-2">
        <h2 className="text-lg font-semibold">Requests</h2>
        {metrics.error ? <ProblemAlert error={metrics.error} /> : null}
        {metrics.data === "private" ? (
          <p className="rounded-md border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-3 text-sm text-slate-800 dark:text-slate-200">Metrics are private</p>
        ) : null}
        {summary ? (
          <>
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
              <dt className="font-medium">Requests served</dt>
              <dd data-testid="requests-total">{formatCount(summary.requestsTotal)}</dd>
              <dt className="font-medium">Request log records dropped</dt>
              <dd data-testid="log-dropped">{formatCount(summary.logDropped)}</dd>
            </dl>
            <TableScroll>
              <table className="w-auto border-collapse text-left text-sm">
                <caption className="sr-only">Requests by source</caption>
                <thead className="border-b border-slate-300 dark:border-slate-700 text-slate-700 dark:text-slate-300">
                  <tr>
                    <th scope="col" className="px-3 py-2 font-semibold">
                      Source
                    </th>
                    <th scope="col" className="px-3 py-2 font-semibold">
                      Requests
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(summary.bySource).length === 0 ? (
                    <tr>
                      <td colSpan={2} className="px-3 py-2 text-slate-600 dark:text-slate-400">
                        No requests served yet.
                      </td>
                    </tr>
                  ) : (
                    Object.entries(summary.bySource).map(([source, n]) => (
                      <tr key={source} className="border-b border-slate-200 dark:border-slate-800">
                        <th scope="row" className="px-3 py-2 font-normal">
                          <SourceName source={source} />
                        </th>
                        <td className="px-3 py-2">{formatCount(n)}</td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </TableScroll>
          </>
        ) : metrics.isPending ? (
          <p className="text-slate-700 dark:text-slate-300">Loading…</p>
        ) : null}
      </div>
    </section>
  );
}
