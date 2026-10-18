import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { json, localMe, mockView, renderAt, serve, shell, viewerMe, type Answer } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/pets";
const DRAFT = `${MOCK}/contracts/2/draft`;
const PAGE = "/w/default/mocks/pets/contracts/2/draft";

const PET = { type: "object", properties: { id: { type: "integer" }, name: { type: "string" } } };

function document() {
  return {
    openapi: "3.1.0",
    info: { title: "Pets", version: "1" },
    paths: {
      "/pets": {
        get: {
          operationId: "listPets",
          summary: "List pets",
          responses: {
            "200": { description: "OK", content: { "application/json": { schema: { type: "array", items: PET } } } },
          },
        },
      },
      "/pets/{petId}": {
        get: {
          operationId: "getPet",
          summary: "Get a pet",
          parameters: [{ name: "petId", in: "path", required: true, schema: { type: "integer" } }],
          responses: {
            "200": { description: "OK", content: { "application/json": { schema: PET } } },
            "404": { description: "Not Found" },
          },
        },
      },
    },
  };
}

function draft(overrides: Record<string, unknown> = {}) {
  return {
    version: 2,
    status: "draft",
    document: document(),
    meta: {
      source_kind: "markdown",
      source_name: "pets.md",
      source_url: null,
      chunk_count: 3,
      chunks_processed: 2,
      usage: { prompt_tokens: 1200, completion_tokens: 340 },
      warnings: [
        { code: "chunk_skipped", message: "chunk 3 could not be analyzed", operation: null },
        { code: "no_responses", message: "GET /pets: no responses were described", operation: "GET /pets" },
      ],
      edited: false,
    },
    operations: [
      {
        id: "listPets",
        method: "GET",
        path: "/pets",
        summary: "List pets",
        confidence: 0.6,
        edited: false,
        provenance: [],
      },
      {
        id: "getPet",
        method: "GET",
        path: "/pets/{petId}",
        summary: "Get a pet",
        confidence: 0.95,
        edited: false,
        provenance: [{ chunk: 1, location: "## Get a pet", excerpt: "GET /pets/{petId} returns one pet." }],
      },
    ],
    diff: {
      against_version: 1,
      added: ["GET /pets/{petId}"],
      removed: ["DELETE /pets/{petId}"],
      changed: ["GET /pets"],
    },
    ...overrides,
  };
}

function routes(me = localMe, extra: Record<string, Answer> = {}): Record<string, Answer> {
  return {
    ...shell(me),
    [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
    [`GET ${DRAFT}`]: draft(),
    ...extra,
  };
}

function operationRow(target: string) {
  const table = screen.getByRole("table", { name: "Draft operations" });
  const [method, path] = target.split(" ");
  const row = within(table)
    .getAllByRole("row")
    .find(
      (r) =>
        within(r).queryByRole("cell", { name: method }) !== null &&
        within(r).queryByRole("cell", { name: path }) !== null,
    );
  if (!row) throw new Error(`no row ${target}`);
  return row;
}

describe("draft review", () => {
  it("a deterministic import says no LLM was used instead of zero chunks and tokens", async () => {
    const base = draft();
    serve(
      routes(localMe, {
        [`GET ${DRAFT}`]: draft({
          meta: {
            ...base.meta,
            source_kind: "har",
            source_name: "pets.har",
            chunk_count: 0,
            chunks_processed: 0,
            usage: { prompt_tokens: 0, completion_tokens: 0 },
          },
        }),
      }),
    );
    renderAt(PAGE);

    expect(await screen.findByText("Deterministic import (no LLM used)")).toBeTruthy();
    expect(screen.queryByText(/chunks processed/)).toBeNull();
    expect(screen.queryByText(/completion tokens/)).toBeNull();
  });

  it("lists operations with confidence badges and provenance", async () => {
    const user = userEvent.setup();
    serve(routes());
    renderAt(PAGE);

    await screen.findByRole("heading", { name: "Draft v2" });
    expect(screen.getByText("Status: draft")).toBeTruthy();
    expect(screen.getByText(/markdown · pets\.md/)).toBeTruthy();
    expect(screen.getByText("2 of 3 chunks processed")).toBeTruthy();
    expect(screen.getByText("1200 prompt + 340 completion tokens")).toBeTruthy();

    const pet = operationRow("GET /pets/{petId}");
    expect(within(pet).getByText("High · 95%")).toBeTruthy();
    expect(within(operationRow("GET /pets")).getByText("Medium · 60%")).toBeTruthy();
    expect(within(pet).getByRole("cell", { name: "Get a pet" })).toBeTruthy();

    const toggle = within(pet).getByRole("button", { name: "Show provenance of GET /pets/{petId}" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    await user.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    const quote = pet.querySelector("blockquote");
    expect(quote?.textContent).toBe("GET /pets/{petId} returns one pet.");
    expect(within(pet).getByText("## Get a pet")).toBeTruthy();
    // No provenance: nothing to toggle.
    expect(within(operationRow("GET /pets")).queryByRole("button", { name: /provenance/ })).toBeNull();
  });

  it("shows warnings and diff summary", async () => {
    serve(routes());
    renderAt(PAGE);

    const warnings = await screen.findByRole("list", { name: "Warnings" });
    const items = within(warnings).getAllByRole("listitem");
    expect(items).toHaveLength(2);
    expect(items[0]?.textContent).toContain("chunk_skipped");
    expect(items[0]?.textContent).toContain("chunk 3 could not be analyzed");

    expect(screen.getByText("Compared with v1: 1 added, 1 removed, 1 changed")).toBeTruthy();
    const details = screen.getByText("Compared with v1: 1 added, 1 removed, 1 changed").closest("details");
    expect(details).not.toBeNull();
    expect(within(details as HTMLElement).getByRole("list", { name: "Added" }).textContent).toContain(
      "GET /pets/{petId}",
    );
    expect(within(details as HTMLElement).getByRole("list", { name: "Removed" }).textContent).toContain(
      "DELETE /pets/{petId}",
    );
    expect(within(details as HTMLElement).getByRole("list", { name: "Changed" }).textContent).toContain("GET /pets");
  });

  it("edit saves the updated document", async () => {
    const user = userEvent.setup();
    let saved: { document: ReturnType<typeof document> } | undefined;
    const calls = serve(
      routes(localMe, {
        [`PUT ${DRAFT}`]: async (req: Request) => {
          saved = await req.json();
          const view = draft({ document: saved?.document, meta: { ...draft().meta, edited: true } });
          view.operations[1] = { ...view.operations[1]!, summary: "Fetch a pet", edited: true, confidence: 1 };
          return json(view);
        },
      }),
    );
    renderAt(PAGE);

    await user.click(await screen.findByRole("button", { name: "Edit GET /pets/{petId}" }));
    const editor = screen.getByRole("form", { name: "Edit GET /pets/{petId}" });
    expect((within(editor).getByLabelText("Method") as HTMLSelectElement).value).toBe("GET");
    expect((within(editor).getByLabelText("Path") as HTMLInputElement).value).toBe("/pets/{petId}");
    expect((within(editor).getByLabelText("Response 200 schema") as HTMLTextAreaElement).value).toBe(
      JSON.stringify(PET, null, 2),
    );
    expect((within(editor).getByLabelText("Response 404 schema") as HTMLTextAreaElement).value).toBe("");
    // The editor opens inline, in the row right under its operation, with focus on its first field.
    const rows = within(screen.getByRole("table", { name: "Draft operations" })).getAllByRole("row");
    const opIndex = rows.indexOf(operationRow("GET /pets/{petId}"));
    expect(rows[opIndex + 1]?.contains(editor)).toBe(true);
    expect(window.document.activeElement).toBe(within(editor).getByLabelText("Method"));
    // Approving waits for the open edit.
    const summary = within(editor).getByLabelText("Summary");
    await user.clear(summary);
    await user.type(summary, "Fetch a pet");
    expect((screen.getByRole("button", { name: "Approve" }) as HTMLButtonElement).disabled).toBe(true);
    await user.click(within(editor).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(within(operationRow("GET /pets/{petId}")).getByText("edited")).toBeTruthy());
    expect(within(operationRow("GET /pets/{petId}")).getByRole("cell", { name: "Fetch a pet" })).toBeTruthy();
    expect(screen.queryByRole("form", { name: "Edit GET /pets/{petId}" })).toBeNull();
    expect((screen.getByRole("button", { name: "Approve" }) as HTMLButtonElement).disabled).toBe(false);

    const put = calls.find((c) => c.method === "PUT");
    expect(put?.path).toBe(DRAFT);
    const expected = document();
    expected.paths["/pets/{petId}"].get.summary = "Fetch a pet";
    expect(saved?.document).toEqual(expected);
  });

  it("invalid schema text blocks save", async () => {
    const user = userEvent.setup();
    const calls = serve(routes());
    renderAt(PAGE);

    await user.click(await screen.findByRole("button", { name: "Edit GET /pets/{petId}" }));
    const editor = screen.getByRole("form", { name: "Edit GET /pets/{petId}" });
    const request = within(editor).getByLabelText("Request schema");
    await user.click(request);
    await user.paste("[]");
    expect(within(editor).getByText("A schema must be a JSON object.")).toBeTruthy();
    expect(request.getAttribute("aria-invalid")).toBe("true");
    const save = within(editor).getByRole("button", { name: "Save" }) as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    const response = within(editor).getByLabelText("Response 200 schema");
    await user.clear(response);
    await user.click(response);
    await user.paste("{nope");
    expect(within(editor).getByText(/^The schema is not valid JSON/)).toBeTruthy();

    await user.clear(request);
    expect(save.disabled).toBe(true);
    await user.clear(response);
    expect(save.disabled).toBe(false);
    expect(calls.some((c) => c.method === "PUT")).toBe(false);
  });

  it("422 shows pointers", async () => {
    const user = userEvent.setup();
    serve(
      routes(localMe, {
        [`PUT ${DRAFT}`]: () =>
          new Response(
            JSON.stringify({
              type: "about:blank",
              title: "Unprocessable Content",
              status: 422,
              detail: "the draft document is invalid",
              errors: [
                { pointer: "/paths/~1pets~1{id}/get", message: "path parameter id is not declared" },
                { pointer: "/paths/~1pets/get/responses", message: "at least one response is required" },
              ],
            }),
            { status: 422, headers: { "Content-Type": "application/problem+json" } },
          ),
      }),
    );
    renderAt(PAGE);

    await user.click(await screen.findByRole("button", { name: "Edit GET /pets/{petId}" }));
    const editor = screen.getByRole("form", { name: "Edit GET /pets/{petId}" });
    const path = within(editor).getByLabelText("Path");
    await user.clear(path);
    await user.click(path);
    await user.paste("/pets/{id}");
    await user.click(within(editor).getByRole("button", { name: "Save" }));

    const problems = await within(editor).findByRole("list", { name: "Problems" });
    const items = within(problems)
      .getAllByRole("listitem")
      .map((li) => li.textContent);
    expect(items).toEqual([
      "/paths/~1pets~1{id}/get: path parameter id is not declared",
      "/paths/~1pets/get/responses: at least one response is required",
    ]);
    // The editor stays open with the user's input.
    expect((within(editor).getByLabelText("Path") as HTMLInputElement).value).toBe("/pets/{id}");
  });

  it("preview shows generated body", async () => {
    const user = userEvent.setup();
    const bodies: unknown[] = [];
    serve(
      routes(localMe, {
        [`POST ${MOCK}/contracts/2/preview`]: async (req: Request) => {
          const body = (await req.json()) as { status?: number };
          bodies.push(body);
          return body.status === 404
            ? json({ status: 404, content_type: "application/json", source: "fallback", body: { error: "missing" } })
            : json({ status: 200, content_type: "application/json", source: "llm", body: { id: 7, name: "Rex" } });
        },
      }),
    );
    renderAt(PAGE);

    await user.click(await screen.findByRole("button", { name: "Preview GET /pets/{petId}" }));
    const pre = await screen.findByLabelText("Preview body");
    expect(pre.tagName).toBe("PRE");
    expect(pre.textContent).toBe(JSON.stringify({ id: 7, name: "Rex" }, null, 2));
    expect(screen.getByText("Status 200 · source llm")).toBeTruthy();
    expect(bodies[0]).toEqual({ operation_id: "getPet" });

    const status = screen.getByLabelText("Response status") as HTMLSelectElement;
    expect(Array.from(status.options).map((o) => o.value)).toEqual(["", "200", "404"]);
    await user.selectOptions(status, "404");
    await user.click(screen.getByRole("button", { name: "Generate preview" }));
    await screen.findByText("Status 404 · source fallback");
    expect(bodies[1]).toEqual({ operation_id: "getPet", status: 404 });
    expect(screen.getByLabelText("Preview body").textContent).toContain('"error": "missing"');
  });

  it("approve confirms warnings then offers activate", async () => {
    const user = userEvent.setup();
    let status = "draft";
    const calls = serve(
      routes(localMe, {
        [`GET ${DRAFT}`]: () => json(draft({ status })),
        [`POST ${MOCK}/contracts/2/approve`]: () => {
          status = "approved";
          return json({
            version: 2,
            status,
            source_digest: "2".repeat(64),
            created_by: "local",
            created_at: "2026-10-16T00:00:00Z",
            operations: 2,
            imported: true,
          });
        },
        [`POST ${MOCK}/contracts/2/activate`]: () => {
          status = "active";
          return json({
            version: 2,
            status,
            source_digest: "2".repeat(64),
            created_by: "local",
            created_at: "2026-10-16T00:00:00Z",
            operations: 2,
            imported: true,
          });
        },
      }),
    );
    renderAt(PAGE);

    await user.click(await screen.findByRole("button", { name: "Approve" }));
    const dialog = screen.getByRole("dialog", { name: "Approve draft v2" });
    expect(dialog.textContent).toContain("This draft has 2 warnings");
    await user.click(within(dialog).getByRole("button", { name: "Approve" }));

    await screen.findByText("Status: approved");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
    const approve = calls.find((c) => c.method === "POST" && c.path.endsWith("/approve"));
    expect(approve?.contentType).toBe("application/json");

    await user.click(screen.getByRole("button", { name: "Activate" }));
    await screen.findByText("Status: active");
    expect(screen.queryByRole("button", { name: "Activate" })).toBeNull();
    const back = screen.getByRole("link", { name: "Back to contracts" });
    expect(back.getAttribute("href")).toBe("/w/default/mocks/pets/contracts");
  });

  it("approved draft is read-only", async () => {
    serve(routes(localMe, { [`GET ${DRAFT}`]: draft({ status: "approved" }) }));
    renderAt(PAGE);

    const edit = (await screen.findByRole("button", { name: "Edit GET /pets/{petId}" })) as HTMLButtonElement;
    expect(edit.disabled).toBe(true);
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
    expect(screen.getByRole("button", { name: "Activate" })).toBeTruthy();
  });

  it("viewer sees no edit or approve actions", async () => {
    serve(routes(viewerMe));
    renderAt(PAGE);

    await screen.findByRole("table", { name: "Draft operations" });
    expect(screen.queryByRole("button", { name: /^Edit / })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Preview / })).toBeNull();
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Activate" })).toBeNull();
  });

  it("a version that is not a number is not found", async () => {
    serve(routes());
    renderAt("/w/default/mocks/pets/contracts/abc/draft");
    await screen.findByText("Page not found");
  });
});
