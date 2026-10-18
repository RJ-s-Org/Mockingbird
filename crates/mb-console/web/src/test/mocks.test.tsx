import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { json, localMe, mockView, problem, renderAt, serve, shell, viewerMe } from "./harness";

const BASE = "/api/v1/workspaces/default";

const profiles = [
  {
    name: "fast",
    base_url: "http://llm",
    model: "m",
    temperature: 0.2,
    max_tokens: 512,
    timeout_ms: 1000,
    json_schema: true,
    source: "api",
  },
];

function isDisabled(el: HTMLElement): boolean {
  return el.matches(":disabled");
}

describe("mocks", () => {
  it("mocks list shows source badge and active version", async () => {
    serve({
      ...shell(localMe),
      [`GET ${BASE}/mocks`]: [
        mockView("pets", { active_version: 2 }),
        mockView("files", { source: "file", data_plane_auth: { mode: "api_key", header: "x-api-key" } }),
      ],
    });
    renderAt("/w/default/mocks");

    const table = await screen.findByRole("table", { name: "Mocks" });
    const pets = (await within(table).findByRole("link", { name: "pets" })).closest("tr") as HTMLElement;
    expect(within(pets).getByRole("link", { name: "pets" }).getAttribute("href")).toBe(
      "/w/default/mocks/pets/overview",
    );
    expect(within(pets).getByText("/pets")).toBeTruthy();
    expect(within(pets).getByText("v2")).toBeTruthy();
    expect(within(pets).getByText("api")).toBeTruthy();
    expect(within(pets).getByText("none")).toBeTruthy();

    const files = within(table).getByRole("link", { name: "files" }).closest("tr") as HTMLElement;
    expect(within(files).getByText("—")).toBeTruthy();
    expect(within(files).getByText("file")).toBeTruthy();
    expect(within(files).getByText("api_key")).toBeTruthy();
    expect(screen.getByRole("button", { name: "New mock" })).toBeTruthy();
  });

  it("create mock dialog posts and shows 409 problem", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/mocks`]: [],
      [`GET ${BASE}/llm-profiles`]: profiles,
      [`POST ${BASE}/mocks`]: () => problem(409, "Conflict", "route prefix /pets is already used by mock pets"),
    });
    renderAt("/w/default/mocks");

    await user.click(await screen.findByRole("button", { name: "New mock" }));
    const dialog = await screen.findByRole("dialog", { name: "New mock" });
    await user.type(within(dialog).getByLabelText("Name"), "pets");
    expect(within(dialog).getByLabelText("Route prefix").getAttribute("placeholder")).toBe("/pets");
    await user.type(within(dialog).getByLabelText("Persona"), "A friendly pet shop");
    await within(dialog).findByRole("option", { name: "fast" });
    await user.selectOptions(within(dialog).getByLabelText("LLM profile"), "fast");
    // On by default, as in the API; unticking it sends false.
    const overrides = within(dialog).getByLabelText<HTMLInputElement>("Header overrides");
    expect(overrides.checked).toBe(true);
    await user.click(overrides);
    await user.click(within(dialog).getByRole("button", { name: "Create" }));

    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toContain("route prefix /pets is already used by mock pets");
    const post = calls.find((c) => c.method === "POST");
    expect(post?.path).toBe(`${BASE}/mocks`);
    expect(JSON.parse(post?.body ?? "")).toEqual({
      name: "pets",
      route_prefix: null,
      persona: "A friendly pet shop",
      llm_profile: "fast",
      header_overrides: false,
    });
    // The dialog stays open so the name or prefix can be fixed.
    expect(screen.getByRole("dialog", { name: "New mock" })).toBeTruthy();
  });

  it("create mock dialog shows any problem title and detail, cleared by editing", async () => {
    const user = userEvent.setup();
    serve({
      ...shell(localMe),
      [`GET ${BASE}/mocks`]: [],
      [`GET ${BASE}/llm-profiles`]: [],
      [`POST ${BASE}/mocks`]: () => problem(409, "Mock already exists", 'mock "pets" already exists in workspace default'),
    });
    renderAt("/w/default/mocks");

    await user.click(await screen.findByRole("button", { name: "New mock" }));
    const dialog = await screen.findByRole("dialog", { name: "New mock" });
    const name = within(dialog).getByLabelText("Name");
    await user.type(name, "pets");
    await user.click(within(dialog).getByRole("button", { name: "Create" }));
    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toContain("Mock already exists");
    expect(alert.textContent).toContain('mock "pets" already exists in workspace default');

    await user.type(name, "2");
    expect(within(dialog).queryByRole("alert")).toBeNull();
  });

  it("create mock dialog validates the name like the server", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/mocks`]: [],
      [`GET ${BASE}/llm-profiles`]: [],
    });
    renderAt("/w/default/mocks");

    await user.click(await screen.findByRole("button", { name: "New mock" }));
    const dialog = await screen.findByRole("dialog", { name: "New mock" });
    const name = within(dialog).getByLabelText("Name");
    const prefix = within(dialog).getByLabelText("Route prefix");
    const hint = within(dialog).getByText(/Up to 63 lowercase letters/);
    expect(prefix.getAttribute("placeholder")).toBe("/<name>");

    await user.type(name, "Bad Name");
    expect(name.getAttribute("aria-invalid")).toBe("true");
    expect(hint.textContent).toMatch(/^Invalid name\./);
    // Invalid input is never echoed as the default prefix.
    expect(prefix.getAttribute("placeholder")).toBe("/<name>");
    expect(isDisabled(within(dialog).getByRole("button", { name: "Create" }))).toBe(true);

    await user.clear(name);
    await user.type(name, "-pets");
    expect(name.getAttribute("aria-invalid")).toBe("true");
    await user.clear(name);
    await user.type(name, "good-name-1");
    expect(name.getAttribute("aria-invalid")).toBe("false");
    expect(prefix.getAttribute("placeholder")).toBe("/good-name-1");
    expect(isDisabled(within(dialog).getByRole("button", { name: "Create" }))).toBe(false);
    expect(calls.some((c) => c.method === "POST")).toBe(false);
  });

  it("file-managed mock is read-only", async () => {
    serve({
      ...shell(localMe),
      [`GET ${BASE}/mocks/files`]: mockView("files", { source: "file", active_version: 1 }),
      [`GET ${BASE}/llm-profiles`]: profiles,
      [`GET ${BASE}/mocks/files/contracts`]: [],
    });
    renderAt("/w/default/mocks/files/overview");

    // Shown for the settings and again for the chaos and limits section.
    expect(await screen.findAllByText("Managed by config file")).toHaveLength(2);
    for (const label of ["Route prefix", "Persona", "LLM profile", "Header overrides", "Data-plane auth", "Key header"]) {
      expect(isDisabled(screen.getByLabelText(label)), label).toBe(true);
    }
    expect(isDisabled(screen.getByRole("button", { name: "Save" }))).toBe(true);
    expect(isDisabled(screen.getByRole("button", { name: "Delete mock" }))).toBe(true);

    await userEvent.setup().click(screen.getByRole("link", { name: "Contracts" }));
    await screen.findByRole("table", { name: "Contract versions" });
    expect(screen.getByText("Managed by config file")).toBeTruthy();
    expect(screen.queryByLabelText("Contract file")).toBeNull();
  });

  it("overview save patches only changed fields", async () => {
    const user = userEvent.setup();
    let current = mockView("pets", { persona: "old" });
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/mocks/pets`]: () => json(current),
      [`GET ${BASE}/llm-profiles`]: profiles,
      [`PATCH ${BASE}/mocks/pets`]: async (req: Request) => {
        current = { ...current, ...((await req.json()) as object), updated_at: "2026-10-16T01:00:00Z" };
        return json(current);
      },
    });
    renderAt("/w/default/mocks/pets/overview");

    const save = await screen.findByRole("button", { name: "Save" });
    expect(isDisabled(save)).toBe(true);
    await user.clear(screen.getByLabelText("Persona"));
    await user.selectOptions(screen.getByLabelText("Data-plane auth"), "api_key");
    await user.click(save);

    await screen.findByText("Saved.");
    const patch = calls.find((c) => c.method === "PATCH");
    expect(JSON.parse(patch?.body ?? "")).toEqual({
      persona: null,
      data_plane_auth: { mode: "api_key", header: "x-api-key" },
    });
  });

  it("deletes a mock after confirmation", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/mocks/pets`]: mockView("pets"),
      [`GET ${BASE}/mocks`]: [],
      [`GET ${BASE}/llm-profiles`]: profiles,
      [`DELETE ${BASE}/mocks/pets`]: () => new Response(null, { status: 204 }),
    });
    const router = renderAt("/w/default/mocks/pets/overview");

    await user.click(await screen.findByRole("button", { name: "Delete mock" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete mock pets?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/w/default/mocks"));
    expect(calls.some((c) => c.method === "DELETE" && c.path === `${BASE}/mocks/pets`)).toBe(true);
  });

  it("viewer sees no mutating actions", async () => {
    const user = userEvent.setup();
    serve({
      ...shell(viewerMe),
      [`GET ${BASE}/mocks`]: [mockView("pets", { active_version: 2 })],
      [`GET ${BASE}/mocks/pets`]: mockView("pets", { active_version: 2 }),
      [`GET ${BASE}/llm-profiles`]: profiles,
      [`GET ${BASE}/mocks/pets/contracts`]: [
        {
          version: 2,
          status: "active",
          source_digest: "a".repeat(64),
          created_at: "2026-10-16T00:00:00Z",
          operations: 3,
        },
        {
          version: 1,
          status: "approved",
          source_digest: "b".repeat(64),
          created_at: "2026-10-15T00:00:00Z",
          operations: 3,
        },
      ],
    });
    renderAt("/w/default/mocks");

    // Wait for both the list and `me` (the header's Sign out renders only once
    // `me` has loaded), so the Edit gate on "New mock" has been evaluated.
    const list = await screen.findByRole("table", { name: "Mocks" });
    await screen.findByRole("button", { name: "Sign out" });
    expect(screen.getByText("Vera")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "New mock" })).toBeNull();

    await user.click(within(list).getByRole("link", { name: "pets" }));
    await screen.findByLabelText("Persona");
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Delete/ })).toBeNull();
    expect(isDisabled(screen.getByLabelText("Persona"))).toBe(true);

    await user.click(screen.getByRole("link", { name: "Contracts" }));
    const table = await screen.findByRole("table", { name: "Contract versions" });
    await within(table).findByText("v1");
    expect(screen.queryByLabelText("Contract file")).toBeNull();
    expect(screen.queryByRole("button", { name: /Activate|Roll back/ })).toBeNull();
    // Reading stays possible.
    expect(within(table).getAllByRole("link", { name: /Download source/ })).toHaveLength(2);
    expect(within(table).getAllByRole("button", { name: /View operations/ })).toHaveLength(2);
  });
});
