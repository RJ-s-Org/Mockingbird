import { Link, Outlet } from "@tanstack/react-router";
import { useEffect } from "react";
import { rememberWorkspace, useWorkspaces } from "../api/queries";
import { useMe } from "../auth/me";
import { ProblemAlert } from "./ProblemAlert";
import { WorkspaceSwitcher } from "./WorkspaceSwitcher";

const navLink =
  "rounded-md px-3 py-1.5 text-sm font-medium whitespace-nowrap text-slate-800 hover:bg-slate-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700";
const activeNavLink = "bg-slate-200 text-slate-950";

/**
 * The console frame: header (switcher, user menu), workspace navigation and the page.
 * `global` pages (Status) are outside any workspace: `ws` is then only the
 * switcher's preselection, and an unknown one is not an error.
 */
export function Layout({ ws, global = false }: { ws?: string; global?: boolean }) {
  const me = useMe();
  const workspaces = useWorkspaces();
  const known = ws !== undefined && workspaces.data ? workspaces.data.some((w) => w.name === ws) : undefined;
  // A workspace route whose workspace the list does not have.
  const missing = !global && ws !== undefined && known === false;
  const current = global && known === false ? undefined : ws;
  const scoped = ws !== undefined && !missing && !(global && known === false);

  useEffect(() => {
    if (ws && known) rememberWorkspace(ws);
  }, [ws, known]);

  return (
    <div className="min-h-screen bg-slate-50 text-slate-900">
      <header className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 bg-slate-900 px-4 py-3 text-white sm:px-6">
        <div className="flex min-w-0 flex-wrap items-center gap-x-6 gap-y-2">
          <span className="text-lg font-semibold">Mockingbird</span>
          <WorkspaceSwitcher current={current} missing={missing} sysadmin={me.data?.sysadmin ?? false} />
        </div>
        {me.data ? <UserMenu name={me.data.display_name} local={me.data.kind === "local"} /> : null}
      </header>
      <nav aria-label="Main" className="flex flex-wrap gap-1 border-b border-slate-300 bg-white px-4 py-2 sm:px-6">
        {scoped && ws ? (
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
      <main className="mx-auto max-w-6xl px-4 py-6 sm:px-6">
        {me.error ? (
          <ProblemAlert error={me.error} />
        ) : missing && ws ? (
          <WorkspaceNotFound ws={ws} fallback={pickFallback(workspaces.data ?? [])} />
        ) : (
          <Outlet />
        )}
      </main>
    </div>
  );
}

/** Where "back" leads from an unknown workspace: `default` if it exists, else the first one. */
function pickFallback(workspaces: { name: string; display_name: string }[]) {
  return workspaces.find((w) => w.name === "default") ?? workspaces[0];
}

/** Shown instead of any workspace page when the workspace does not exist (or is not visible). */
function WorkspaceNotFound({ ws, fallback }: { ws: string; fallback?: { name: string; display_name: string } }) {
  return (
    <section aria-labelledby="ws-not-found" className="mx-auto max-w-xl space-y-3 py-10">
      <h1 id="ws-not-found" className="text-2xl font-semibold">
        Workspace not found
      </h1>
      <p className="text-slate-700">
        There is no workspace named <code className="font-semibold break-all">{ws}</code>, or you do not have access to
        it.
      </p>
      {fallback ? (
        <Link
          to="/w/$ws/mocks"
          params={{ ws: fallback.name }}
          className="font-medium text-indigo-700 underline hover:text-indigo-900 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700"
        >
          {`Go to the ${fallback.display_name} workspace`}
        </Link>
      ) : (
        <Link
          to="/"
          className="font-medium text-indigo-700 underline hover:text-indigo-900 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700"
        >
          Back to the console
        </Link>
      )}
    </section>
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
