import { useMetricsSummary, useReadiness } from "../api/queries";
import { ProblemAlert } from "../components/ProblemAlert";

const STATUS_STYLE: Record<string, string> = {
  ready: "border-green-300 bg-green-50 text-green-900",
  degraded: "border-amber-300 bg-amber-50 text-amber-900",
};

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
                  STATUS_STYLE[ready.data.status] ?? "border-red-300 bg-red-50 text-red-900"
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
          <p className="text-slate-700">Loading…</p>
        ) : null}
      </div>
      <div className="space-y-2">
        <h2 className="text-lg font-semibold">Requests</h2>
        {metrics.error ? <ProblemAlert error={metrics.error} /> : null}
        {metrics.data === "private" ? (
          <p className="rounded-md border border-slate-300 bg-white p-3 text-sm text-slate-800">Metrics are private</p>
        ) : null}
        {summary ? (
          <>
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
              <dt className="font-medium">Requests served</dt>
              <dd data-testid="requests-total">{formatCount(summary.requestsTotal)}</dd>
              <dt className="font-medium">Request log records dropped</dt>
              <dd data-testid="log-dropped">{formatCount(summary.logDropped)}</dd>
            </dl>
            <table className="w-auto border-collapse text-left text-sm">
              <caption className="sr-only">Requests by source</caption>
              <thead className="border-b border-slate-300 text-slate-700">
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
                    <td colSpan={2} className="px-3 py-2 text-slate-600">
                      No requests served yet.
                    </td>
                  </tr>
                ) : (
                  Object.entries(summary.bySource).map(([source, n]) => (
                    <tr key={source} className="border-b border-slate-200">
                      <th scope="row" className="px-3 py-2 font-normal">
                        {source}
                      </th>
                      <td className="px-3 py-2">{formatCount(n)}</td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </>
        ) : metrics.isPending ? (
          <p className="text-slate-700">Loading…</p>
        ) : null}
      </div>
    </section>
  );
}
