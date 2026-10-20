import { Link, Outlet } from "@tanstack/react-router";
import { useMock } from "../../api/queries";
import { ProblemAlert } from "../../components/ProblemAlert";
import { SourceBadge } from "../../components/SourceBadge";
import { Badge, focusRing } from "../../components/ui";

const tabClass = `shrink-0 border-b-2 border-transparent px-3 py-2 text-sm font-medium whitespace-nowrap text-fg-muted transition-colors hover:border-line-strong hover:text-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-ring`;
const activeTab = "border-accent-solid! text-fg";

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
    <section className="space-y-6">
      <div className="space-y-4">
        <div className="space-y-2">
          <Link
            to="/w/$ws/mocks"
            params={{ ws }}
            className={`inline-flex items-center gap-1 rounded-sm text-sm text-fg-muted hover:text-fg ${focusRing}`}
          >
            <span aria-hidden="true">←</span> All mocks
          </Link>
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
            <h1 className="text-2xl font-semibold tracking-tight break-all">{mock}</h1>
            {m.data ? (
              <>
                <SourceBadge source={m.data.source} />
                {m.data.active_version == null ? (
                  <Badge tone="warn">No active version</Badge>
                ) : (
                  <Badge tone="ok">{`Active version v${m.data.active_version}`}</Badge>
                )}
                <code className="text-fg-muted">{m.data.route_prefix}</code>
              </>
            ) : null}
          </div>
        </div>
        {/* One row that scrolls sideways on narrow screens; the inset shadow is the baseline under the tabs. */}
        <nav aria-label="Mock" className="flex gap-1 overflow-x-auto shadow-[inset_0_-1px_0_var(--color-line)]">
          {TABS.map((t) => (
            <Link key={t.to} to={t.to} params={{ ws, mock }} className={tabClass} activeProps={{ className: activeTab }}>
              {t.label}
            </Link>
          ))}
        </nav>
      </div>
      {m.error ? <ProblemAlert error={m.error} /> : <Outlet />}
    </section>
  );
}
