import { useState, type FormEvent } from "react";
import { auditExportUrl, useAudit, useGlobalAudit, type AuditEvent, type AuditFilters } from "../api/queries";
import { useMe } from "../auth/me";
import { inputClass, primaryButton, secondaryButton } from "../components/ConfirmDialog";
import { DataTable, columnHelper } from "../components/DataTable";
import { ProblemAlert } from "../components/ProblemAlert";
import { linkClass } from "./mocks/MocksList";
import { DateTime } from "./ApiKeys";

const col = columnHelper<AuditEvent>();

function pretty(v: unknown): string {
  return v == null ? "—" : JSON.stringify(v, null, 2);
}

/** An event's before and after views, side by side. */
function Changes({ event }: { event: AuditEvent }) {
  if (event.before == null && event.after == null) return <>—</>;
  const pane = "max-h-80 overflow-auto rounded-md border border-slate-300 bg-slate-50 p-2 font-mono text-xs";
  return (
    <details>
      <summary className="cursor-pointer text-indigo-700 underline">Changes</summary>
      <div className="mt-2 grid min-w-[32rem] grid-cols-2 gap-2">
        <div>
          <p className="mb-1 text-xs font-semibold">Before</p>
          <pre role="region" aria-label="Before" tabIndex={0} className={pane}>
            {pretty(event.before)}
          </pre>
        </div>
        <div>
          <p className="mb-1 text-xs font-semibold">After</p>
          <pre role="region" aria-label="After" tabIndex={0} className={pane}>
            {pretty(event.after)}
          </pre>
        </div>
      </div>
    </details>
  );
}

/** The audit log of a workspace, or of every workspace for a sysadmin. */
export function Audit({ ws }: { ws: string }) {
  const me = useMe();
  const sysadmin = me.data?.sysadmin ?? false;
  const [global, setGlobal] = useState(false);
  const [action, setAction] = useState("");
  const [actor, setActor] = useState("");
  const [filters, setFilters] = useState<AuditFilters>({});
  const allWorkspaces = sysadmin && global;
  const local = useAudit(ws, filters, !allWorkspaces);
  const everywhere = useGlobalAudit(filters, allWorkspaces);
  const audit = allWorkspaces ? everywhere : local;
  const events = audit.data?.pages.flat() ?? [];

  const apply = (e: FormEvent) => {
    e.preventDefault();
    setFilters({ action: action.trim(), actor: actor.trim() });
  };

  const columns = [
    col.accessor("at", { header: "Time", cell: (info) => <DateTime value={info.getValue()} /> }),
    ...(allWorkspaces
      ? [col.accessor("workspace", { header: "Workspace", cell: (info) => info.getValue() ?? "—" })]
      : []),
    col.accessor("actor_subject", {
      header: "Actor",
      cell: (info) => (
        <>
          <span>{info.getValue()}</span> <span className="text-slate-600">({info.row.original.actor_kind})</span>
        </>
      ),
    }),
    col.accessor("action", { header: "Action", cell: (info) => <code>{info.getValue()}</code> }),
    col.accessor("target", { header: "Target", cell: (info) => <code>{info.getValue()}</code> }),
    col.display({ id: "changes", header: "Changes", cell: ({ row }) => <Changes event={row.original} /> }),
  ];

  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h1 className="text-2xl font-semibold">Audit</h1>
        <a href={auditExportUrl(allWorkspaces ? null : ws, filters)} download className={linkClass}>
          Export JSONL
        </a>
      </div>
      <form onSubmit={apply} className="flex flex-wrap items-end gap-3" aria-label="Audit filters">
        <div className="space-y-1">
          <label htmlFor="audit-action" className="block text-sm font-medium">
            Action
          </label>
          <input
            id="audit-action"
            className={inputClass}
            value={action}
            placeholder="mock.update"
            onChange={(e) => setAction(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <label htmlFor="audit-actor" className="block text-sm font-medium">
            Actor
          </label>
          <input id="audit-actor" className={inputClass} value={actor} onChange={(e) => setActor(e.target.value)} />
        </div>
        <button type="submit" className={primaryButton}>
          Apply
        </button>
        {sysadmin ? (
          <div className="flex items-center gap-2 pb-1.5">
            <input
              id="audit-global"
              type="checkbox"
              checked={global}
              onChange={(e) => setGlobal(e.target.checked)}
            />
            <label htmlFor="audit-global" className="text-sm font-medium">
              All workspaces
            </label>
          </div>
        ) : null}
      </form>
      {audit.error ? <ProblemAlert error={audit.error} /> : null}
      {audit.data ? (
        <div className="overflow-x-auto">
          <DataTable caption="Audit events" columns={columns} data={events} empty="No audit events." />
        </div>
      ) : audit.isPending ? (
        <p className="text-slate-700">Loading…</p>
      ) : null}
      {audit.hasNextPage ? (
        <button
          type="button"
          className={secondaryButton}
          disabled={audit.isFetchingNextPage}
          onClick={() => void audit.fetchNextPage()}
        >
          Load more
        </button>
      ) : null}
    </section>
  );
}
