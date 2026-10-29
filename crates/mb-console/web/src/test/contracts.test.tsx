import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { CONTRACT_EXTENSIONS, contentTypeFor } from "../api/queries";
import { json, localMe, mockView, problem, renderAt, serve, shell } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/pets";

function summary(version: number, status: string, created_at = "2026-10-16T00:00:00Z") {
  return {
    version,
    status,
    source_digest: `${version}`.repeat(64),
    created_by: version === 1 ? "alice" : null,
    created_at,
    operations: 4,
  };
}

describe("contracts", () => {
  it("chooses the content type by extension", () => {
    expect(contentTypeFor("orders.json")).toBe("application/json");
    expect(contentTypeFor("orders.YAML")).toBe("application/yaml");
    expect(contentTypeFor("orders.yml")).toBe("application/yaml");
    expect(contentTypeFor("schema.graphql")).toBe("application/graphql");
    expect(contentTypeFor("calc.proto")).toBe("text/x-protobuf");
    expect(contentTypeFor("set.pb")).toBe("application/x-protobuf");
    expect(contentTypeFor("calc.wsdl")).toBe("application/xml");
    expect(contentTypeFor("protos.zip")).toBe("application/zip");
    expect(contentTypeFor("protos.TAR.GZ")).toBe("application/gzip");
    expect(contentTypeFor("protos.tgz")).toBe("application/gzip");
    expect(contentTypeFor("orders.txt")).toBeUndefined();
    expect(contentTypeFor("README")).toBeUndefined();
  });

  it("upload sends yaml content type and activate flag", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
      [`GET ${MOCK}/contracts`]: [summary(1, "active")],
      [`POST ${MOCK}/contracts`]: () => json({ version: 2, status: "active", operations: 4 }, 201),
    });
    renderAt("/w/default/mocks/pets/contracts");

    const source = "openapi: 3.0.3\ninfo: {title: Orders, version: '1'}\npaths: {}\n";
    const input = await screen.findByLabelText("Contract file");
    expect(input.getAttribute("accept")).toBe(CONTRACT_EXTENSIONS.join(","));
    await user.upload(input, new File([source], "orders.yaml", { type: "" }));
    await user.click(screen.getByLabelText("Activate after upload"));
    await user.click(screen.getByRole("button", { name: "Upload" }));

    await screen.findByText("Uploaded version 2 (active, 4 operations).");
    const post = calls.find((c) => c.method === "POST");
    expect(post?.path).toBe(`${MOCK}/contracts`);
    expect(post?.search).toBe("?activate=true");
    expect(post?.contentType).toBe("application/yaml");
    expect(post?.body).toBe(source);
  });

  it("upload sends an archive as raw bytes with its media type", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
      [`GET ${MOCK}/contracts`]: [summary(1, "active")],
      [`POST ${MOCK}/contracts`]: () => json({ version: 2, status: "approved", operations: 3 }, 201),
    });
    renderAt("/w/default/mocks/pets/contracts");

    // A zip's local file header magic, then ASCII so the recorded text is exact.
    const bytes = new TextEncoder().encode("PK\u0003\u0004protos/calc.proto");
    const input = await screen.findByLabelText("Contract file");
    await user.upload(input, new File([bytes], "protos.zip", { type: "" }));
    await user.click(screen.getByRole("button", { name: "Upload" }));

    await screen.findByText("Uploaded version 2 (approved, 3 operations).");
    const post = calls.find((c) => c.method === "POST");
    expect(post?.contentType).toBe("application/zip");
    expect(post?.body).toBe("PK\u0003\u0004protos/calc.proto");
  });

  it("upload without activation and a 422 shows the importer detail", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`GET ${MOCK}/contracts`]: [],
      [`POST ${MOCK}/contracts`]: () =>
        problem(422, "Unprocessable Entity", "contract does not compile: missing field `openapi`"),
    });
    renderAt("/w/default/mocks/pets/contracts");

    await user.upload(await screen.findByLabelText("Contract file"), new File(["{}"], "broken.json"));
    await user.click(screen.getByRole("button", { name: "Upload" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("contract does not compile: missing field `openapi`");
    const post = calls.find((c) => c.method === "POST");
    expect(post?.search).toBe("");
    expect(post?.contentType).toBe("application/json");
  });

  it("rollback activates older version", async () => {
    const user = userEvent.setup();
    let active = 2;
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: () => json(mockView("pets", { active_version: active })),
      [`GET ${MOCK}/contracts`]: () =>
        json([
          summary(3, active === 3 ? "active" : "approved", "2026-10-16T02:00:00Z"),
          summary(2, active === 2 ? "active" : "approved", "2026-10-16T01:00:00Z"),
          summary(1, active === 1 ? "active" : "approved"),
        ]),
      [`POST ${MOCK}/contracts/1/activate`]: () => {
        active = 1;
        return json(summary(1, "active"));
      },
      [`GET ${MOCK}/contracts/1`]: {
        ...summary(1, "active"),
        title: "Orders",
        api_version: "1.0.0",
        operations: [
          {
            id: "listOrders",
            method: "GET",
            path: "/orders",
            kind: "query",
            protocol: "http",
            target: "GET /orders",
          },
        ],
      },
    });
    renderAt("/w/default/mocks/pets/contracts");

    const table = await screen.findByRole("table", { name: "Contract versions" });
    await within(table).findByText("v3");
    const row = (v: number) => within(table).getByText(`v${v}`).closest("tr") as HTMLElement;
    // The digest is shortened to 12 characters.
    expect(within(row(1)).getByText("111111111111")).toBeTruthy();
    expect(within(row(1)).getByText("alice")).toBeTruthy();
    // Newer than the active version: Activate; older: Roll back; active: neither.
    expect(within(row(3)).getByRole("button", { name: "Activate version 3" })).toBeTruthy();
    expect(within(row(2)).queryByRole("button", { name: /Activate|Roll back/ })).toBeNull();
    const download = within(row(1)).getByRole("link", { name: "Download source of version 1" });
    expect(download.getAttribute("href")).toBe(`${MOCK}/contracts/1/source`);
    expect(download.hasAttribute("download")).toBe(true);

    await user.click(within(row(1)).getByRole("button", { name: "Roll back to version 1" }));
    await waitFor(() => expect(within(row(1)).getByText("active")).toBeTruthy());
    const activation = calls.find((c) => c.method === "POST");
    expect(activation?.path).toBe(`${MOCK}/contracts/1/activate`);
    expect(within(row(2)).getByRole("button", { name: "Activate version 2" })).toBeTruthy();

    await user.click(within(row(1)).getByRole("button", { name: "View operations of version 1" }));
    const drawer = await screen.findByRole("dialog", { name: "Operations of version 1" });
    await within(drawer).findByText("listOrders");
    expect(within(drawer).getByText("GET /orders")).toBeTruthy();
    expect(within(drawer).getByText("http")).toBeTruthy();
  });

  it("discard deletes a version that is not active, after confirmation", async () => {
    const user = userEvent.setup();
    let versions = [summary(2, "draft", "2026-10-16T01:00:00Z"), summary(1, "active")];
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
      [`GET ${MOCK}/contracts`]: () => json(versions),
      [`DELETE ${MOCK}/contracts/2`]: () => {
        versions = versions.filter((v) => v.version !== 2);
        return new Response(null, { status: 204 });
      },
    });
    renderAt("/w/default/mocks/pets/contracts");

    const table = await screen.findByRole("table", { name: "Contract versions" });
    await within(table).findByText("v2");
    const row = (v: number) => within(table).getByText(`v${v}`).closest("tr") as HTMLElement;
    // The active version cannot be discarded.
    expect(within(row(1)).queryByRole("button", { name: /Discard/ })).toBeNull();

    await user.click(within(row(2)).getByRole("button", { name: "Discard version 2" }));
    const confirm = await screen.findByRole("dialog", { name: "Discard version 2?" });
    expect(calls.some((c) => c.method === "DELETE")).toBe(false);
    await user.click(within(confirm).getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(within(table).queryByText("v2")).toBeNull());
    expect(calls.some((c) => c.method === "DELETE" && c.path === `${MOCK}/contracts/2`)).toBe(true);
  });
});
