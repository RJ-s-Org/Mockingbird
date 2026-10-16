import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, screen, waitFor, within } from "@testing-library/react";
import type { ReactNode } from "react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { useCreateApiKey } from "../api/queries";
import type { Me } from "../auth/me";
import { json, localMe, renderAt, serve, shell, viewerMe } from "./harness";

const BASE = "/api/v1/workspaces/default";
const TOKEN = "mbk_abcdefghijkl_c2VjcmV0LXNlY3JldC1zZWNyZXQtc2VjcmV0LXNlY3JldA";

const adminMe: Me = {
  subject: "ada",
  display_name: "Ada",
  kind: "user",
  sysadmin: false,
  workspaces: [{ name: "default", role: "admin" }],
};

function keyView(overrides: Record<string, unknown> = {}) {
  return {
    name: "ci",
    key_id: "abcdefghijkl",
    role: "editor",
    scopes: ["admin"],
    sysadmin: false,
    created_by: "local",
    created_at: "2026-10-16T08:00:00Z",
    expires_at: null,
    last_used_at: null,
    revoked_at: null,
    ...overrides,
  };
}

describe("api keys", () => {
  it("the created token does not linger in the mutation cache", async () => {
    serve({ [`POST ${BASE}/api-keys`]: () => json({ token: TOKEN, key: keyView() }, 201) });
    const queryClient = new QueryClient();
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    );
    const { result, unmount } = renderHook(() => useCreateApiKey("default"), { wrapper });
    await act(async () => {
      await result.current.mutateAsync({ name: "ci", role: "editor", scopes: ["admin"] });
    });
    expect(JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state.data))).toContain(TOKEN);
    // Closing the dialog resets the mutation and unmounts its observer.
    act(() => result.current.reset());
    unmount();
    await waitFor(() => expect(queryClient.getMutationCache().getAll()).toHaveLength(0));
  });

  it("lists keys with their status", async () => {
    serve({
      ...shell(localMe),
      [`GET ${BASE}/api-keys`]: [
        keyView(),
        keyView({ name: "old", key_id: "bbbbbbbbbbbb", revoked_at: "2026-10-16T09:00:00Z" }),
        keyView({ name: "gone", key_id: "cccccccccccc", expires_at: "2020-01-01T00:00:00Z" }),
      ],
    });
    renderAt("/w/default/api-keys");

    const table = await screen.findByRole("table", { name: "API keys" });
    const headers = within(table)
      .getAllByRole("columnheader")
      .map((h) => h.textContent);
    expect(headers.slice(0, 9)).toEqual([
      "Name",
      "Key id",
      "Role",
      "Scopes",
      "Created by",
      "Created",
      "Expires",
      "Last used",
      "Status",
    ]);
    const row = (name: string) => within(table).getByRole("cell", { name }).closest("tr") as HTMLElement;
    expect(within(row("ci")).getByText("active")).toBeTruthy();
    expect(within(row("old")).getByText("revoked")).toBeTruthy();
    expect(within(row("gone")).getByText("expired")).toBeTruthy();
    expect(within(row("ci")).getByRole("button", { name: "Revoke ci" })).toBeTruthy();
    expect(within(row("old")).queryByRole("button", { name: /Revoke/ })).toBeNull();
  });

  it("create key shows token once and clears on close", async () => {
    const user = userEvent.setup();
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/api-keys`]: [],
      [`POST ${BASE}/api-keys`]: () =>
        json({ token: TOKEN, key: keyView({ scopes: ["data-plane"], expires_at: "2027-01-31T23:59:59Z" }) }, 201),
    });
    renderAt("/w/default/api-keys");

    await user.click(await screen.findByRole("button", { name: "New API key" }));
    const dialog = await screen.findByRole("dialog", { name: "New API key" });
    await user.type(within(dialog).getByLabelText("Name"), "ci");
    await user.selectOptions(within(dialog).getByLabelText("Role"), "editor");
    await user.click(within(dialog).getByLabelText("admin"));
    await user.click(within(dialog).getByLabelText("data-plane"));
    await user.type(within(dialog).getByLabelText("Expires on (optional)"), "2027-01-31");
    await user.click(within(dialog).getByRole("button", { name: "Create" }));

    const result = await screen.findByRole("dialog", { name: "API key created" });
    expect(within(result).getByText(TOKEN)).toBeTruthy();
    expect(within(result).getByText("This token will not be shown again")).toBeTruthy();
    await user.click(within(result).getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledWith(TOKEN);

    const post = calls.find((c) => c.method === "POST");
    expect(JSON.parse(post?.body ?? "")).toEqual({
      name: "ci",
      role: "editor",
      scopes: ["data-plane"],
      expires_at: "2027-01-31T23:59:59Z",
    });

    await user.click(within(result).getByRole("button", { name: "Close" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByText(TOKEN)).toBeNull();
    // Opening the dialog again starts from an empty form, not the old token.
    await user.click(screen.getByRole("button", { name: "New API key" }));
    const again = await screen.findByRole("dialog", { name: "New API key" });
    expect(within(again).queryByText(TOKEN)).toBeNull();
    expect((within(again).getByLabelText("Name") as HTMLInputElement).value).toBe("");
  });

  it("role options capped at my role", async () => {
    const user = userEvent.setup();
    serve({ ...shell(adminMe), [`GET ${BASE}/api-keys`]: [] });
    renderAt("/w/default/api-keys");

    await user.click(await screen.findByRole("button", { name: "New API key" }));
    const dialog = await screen.findByRole("dialog", { name: "New API key" });
    const options = within(within(dialog).getByLabelText("Role"))
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(options).toEqual(["viewer", "editor", "admin"]);
    // Only a sysadmin can create a sysadmin key.
    expect(within(dialog).queryByLabelText(/sysadmin/i)).toBeNull();
  });

  it("sysadmin can create a sysadmin key with any role", async () => {
    const user = userEvent.setup();
    serve({ ...shell(localMe), [`GET ${BASE}/api-keys`]: [] });
    renderAt("/w/default/api-keys");

    await user.click(await screen.findByRole("button", { name: "New API key" }));
    const dialog = await screen.findByRole("dialog", { name: "New API key" });
    const options = within(within(dialog).getByLabelText("Role"))
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(options).toEqual(["viewer", "editor", "admin", "owner"]);
    expect(within(dialog).getByLabelText(/sysadmin/i)).toBeTruthy();
  });

  it("revoke asks for confirmation", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/api-keys`]: [keyView()],
      [`DELETE ${BASE}/api-keys/abcdefghijkl`]: () => new Response(null, { status: 204 }),
    });
    renderAt("/w/default/api-keys");

    await user.click(await screen.findByRole("button", { name: "Revoke ci" }));
    const confirm = await screen.findByRole("dialog", { name: "Revoke key ci?" });
    expect(calls.some((c) => c.method === "DELETE")).toBe(false);
    await user.click(within(confirm).getByRole("button", { name: "Revoke" }));
    await waitFor(() => expect(calls.some((c) => c.method === "DELETE")).toBe(true));
  });

  it("without ManageApiKeys the page explains the 403", async () => {
    const calls = serve({ ...shell(viewerMe) });
    renderAt("/w/default/api-keys");

    expect(await screen.findByText(/needs the admin role/)).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
    expect(calls.some((c) => c.path.endsWith("/api-keys"))).toBe(false);
  });
});
