import { Link, Outlet } from "@tanstack/react-router";
import { useEffect } from "react";
import { rememberWorkspace } from "../api/queries";
import { useMe } from "../auth/me";
import { ProblemAlert } from "./ProblemAlert";
import { WorkspaceSwitcher } from "./WorkspaceSwitcher";

const navLink =
  "rounded-md px-3 py-1.5 text-sm font-medium text-slate-800 hover:bg-slate-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700";
const activeNavLink = "bg-slate-200 text-slate-950";

/** The console frame: header (switcher, user menu), workspace navigation and the page. */
export function Layout({ ws }: { ws?: string }) {
  const me = useMe();

  useEffect(() => {
    if (ws) rememberWorkspace(ws);
  }, [ws]);

  return (
    <div className="min-h-screen bg-slate-50 text-slate-900">
      <header className="flex flex-wrap items-center justify-between gap-4 bg-slate-900 px-6 py-3 text-white">
        <div className="flex items-center gap-6">
          <span className="text-lg font-semibold">Mockingbird</span>
          <WorkspaceSwitcher current={ws} sysadmin={me.data?.sysadmin ?? false} />
        </div>
        {me.data ? <UserMenu name={me.data.display_name} local={me.data.kind === "local"} /> : null}
      </header>
      <nav aria-label="Main" className="flex flex-wrap gap-1 border-b border-slate-300 bg-white px-6 py-2">
        {ws ? (
          <>
            <Link to="/w/$ws/mocks" params={{ ws }} className={navLink} activeProps={{ className: activeNavLink }}>
              Mocks
            </Link>
            <Link
              to="/w/$ws/llm-profiles"
              params={{ ws }}
              className={navLink}
              activeProps={{ className: activeNavLink }}
            >
              LLM profiles
            </Link>
            <Link to="/w/$ws/api-keys" params={{ ws }} className={navLink} activeProps={{ className: activeNavLink }}>
              API keys
            </Link>
            <Link to="/w/$ws/audit" params={{ ws }} className={navLink} activeProps={{ className: activeNavLink }}>
              Audit
            </Link>
          </>
        ) : null}
        <Link to="/status" className={navLink} activeProps={{ className: activeNavLink }}>
          Status
        </Link>
      </nav>
      <main className="mx-auto max-w-6xl px-6 py-6">
        {me.error ? <ProblemAlert error={me.error} /> : <Outlet />}
      </main>
    </div>
  );
}

/** The signed-in user's name and, unless they are the local user, Sign out. */
function UserMenu({ name, local }: { name: string; local: boolean }) {
  const signOut = async () => {
    try {
      await fetch("/auth/logout", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      });
    } finally {
      window.location.href = "/auth/login?return_to=/";
    }
  };
  return (
    <div className="flex items-center gap-3">
      <span className="text-sm text-slate-200">{name}</span>
      {local ? null : (
        <button
          type="button"
          className="rounded-md border border-slate-400 px-2 py-1 text-sm text-white hover:bg-slate-700 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-white"
          onClick={() => void signOut()}
        >
          Sign out
        </button>
      )}
    </div>
  );
}
