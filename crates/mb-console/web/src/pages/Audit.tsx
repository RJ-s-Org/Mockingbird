import { createContext, useCallback, useContext, useMemo, useState, type FormEvent } from "react";
import { auditExportUrl, useAudit, useGlobalAudit, type AuditEvent, type AuditFilters } from "../api/queries";
import { useMe } from "../auth/me";
import { inputClass, primaryButton, secondaryButton } from "../components/ConfirmDialog";
import { DataTable, columnHelper } from "../components/DataTable";
import { ProblemAlert } from "../components/ProblemAlert";
import { linkClass } from "./mocks/MocksList";

const col = columnHelper<AuditEvent>();

function pretty(v: unknown): string {
  return v == null ? "—" : JSON.stringify(v, null, 2);
}

/** An event's before and after views: side by side on wide screens, wrapping long lines. */
function Changes({ event }: { event: AuditEvent }) {
  const pane =
    "max-h-80 overflow-auto rounded-md border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-2 font-mono text-xs whitespace-pre-wrap break-all";
  return (
    <div className="grid min-w-0 grid-cols-1 gap-2 md:grid-cols-2">
      <div className="min-w-0">
        <p className="mb-1 text-xs font-semibold">Before</p>
        <pre role="region" aria-label="Before" tabIndex={0} className={pane}>
          {pretty(event.before)}
        </pre>
      </div>
      <div className="min-w-0">
        <p className="mb-1 text-xs font-semibold">After</p>
        <pre role="region" aria-label="After" tabIndex={0} className={pane}>
          {pretty(event.after)}
        </pre>
      </div>
    </div>
  );
}

/** Which events have their changes shown, for the toggles inside the table. */
const Expansion = createContext<{ open: ReadonlySet<number>; toggle: (id: number) => void }>({
  open: new Set(),
  toggle: () => {},
});

/** Shows or hides an event's changes (in a full-width row under it). */
function ChangesToggle({ event }: { event: AuditEvent }) {
  const expansion = useContext(Expansion);
  if (event.before == null && event.after == null) return <>—</>;
  const open = expansion.open.has(event.id);
  const onToggle = () => expansion.toggle(event.id);
  return (
    <button
      type="button"
      aria-expanded={open}
      aria-label={`${open ? "Hide" : "Show"} changes of ${event.action} on ${event.target}`}
      className="text-sm whitespace-nowrap text-indigo-700 dark:text-indigo-300 underline hover:text-indigo-900 dark:hover:text-indigo-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700 dark:focus-visible:outline-indigo-400"
      onClick={onToggle}
    >
      {open ? "Hide changes" : "Changes"}
    </button>
  );
}

/** An event's time on one line; the tooltip carries the full ISO timestamp. */
function EventTime({ at }: { at: string }) {
  return (
    <time dateTime={at} title={at} className="whitespace-nowrap">
      {new Date(at).toLocaleString()}
    </time>
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
  const [open, setOpen] = useState<ReadonlySet<number>>(new Set());
  const toggle = useCallback(
    (id: number) =>
      setOpen((current) => {
        const next = new Set(current);
        if (!next.delete(id)) next.add(id);
        return next;
      }),
    [],
  );
  const expansion = useMemo(() => ({ open, toggle }), [open, toggle]);

  const apply = (e: FormEvent) => {
    e.preventDefault();
    setFilters({ action: action.trim(), actor: actor.trim() });
  };

  // The columns only change with the workspace scope; which events are open reaches
  // the Changes toggles by context, so opening one re-renders only the toggles.
  const columns = useMemo(
    () => [
      col.accessor("at", { header: "Time", cell: (info) => <EventTime at={info.getValue()} /> }),
      ...(allWorkspaces
        ? [col.accessor("workspace", { header: "Workspace", cell: (info) => info.getValue() ?? "—" })]
        : []),
      col.accessor("actor_subject", {
        header: "Actor",
        cell: (info) => (
          <>
            <span>{info.getValue()}</span> <span className="text-slate-600 dark:text-slate-400">({info.row.original.actor_kind})</span>
          </>
        ),
      }),
      col.accessor("action", { header: "Action", cell: (info) => <code>{info.getValue()}</code> }),
      col.accessor("target", { header: "Target", cell: (info) => <code>{info.getValue()}</code> }),
      col.display({
        id: "changes",
        header: "Changes",
        cell: ({ row }) => <ChangesToggle event={row.original} />,
      }),
    ],
    [allWorkspaces],
  );

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
        <Expansion.Provider value={expansion}>
          <DataTable
            caption="Audit events"
            columns={columns}
            data={events}
            empty="No audit events."
            expanded={(event) => (open.has(event.id) ? <Changes event={event} /> : null)}
          />
        </Expansion.Provider>
      ) : audit.isPending ? (
        <p className="text-slate-700 dark:text-slate-300">Loading…</p>
      ) : null}
      {audit.data && events.length > 0 ? (
        <div className="flex flex-wrap items-center gap-3">
          <p className="text-sm text-slate-700 dark:text-slate-300" aria-live="polite">
            {audit.hasNextPage
              ? `Showing the latest ${events.length} events.`
              : `Showing all ${events.length} ${events.length === 1 ? "event" : "events"}.`}
          </p>
          {audit.hasNextPage ? (
            <button
              type="button"
              className={secondaryButton}
              disabled={audit.isFetchingNextPage}
              onClick={() => void audit.fetchNextPage()}
            >
              {audit.isFetchingNextPage ? "Loading…" : "Load more"}
            </button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}
