import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { sourceRows } from "../api/queries";
import { sourceLabel } from "../lib/sources";
import { json, localMe, renderAt, serve, shell } from "./harness";

const METRICS = `# HELP mb_requests_total Requests served
# TYPE mb_requests_total counter
mb_requests_total{mock="orders",operation="listOrders",status="200",source="llm"} 3
mb_requests_total{mock="orders",operation="getOrder",status="200",source="fallback"} 2
mb_requests_total{mock="pets",operation="listPets",status="200",source="llm"} 4
mb_requests_total{mock="pets",operation="x",status="500",source="error"} 1.5e0
mb_requests_total{mock="pets",operation="y",status="422",source="not_generated"} 4
mb_request_log_dropped_total{mock="orders"} 2
mb_request_log_dropped_total{mock="pets"} 5
mb_requests_total_other 99
`;

function text(body: string, status = 200): Response {
  return new Response(body, { status, headers: { "Content-Type": "text/plain; version=0.0.4" } });
}

describe("status", () => {
  it("status parses metrics text", async () => {
    serve({
      ...shell(localMe),
      "GET /readyz": () => json({ status: "degraded", reasons: ["mock default/orders: boom"] }),
      "GET /metrics": () => text(METRICS),
    });
    renderAt("/status");

    expect(await screen.findByText("degraded")).toBeTruthy();
    expect(screen.getByText("mock default/orders: boom")).toBeTruthy();
    const summary = await screen.findByRole("table", { name: "Requests by source" });
    const row = (source: string) => within(summary).getByRole("rowheader", { name: source }).closest("tr") as HTMLElement;
    expect(within(row("LLM")).getByText("7")).toBeTruthy();
    expect(within(row("Fallback")).getByText("2")).toBeTruthy();
    // An unknown source is shown as is.
    expect(within(row("error")).getByText("1.5")).toBeTruthy();
    // "not_generated" gets a human label and an explaining tooltip.
    const none = row("Not generated (rejected or failed before generation)");
    expect(within(none).getByText("4")).toBeTruthy();
    expect(within(none).getByText(/^Not generated/).getAttribute("title")).toMatch(/rejected by validation/);
    expect(screen.getByTestId("requests-total").textContent).toBe("14.5");
    expect(screen.getByTestId("log-dropped").textContent).toBe("7");
  });

  it("source rows are sorted by count, then name, whatever the metrics order", async () => {
    expect(sourceRows({ llm: 3, fallback: 81, not_generated: 10, error: 3 })).toEqual([
      ["fallback", 81],
      ["not_generated", 10],
      ["error", 3],
      ["llm", 3],
    ]);
    serve({
      ...shell(localMe),
      "GET /readyz": () => json({ status: "ready" }),
      "GET /metrics": () =>
        text(
          'mb_requests_total{source="not_generated"} 10\nmb_requests_total{source="fallback"} 81\nmb_requests_total{source="llm"} 81\n',
        ),
    });
    renderAt("/status");
    const summary = await screen.findByRole("table", { name: "Requests by source" });
    const names = within(summary)
      .getAllByRole("rowheader")
      .map((h) => h.textContent);
    expect(names).toEqual(["Fallback", "LLM", "Not generated (rejected or failed before generation)"]);
  });

  it("an unavailable server shows its reasons", async () => {
    serve({
      ...shell(localMe),
      "GET /readyz": () => json({ status: "unavailable", reasons: ["control store unreachable: x"] }, 503),
      "GET /metrics": () => text(""),
    });
    renderAt("/status");
    expect(await screen.findByText("unavailable")).toBeTruthy();
    expect(screen.getByText("control store unreachable: x")).toBeTruthy();
    expect((await screen.findByTestId("requests-total")).textContent).toBe("0");
  });

  it("private metrics are explained", async () => {
    serve({
      ...shell(localMe),
      "GET /readyz": () => json({ status: "ready" }),
      "GET /metrics": () => text("", 401),
    });
    renderAt("/status");
    expect(await screen.findByText("ready")).toBeTruthy();
    expect(await screen.findByText("Metrics are private")).toBeTruthy();
  });

  it("maps known source names to labels with a raw fallback", () => {
    for (const key of ["none", "not_generated", "rejected"]) {
      expect(sourceLabel(key).label).toBe("Not generated (rejected or failed before generation)");
    }
    expect(sourceLabel("llm").label).toBe("LLM");
    expect(sourceLabel("something_new")).toEqual({ label: "something_new", hint: "something_new" });
  });
});
