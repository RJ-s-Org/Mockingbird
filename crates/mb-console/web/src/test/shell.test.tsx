import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider, createMemoryHistory } from "@tanstack/react-router";
import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createAppRouter } from "../router";

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

function serve(routes: Record<string, unknown>) {
  const fetchMock = vi.fn(async (req: Request) => {
    const path = new URL(req.url).pathname;
    return path in routes ? json(routes[path]) : json({ title: "Not Found", status: 404, detail: path }, 404);
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

const workspaces = [
  { name: "default", display_name: "Default", created_at: "2026-10-16T00:00:00Z", role: "owner" },
  { name: "team", display_name: "Team", created_at: "2026-10-16T00:00:00Z", role: "owner" },
];

function renderAt(path: string) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createAppRouter({
    queryClient,
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

describe("shell", () => {
  it("shell renders workspace switcher and hides sign-out for local user", async () => {
    serve({
      "/api/v1/me": {
        subject: "local",
        display_name: "Local",
        kind: "local",
        sysadmin: true,
        workspaces: [{ name: "default", role: "owner" }],
      },
      "/api/v1/workspaces": workspaces,
    });
    renderAt("/w/default/mocks");

    const switcher = await screen.findByLabelText("Workspace");
    await within(switcher).findByRole("option", { name: "Team" });
    expect((switcher as HTMLSelectElement).value).toBe("default");
    expect(within(switcher).getAllByRole("option").map((o) => o.textContent)).toEqual(["Default", "Team"]);

    const nav = screen.getByRole("navigation", { name: "Main" });
    for (const name of ["Mocks", "LLM profiles", "API keys", "Audit", "Status"]) {
      expect(within(nav).getByRole("link", { name })).toBeTruthy();
    }
    // A sysadmin may create workspaces; the local user cannot sign out.
    expect(screen.getByRole("button", { name: "New workspace" })).toBeTruthy();
    await screen.findByText("Local");
    expect(screen.queryByRole("button", { name: "Sign out" })).toBeNull();
  });

  it("shows sign-out for a signed-in user and remembers the workspace", async () => {
    serve({
      "/api/v1/me": {
        subject: "alice",
        display_name: "Alice",
        kind: "user",
        sysadmin: false,
        workspaces: [{ name: "team", role: "viewer" }],
      },
      "/api/v1/workspaces": [workspaces[1]],
    });
    renderAt("/w/team/mocks");
    await screen.findByLabelText("Workspace");
    await screen.findByText("Alice");
    expect(screen.getByRole("button", { name: "Sign out" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "New workspace" })).toBeNull();
    expect(window.localStorage.getItem("mb.lastWorkspace")).toBe("team");
  });

  it("redirects / to the remembered workspace", async () => {
    serve({ "/api/v1/workspaces": workspaces, "/api/v1/me": { subject: "local", display_name: "Local", kind: "local", sysadmin: true, workspaces: [] } });
    window.localStorage.setItem("mb.lastWorkspace", "team");
    const router = renderAt("/");
    await screen.findByLabelText("Workspace");
    expect(router.state.location.pathname).toBe("/w/team/mocks");
  });

  it("an unknown workspace shows a not-found state without workspace actions", async () => {
    serve({
      "/api/v1/me": { subject: "local", display_name: "Local", kind: "local", sysadmin: true, workspaces: [] },
      "/api/v1/workspaces": workspaces,
      "/api/v1/workspaces/nowhere/mocks": [],
    });
    window.localStorage.setItem("mb.lastWorkspace", "team");
    renderAt("/w/nowhere/mocks");

    expect(await screen.findByRole("heading", { name: "Workspace not found" })).toBeTruthy();
    const back = screen.getByRole("link", { name: "Go to the Default workspace" });
    expect(back.getAttribute("href")).toBe("/w/default/mocks");
    // No workspace-scoped actions or navigation.
    expect(screen.queryByRole("button", { name: "New mock" })).toBeNull();
    const nav = screen.getByRole("navigation", { name: "Main" });
    expect(within(nav).queryByRole("link", { name: "Mocks" })).toBeNull();
    expect(within(nav).getByRole("link", { name: "Status" })).toBeTruthy();
    // The switcher names the missing workspace instead of showing another one.
    const switcher = screen.getByLabelText("Workspace") as HTMLSelectElement;
    expect(switcher.value).toBe("nowhere");
    expect(switcher.selectedOptions[0]?.textContent).toBe("nowhere (not found)");
    // An unknown workspace is never remembered.
    expect(window.localStorage.getItem("mb.lastWorkspace")).toBe("team");
  });
});
