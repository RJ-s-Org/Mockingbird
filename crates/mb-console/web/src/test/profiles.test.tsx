import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { json, localMe, problem, renderAt, serve, shell, viewerMe } from "./harness";

const BASE = "/api/v1/workspaces/default";

const fast = {
  name: "fast",
  base_url: "http://llm.local/v1",
  model: "gpt-mini",
  secret_ref: "env:MB_LLM_API_KEY",
  temperature: 0.2,
  max_tokens: 512,
  timeout_ms: 1000,
  json_schema: true,
  fallback_profile: null,
  source: "api",
};

const fromFile = { ...fast, name: "boot", secret_ref: null, fallback_profile: "fast", source: "file" };

describe("llm profiles", () => {
  it("profile table lists every column", async () => {
    serve({ ...shell(localMe), [`GET ${BASE}/llm-profiles`]: [fast, fromFile] });
    renderAt("/w/default/llm-profiles");

    const table = await screen.findByRole("table", { name: "LLM profiles" });
    const headers = within(table)
      .getAllByRole("columnheader")
      .map((h) => h.textContent);
    expect(headers.slice(0, 6)).toEqual(["Name", "Model", "Base URL", "Secret ref", "Fallback", "Source"]);
    const boot = within(table).getByRole("cell", { name: "boot" }).closest("tr") as HTMLElement;
    expect(within(boot).getByText("file")).toBeTruthy();
    expect(within(boot).getByRole("cell", { name: "fast" })).toBeTruthy();
    // A file profile cannot be changed here.
    expect(within(boot).queryByRole("button", { name: /Edit/ })).toBeNull();
    expect(within(table).getByRole("button", { name: "Edit fast" })).toBeTruthy();
  });

  it("profile form has no raw key field and shows warnings", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/llm-profiles`]: [fromFile],
      [`PUT ${BASE}/llm-profiles/fast`]: () => json({ ...fast, warnings: ["mock orders uses profile fast"] }),
    });
    renderAt("/w/default/llm-profiles");

    await user.click(await screen.findByRole("button", { name: "New profile" }));
    const dialog = await screen.findByRole("dialog", { name: "New profile" });
    // Only a reference to the key: no field takes the key itself.
    expect(within(dialog).queryByLabelText(/api.?key/i)).toBeNull();
    expect(dialog.querySelector('input[type="password"]')).toBeNull();
    const secretRef = within(dialog).getByLabelText("Secret ref");
    expect(within(dialog).getByText("Use env:MB_LLM_<NAME> or env:<NAME>_API_KEY. The key itself is never stored.")).toBeTruthy();
    expect(secretRef.getAttribute("aria-describedby")).toBeTruthy();

    await user.type(within(dialog).getByLabelText("Name"), "fast");
    await user.type(within(dialog).getByLabelText("Base URL"), "http://llm.local/v1");
    await user.type(within(dialog).getByLabelText("Model"), "gpt-mini");
    await user.type(secretRef, "env:MB_LLM_API_KEY");
    await user.click(within(dialog).getByLabelText("JSON schema responses"));
    await user.clear(within(dialog).getByLabelText("Temperature"));
    await user.type(within(dialog).getByLabelText("Temperature"), "0.2");
    await user.clear(within(dialog).getByLabelText("Timeout (ms)"));
    await user.type(within(dialog).getByLabelText("Timeout (ms)"), "1000");
    await user.clear(within(dialog).getByLabelText("Max tokens"));
    await user.type(within(dialog).getByLabelText("Max tokens"), "512");
    await user.selectOptions(within(dialog).getByLabelText("Fallback profile"), "boot");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    const status = await screen.findByRole("status");
    expect(status.textContent).toContain("Saved profile fast.");
    expect(status.textContent).toContain("mock orders uses profile fast");
    expect(screen.queryByRole("dialog")).toBeNull();
    const put = calls.find((c) => c.method === "PUT");
    expect(put?.path).toBe(`${BASE}/llm-profiles/fast`);
    const body = JSON.parse(put?.body ?? "");
    expect(body).toEqual({
      base_url: "http://llm.local/v1",
      model: "gpt-mini",
      secret_ref: "env:MB_LLM_API_KEY",
      json_schema: true,
      temperature: 0.2,
      timeout_ms: 1000,
      max_tokens: 512,
      fallback_profile: "boot",
    });
    expect("api_key" in body).toBe(false);
  });

  it("a refused save shows the server's problem detail", async () => {
    const user = userEvent.setup();
    serve({
      ...shell(localMe),
      [`GET ${BASE}/llm-profiles`]: [],
      [`PUT ${BASE}/llm-profiles/local`]: () =>
        problem(422, "Unprocessable Entity", "secret_ref env:OTHER is not allowed by server.api_secret_refs"),
    });
    renderAt("/w/default/llm-profiles");

    await user.click(await screen.findByRole("button", { name: "New profile" }));
    const dialog = await screen.findByRole("dialog", { name: "New profile" });
    await user.type(within(dialog).getByLabelText("Name"), "local");
    await user.type(within(dialog).getByLabelText("Base URL"), "http://llm.local/v1");
    await user.type(within(dialog).getByLabelText("Model"), "m");
    await user.type(within(dialog).getByLabelText("Secret ref"), "env:OTHER");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toContain("secret_ref env:OTHER is not allowed by server.api_secret_refs");
  });

  it("closing the editor gives focus back to the Edit button", async () => {
    const user = userEvent.setup();
    serve({ ...shell(localMe), [`GET ${BASE}/llm-profiles`]: [fast, fromFile] });
    renderAt("/w/default/llm-profiles");

    const edit = await screen.findByRole("button", { name: "Edit fast" });
    await user.click(edit);
    await screen.findByRole("dialog", { name: "Edit profile fast" });
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    // The button was not remounted while the drawer was open, so focus returns to it.
    expect(edit.isConnected).toBe(true);
    expect(document.activeElement).toBe(edit);
  });

  it("delete asks first and shows the warnings", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${BASE}/llm-profiles`]: [fast],
      [`DELETE ${BASE}/llm-profiles/fast`]: () => json({ deleted: "fast", warnings: ["mock orders still uses fast"] }),
    });
    renderAt("/w/default/llm-profiles");

    await user.click(await screen.findByRole("button", { name: "Delete fast" }));
    const confirm = await screen.findByRole("dialog", { name: "Delete profile fast?" });
    await user.click(within(confirm).getByRole("button", { name: "Delete" }));
    const status = await screen.findByRole("status");
    expect(status.textContent).toContain("mock orders still uses fast");
    expect(calls.some((c) => c.method === "DELETE" && c.path === `${BASE}/llm-profiles/fast`)).toBe(true);
  });

  it("viewer sees a read-only table", async () => {
    serve({ ...shell(viewerMe), [`GET ${BASE}/llm-profiles`]: [fast] });
    renderAt("/w/default/llm-profiles");

    const table = await screen.findByRole("table", { name: "LLM profiles" });
    expect(within(table).getByRole("cell", { name: "fast" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "New profile" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Edit|Delete/ })).toBeNull();
    expect(screen.getByText(/Read-only/)).toBeTruthy();
  });
});
