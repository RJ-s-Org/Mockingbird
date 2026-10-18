import type { DraftView } from "../../api/imports";

type Diff = NonNullable<DraftView["diff"]>;

function TargetList({ label, targets }: { label: string; targets: string[] }) {
  return (
    <div className="space-y-1">
      <h4 className="text-sm font-semibold">{label}</h4>
      {targets.length === 0 ? (
        <p className="text-sm text-slate-600 dark:text-slate-400">None</p>
      ) : (
        <ul role="list" aria-label={label} className="space-y-0.5 text-sm">
          {targets.map((t) => (
            <li key={t}>
              <code>{t}</code>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** What a draft changes compared with the active version: counts, with the targets in a disclosure. */
export function DraftDiff({ diff }: { diff: Diff }) {
  return (
    <details className="rounded-md border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-3">
      <summary className="cursor-pointer text-sm font-medium">
        {`Compared with v${diff.against_version}: ${diff.added.length} added, ${diff.removed.length} removed, ${diff.changed.length} changed`}
      </summary>
      <div className="mt-3 grid gap-4 sm:grid-cols-3">
        <TargetList label="Added" targets={diff.added} />
        <TargetList label="Removed" targets={diff.removed} />
        <TargetList label="Changed" targets={diff.changed} />
      </div>
    </details>
  );
}
