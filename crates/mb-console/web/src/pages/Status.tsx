import { sourceRows, useMetricsSummary, useReadiness } from "../api/queries";
import { ProblemAlert } from "../components/ProblemAlert";
import { sourceLabel } from "../lib/sources";
import {
  TableScroll,
  emptyCellClass,
  tableClass,
  tdClass,
  thClass,
  theadClass,
  trClass,
} from "../components/DataTable";
import { Badge, Loading, Notice, PageHeader, cardClass, type Tone } from "../components/ui";

const STATUS_TONE: Record<string, Tone> = {
  ready: "ok",
  degraded: "warn",
};

/** A source's human label; the tooltip explains it and names the raw value. */
function SourceName({ source }: { source: string }) {
  const { label, hint } = sourceLabel(source);
  if (label === source) return <>{source}</>;
  return (
    <span
      title={`${hint} (source "${source}")`}
      className="cursor-help underline decoration-fg-muted decoration-dotted underline-offset-2"
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
      <PageHeader title="Status" description="Whether the server can serve mocks, and how many requests it has served." />
      <div className="space-y-3">
        <h2 className="text-base font-semibold">Readiness</h2>
        {ready.error ? <ProblemAlert error={ready.error} /> : null}
        {ready.data ? (
          <>
            <p>
              <Badge tone={STATUS_TONE[ready.data.status] ?? "bad"}>{ready.data.status}</Badge>
            </p>
            {ready.data.reasons.length > 0 ? (
              <ul aria-label="Reasons" className="list-disc space-y-0.5 pl-5 text-sm">
                {ready.data.reasons.map((r) => (
                  <li key={r}>{r}</li>
                ))}
              </ul>
            ) : null}
          </>
        ) : ready.isPending ? (
          <Loading />
        ) : null}
      </div>
      <div className="space-y-3">
        <h2 className="text-base font-semibold">Requests</h2>
        {metrics.error ? <ProblemAlert error={metrics.error} /> : null}
        {metrics.data === "private" ? <Notice>Metrics are private</Notice> : null}
        {summary ? (
          <>
            <dl className="grid grid-cols-1 gap-4 sm:grid-cols-2">
              <div className={`${cardClass} p-4`}>
                <dt className="text-sm text-fg-muted">Requests served</dt>
                <dd data-testid="requests-total" className="mt-1 text-2xl font-semibold tabular-nums">
                  {formatCount(summary.requestsTotal)}
                </dd>
              </div>
              <div className={`${cardClass} p-4`}>
                <dt className="text-sm text-fg-muted">Request log records dropped</dt>
                <dd data-testid="log-dropped" className="mt-1 text-2xl font-semibold tabular-nums">
                  {formatCount(summary.logDropped)}
                </dd>
              </div>
            </dl>
            <TableScroll>
              <table className={tableClass}>
                <caption className="sr-only">Requests by source</caption>
                <thead className={theadClass}>
                  <tr>
                    <th scope="col" className={thClass}>
                      Source
                    </th>
                    <th scope="col" className={`${thClass} text-right`}>
                      Requests
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(summary.bySource).length === 0 ? (
                    <tr>
                      <td colSpan={2} className={emptyCellClass}>
                        No requests served yet.
                      </td>
                    </tr>
                  ) : (
                    sourceRows(summary.bySource).map(([source, n]) => (
                      <tr key={source} className={trClass}>
                        <th scope="row" className={`${tdClass} font-normal`}>
                          <SourceName source={source} />
                        </th>
                        <td className={`${tdClass} text-right tabular-nums`}>{formatCount(n)}</td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </TableScroll>
          </>
        ) : metrics.isPending ? (
          <Loading />
        ) : null}
      </div>
    </section>
  );
}
