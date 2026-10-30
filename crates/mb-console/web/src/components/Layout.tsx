import { Link, Outlet } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { signOut, signOutEverywhere } from "../api/client";
import { rememberWorkspace, useWorkspaces } from "../api/queries";
import { useMe } from "../auth/me";
import { useTheme, type Theme } from "../lib/theme";
import { ProblemAlert } from "./ProblemAlert";
import { focusRing, linkClass, secondaryButton } from "./ui";
import { WorkspaceSwitcher } from "./WorkspaceSwitcher";

const navLink = `shrink-0 border-b-2 border-transparent px-3 py-2.5 text-sm font-medium whitespace-nowrap text-fg-muted transition-colors hover:border-line-strong hover:text-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-ring`;
const activeNavLink = "border-accent-solid! text-fg";

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
    <div className="min-h-screen bg-canvas text-fg">
      {/* Sticky from `sm` up; on a phone the header scrolls away instead of covering the page. */}
      <header className="border-b border-line bg-surface sm:sticky sm:top-0 sm:z-20 sm:bg-surface/95 sm:backdrop-blur sm:supports-[backdrop-filter]:bg-surface/80">
        <div className="mx-auto max-w-6xl px-4 sm:px-6">
          {/* Phone: brand and account controls on one row, the switcher below; wider: all on one row. */}
          <div className="flex flex-wrap items-center gap-x-4 gap-y-2 py-2.5">
            <Brand />
            <span aria-hidden="true" className="hidden h-5 w-px bg-line sm:block" />
            <div className="order-last w-full min-w-0 sm:order-none sm:w-auto">
              <WorkspaceSwitcher current={current} missing={missing} sysadmin={me.data?.sysadmin ?? false} />
            </div>
            <div className="ml-auto flex min-w-0 items-center gap-2">
              <ThemeSelect />
              {me.data ? <UserMenu name={me.data.display_name} subject={me.data.subject} local={me.data.kind === "local"} /> : null}
            </div>
          </div>
          <nav aria-label="Main" className="-mx-1 flex gap-1 overflow-x-auto">
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
                <Link
                  to="/w/$ws/api-keys"
                  params={{ ws }}
                  className={navLink}
                  activeProps={{ className: activeNavLink }}
                >
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
        </div>
      </header>
      <main className="mx-auto max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
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

/** The product mark and name; leads to the remembered workspace. */
function Brand() {
  return (
    <Link to="/" className={`flex items-center gap-2 rounded-md text-base font-semibold tracking-tight ${focusRing}`}>
      <span aria-hidden="true" className="grid size-7 place-items-center rounded-md bg-accent-solid text-white shadow-sm">
        <svg viewBox="0 0 24 24" className="size-4 fill-current">
          <path d="M21 5.5c-1.6.2-2.9-.3-4-1.3-1.9-1.7-4.9-1.3-6.3.9L3 17.5l1.4 1.4 3.1-2.3c.3 1.7 1.3 3 2.9 3.4l-.4 2h2l.3-1.7h1.4l.3 1.7h2l-.5-2.4c2.3-1.2 3.8-3.6 3.8-6.3V9.6L22 7l-1-1.5Zm-5.5 2.2a1.1 1.1 0 1 1 0 2.2 1.1 1.1 0 0 1 0-2.2Z" />
        </svg>
      </span>
      Mockingbird
    </Link>
  );
}

/** Where "back" leads from an unknown workspace: `default` if it exists, else the first one. */
function pickFallback(workspaces: { name: string; display_name: string }[]) {
  return workspaces.find((w) => w.name === "default") ?? workspaces[0];
}

/** Shown instead of any workspace page when the workspace does not exist (or is not visible). */
function WorkspaceNotFound({ ws, fallback }: { ws: string; fallback?: { name: string; display_name: string } }) {
  return (
    <div className="mx-auto max-w-xl space-y-3 py-12 text-center">
      <h1 className="text-2xl font-semibold tracking-tight">Workspace not found</h1>
      <p className="text-fg-muted">
        There is no workspace named <code className="font-semibold break-all">{ws}</code>, or you do not have access to
        it.
      </p>
      {fallback ? (
        <Link
          to="/w/$ws/mocks"
          params={{ ws: fallback.name }}
          className={linkClass}
        >
          {`Go to the ${fallback.display_name} workspace`}
        </Link>
      ) : (
        <Link to="/" className={linkClass}>
          Back to the console
        </Link>
      )}
    </div>
  );
}

const headerControl = `rounded-md border border-line-strong bg-surface py-1 text-sm text-fg shadow-xs ${focusRing}`;

/** Light, dark or the system's theme; the choice is remembered in this browser. */
function ThemeSelect() {
  const [theme, setTheme] = useTheme();
  return (
    <div className="relative flex items-center">
      <label htmlFor="theme-select" className="sr-only">
        Theme
      </label>
      <svg
        aria-hidden="true"
        viewBox="0 0 20 20"
        className="pointer-events-none absolute left-2 size-4 fill-current text-fg-muted"
      >
        <path d="M10 2a8 8 0 1 0 0 16 8 8 0 0 0 0-16Zm0 2v12a6 6 0 0 1 0-12Z" />
      </svg>
      <select
        id="theme-select"
        className={`${headerControl} pr-2 pl-7`}
        value={theme}
        onChange={(e) => setTheme(e.target.value as Theme)}
      >
        <option value="system">System</option>
        <option value="light">Light</option>
        <option value="dark">Dark</option>
      </select>
    </div>
  );
}

/**
 * The signed-in user's name and, unless they are the local user, Sign out and
 * Sign out everywhere (which ends all of the user's console sessions first).
 */
function UserMenu({ name, subject, local }: { name: string; subject: string; local: boolean }) {
  const [failed, setFailed] = useState<unknown>(null);
  const onSignOut = async () => {
    window.location.href = await signOut();
  };
  const onSignOutEverywhere = async () => {
    try {
      window.location.href = await signOutEverywhere(subject);
    } catch (e) {
      setFailed(e);
    }
  };
  const initial = name.trim().charAt(0).toUpperCase() || "?";
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span className="flex min-w-0 items-center gap-2">
        <span
          aria-hidden="true"
          title={name}
          className="grid size-7 shrink-0 place-items-center rounded-full bg-accent-soft text-xs font-semibold text-accent"
        >
          {initial}
        </span>
        <span className="sr-only text-sm font-medium sm:not-sr-only sm:max-w-[10rem] sm:truncate">{name}</span>
      </span>
      {local ? null : (
        <>
          <button type="button" className={`${secondaryButton} py-1`} onClick={() => void onSignOut()}>
            Sign out
          </button>
          <button
            type="button"
            className={`${secondaryButton} py-1`}
            title="End your console sessions in every browser"
            onClick={() => void onSignOutEverywhere()}
          >
            Sign out everywhere
          </button>
          {failed ? (
            <span role="alert" className="text-sm text-bad-fg">
              Could not sign out everywhere
            </span>
          ) : null}
        </>
      )}
    </div>
  );
}
