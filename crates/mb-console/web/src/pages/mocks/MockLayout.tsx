import { Link, Outlet } from "@tanstack/react-router";
import { useMock } from "../../api/queries";
import { ProblemAlert } from "../../components/ProblemAlert";
import { SourceBadge } from "../../components/SourceBadge";

const tabClass =
  "shrink-0 border-b-2 border-transparent px-3 py-2 text-sm font-medium whitespace-nowrap text-slate-700 dark:text-slate-300 hover:text-slate-950 dark:hover:text-white focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700 dark:focus-visible:outline-indigo-400";
const activeTab = "border-indigo-700 dark:border-indigo-400 text-slate-950 dark:text-white";

const TABS = [
  { to: "/w/$ws/mocks/$mock/overview", label: "Overview" },
  { to: "/w/$ws/mocks/$mock/contracts", label: "Contracts" },
  { to: "/w/$ws/mocks/$mock/import", label: "Import" },
  { to: "/w/$ws/mocks/$mock/scenarios", label: "Scenarios" },
  { to: "/w/$ws/mocks/$mock/sessions", label: "Sessions" },
  { to: "/w/$ws/mocks/$mock/requests", label: "Live requests" },
  { to: "/w/$ws/mocks/$mock/channels", label: "Channels" },
] as const;

/** A mock's header (name, source, active version) and its tabs. */
export function MockLayout({ ws, mock }: { ws: string; mock: string }) {
  const m = useMock(ws, mock);
  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-2xl font-semibold break-all">{mock}</h1>
        {m.data ? (
          <>
            <SourceBadge source={m.data.source} />
            <span className="text-sm text-slate-700 dark:text-slate-300">
              {m.data.active_version == null ? "No active version" : `Active version v${m.data.active_version}`}
            </span>
          </>
        ) : null}
      </div>
      {/* One row that scrolls sideways on narrow screens; the inset shadow is the baseline under the tabs. */}
      <nav aria-label="Mock" className="flex gap-1 overflow-x-auto shadow-[inset_0_-1px_0_var(--color-slate-300)] dark:shadow-[inset_0_-1px_0_var(--color-slate-700)]">
        {TABS.map((t) => (
          <Link key={t.to} to={t.to} params={{ ws, mock }} className={tabClass} activeProps={{ className: activeTab }}>
            {t.label}
          </Link>
        ))}
      </nav>
      {m.error ? <ProblemAlert error={m.error} /> : <Outlet />}
    </section>
  );
}
