import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { json, localMe, renderAt, serve, shell } from "./harness";

const METRICS = `# HELP mb_requests_total Requests served
# TYPE mb_requests_total counter
mb_requests_total{mock="orders",operation="listOrders",status="200",source="llm"} 3
mb_requests_total{mock="orders",operation="getOrder",status="200",source="fallback"} 2
mb_requests_total{mock="pets",operation="listPets",status="200",source="llm"} 4
mb_requests_total{mock="pets",operation="x",status="500",source="error"} 1.5e0
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
    expect(within(row("llm")).getByText("7")).toBeTruthy();
    expect(within(row("fallback")).getByText("2")).toBeTruthy();
    expect(within(row("error")).getByText("1.5")).toBeTruthy();
    expect(screen.getByTestId("requests-total").textContent).toBe("10.5");
    expect(screen.getByTestId("log-dropped").textContent).toBe("7");
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
});
