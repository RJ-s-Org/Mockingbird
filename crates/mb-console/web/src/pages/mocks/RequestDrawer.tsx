import type { LiveRequestView } from "../../api/sse";
import { Drawer } from "../../components/Drawer";
import { preClass } from "../../components/ui";

/** One served call: its summary and the recorded request and response. */
export function RequestDrawer({ record, onClose }: { record: LiveRequestView; onClose: () => void }) {
  const facts: [string, string][] = [
    ["Time", new Date(record.at).toLocaleString()],
    ["Status", String(record.status)],
    ["Operation", record.operation_id ?? "—"],
    ["Source", record.source ?? "—"],
    ["Latency", `${record.latency_ms} ms`],
    ["Session", record.session],
    ["Tokens", `${record.prompt_tokens} prompt, ${record.completion_tokens} completion`],
  ];
  return (
    <Drawer title={`${record.method} ${record.path}`} onClose={onClose}>
      <div className="space-y-6">
        <dl className="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-2 rounded-lg border border-line bg-surface-2/40 p-4 text-sm">
          {facts.map(([term, value]) => (
            <div key={term} className="contents">
              <dt className="text-fg-muted">{term}</dt>
              <dd className="font-medium break-all">{value}</dd>
            </div>
          ))}
        </dl>
        <section className="space-y-2">
          <h3 className="text-sm font-semibold">Request</h3>
          <pre tabIndex={0} className={preClass}>
            {JSON.stringify(record.request, null, 2)}
          </pre>
        </section>
        <section className="space-y-2">
          <h3 className="text-sm font-semibold">Response</h3>
          <pre tabIndex={0} className={preClass}>
            {JSON.stringify(record.response ?? null, null, 2)}
          </pre>
        </section>
      </div>
    </Drawer>
  );
}
