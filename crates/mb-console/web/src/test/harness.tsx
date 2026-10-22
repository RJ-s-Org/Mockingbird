import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider, createMemoryHistory } from "@tanstack/react-router";
import { render } from "@testing-library/react";
import { vi } from "vitest";
import type { Me } from "../auth/me";
import { createAppRouter } from "../router";

export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

export function problem(status: number, title: string, detail: string): Response {
  return new Response(JSON.stringify({ type: "about:blank", title, status, detail }), {
    status,
    headers: { "Content-Type": "application/problem+json" },
  });
}

/** A route's answer: a JSON body, or a function of the request. */
export type Answer = unknown | ((req: Request) => Response | Promise<Response>);

/** A recorded call: method, path, query string, headers and body text. */
export interface Call {
  method: string;
  path: string;
  search: string;
  contentType: string | null;
  body: string;
}

/**
 * Stubs `fetch` with `routes`, keyed `"<METHOD> <path>"`; anything else is a
 * 404 problem. Returns every call made, in order.
 */
export function serve(routes: Record<string, Answer>): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (req: Request) => {
      const url = new URL(req.url);
      const body = await req.clone().text();
      calls.push({
        method: req.method,
        path: url.pathname,
        search: url.search,
        contentType: req.headers.get("Content-Type"),
        body,
      });
      const key = `${req.method} ${url.pathname}`;
      if (!(key in routes)) return problem(404, "Not Found", key);
      const answer = routes[key];
      return typeof answer === "function" ? answer(req) : json(answer);
    }),
  );
  return calls;
}

/** The answers every console page needs: `me` and the workspace list. */
export function shell(me: Me): Record<string, Answer> {
  return {
    "GET /api/v1/me": me,
    "GET /api/v1/workspaces": me.workspaces.map((w) => ({
      name: w.name,
      display_name: w.name,
      created_at: "2026-10-16T00:00:00Z",
      role: w.role,
    })),
  };
}

export const localMe: Me = {
  subject: "local",
  display_name: "Local",
  kind: "local",
  sysadmin: true,
  workspaces: [{ name: "default", role: "owner" }],
};

export const viewerMe: Me = {
  subject: "vera",
  display_name: "Vera",
  kind: "user",
  sysadmin: false,
  workspaces: [{ name: "default", role: "viewer" }],
};

/** Renders the console at `path`, with retries off. */
export function renderAt(path: string) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createAppRouter({ queryClient, history: createMemoryHistory({ initialEntries: [path] }) });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

/** A mock as `GET …/mocks/{mock}` returns it. */
export function mockView(name: string, overrides: Record<string, unknown> = {}) {
  return {
    id: `00000000-0000-0000-0000-${name.padStart(12, "0").slice(-12)}`,
    workspace: "default",
    name,
    route_prefix: `/${name}`,
    persona: null,
    llm_profile: null,
    header_overrides: false,
    data_plane_auth: { mode: "none", header: "x-api-key" },
    hosts: [],
    source: "api",
    active_version: null,
    created_at: "2026-10-16T00:00:00Z",
    updated_at: "2026-10-16T00:00:00Z",
    ...overrides,
  };
}
