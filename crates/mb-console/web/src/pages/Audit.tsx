import { createContext, useCallback, useContext, useMemo, useState, type FormEvent } from "react";
import { auditExportUrl, useAudit, useGlobalAudit, type AuditEvent, type AuditFilters } from "../api/queries";
import { useMe } from "../auth/me";
import { DataTable, columnHelper } from "../components/DataTable";
import { ProblemAlert } from "../components/ProblemAlert";
import { Checkbox, Field, Loading, PageHeader, cardClass, inputClass, linkClass, primaryButton, secondaryButton } from "../components/ui";

const col = columnHelper<AuditEvent>();

function pretty(v: unknown): string {
  return v == null ? "—" : JSON.stringify(v, null, 2);
}

/** An event's before and after views: side by side on wide screens, wrapping long lines. */
function Changes({ event }: { event: AuditEvent }) {
  const pane =
    "max-h-80 overflow-auto rounded-md border border-line bg-surface p-3 font-mono text-xs leading-relaxed whitespace-pre-wrap break-all text-fg";
  return (
    <div className="grid min-w-0 grid-cols-1 gap-2 md:grid-cols-2">
      <div className="min-w-0">
        <p className="mb-1 text-xs font-semibold text-fg-muted">Before</p>
        <pre role="region" aria-label="Before" tabIndex={0} className={pane}>
          {pretty(event.before)}
        </pre>
      </div>
      <div className="min-w-0">
        <p className="mb-1 text-xs font-semibold text-fg-muted">After</p>
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
      className={`${linkClass} text-sm whitespace-nowrap`}
      onClick={onToggle}
    >
      {open ? "Hide changes" : "Changes"}
    </button>
  );
}

/**
 * An event's time: one line from `sm` up, the date over the time on a phone;
 * the tooltip carries the full ISO timestamp.
 */
function EventTime({ at }: { at: string }) {
  const when = new Date(at);
  return (
    <time dateTime={at} title={at} className="sm:whitespace-nowrap">
      <span className="whitespace-nowrap">{when.toLocaleDateString()},</span>{" "}
      <span className="whitespace-nowrap">{when.toLocaleTimeString()}</span>
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
      col.accessor("action", {
        header: "Action",
        cell: (info) => <code className="whitespace-nowrap">{info.getValue()}</code>,
      }),
      col.accessor("actor_subject", {
        header: "Actor",
        cell: (info) => (
          <span className="whitespace-nowrap">
            {info.getValue()} <span className="text-fg-muted">({info.row.original.actor_kind})</span>
          </span>
        ),
      }),
      // Long targets (import job ids) are cut short on one line; the tooltip has them whole.
      col.accessor("target", {
        header: "Target",
        cell: (info) => (
          <code title={info.getValue()} className="block max-w-[14rem] truncate sm:max-w-[24rem]">
            {info.getValue()}
          </code>
        ),
      }),
      col.display({
        id: "changes",
        header: "Changes",
        cell: ({ row }) => <ChangesToggle event={row.original} />,
      }),
    ],
    [allWorkspaces],
  );

  return (
    <section className="space-y-6">
      <PageHeader
        title="Audit"
        description="Who changed what, and when."
        actions={
          <a href={auditExportUrl(allWorkspaces ? null : ws, filters)} download className={`${linkClass} text-sm`}>
            Export JSONL
          </a>
        }
      />
      <form onSubmit={apply} className={`${cardClass} flex flex-wrap items-end gap-4 p-4`} aria-label="Audit filters">
        <Field id="audit-action" label="Action">
          <input
            id="audit-action"
            className={inputClass}
            value={action}
            placeholder="mock.update"
            onChange={(e) => setAction(e.target.value)}
          />
        </Field>
        <Field id="audit-actor" label="Actor">
          <input id="audit-actor" className={inputClass} value={actor} onChange={(e) => setActor(e.target.value)} />
        </Field>
        <button type="submit" className={primaryButton}>
          Apply
        </button>
        {sysadmin ? (
          <div className="pb-1.5">
            <Checkbox id="audit-global" label="All workspaces" checked={global} onChange={setGlobal} />
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
        <Loading />
      ) : null}
      {audit.data && events.length > 0 ? (
        <div className="flex flex-wrap items-center gap-3">
          <p className="text-sm text-fg-muted" aria-live="polite">
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
