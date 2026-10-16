import { describe, expect, it, vi } from "vitest";
import { ApiError, api, loginUrl } from "../api/client";

function problem(status: number, title: string): Response {
  return new Response(JSON.stringify({ type: "about:blank", title, status, detail: title }), {
    status,
    headers: { "Content-Type": "application/problem+json" },
  });
}

describe("api client", () => {
  it("builds the login URL from the current path and query", () => {
    expect(loginUrl({ pathname: "/w/a b/mocks", search: "?x=1&y=2" } as Location)).toBe(
      "/auth/login?return_to=%2Fw%2Fa%20b%2Fmocks%3Fx%3D1%26y%3D2",
    );
  });

  it("client redirects to login on 401", async () => {
    const origin = window.location.origin;
    let href = `${origin}/w/default/mocks?q=1`;
    const assignments: string[] = [];
    vi.stubGlobal("location", {
      origin,
      pathname: "/w/default/mocks",
      search: "?q=1",
      get href() {
        return href;
      },
      set href(v: string) {
        assignments.push(v);
        href = v;
      },
    });
    const fetchMock = vi.fn(async () => problem(401, "authentication required"));
    vi.stubGlobal("fetch", fetchMock);

    const first = api.GET("/api/v1/me");
    await expect(first).rejects.toBeInstanceOf(ApiError);
    await expect(first).rejects.toMatchObject({ status: 401 });
    expect(window.location.href).toBe("/auth/login?return_to=%2Fw%2Fdefault%2Fmocks%3Fq%3D1");

    await expect(api.GET("/api/v1/workspaces")).rejects.toMatchObject({ status: 401 });
    expect(assignments).toHaveLength(1);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("sends JSON content type on mutations", async () => {
    const fetchMock = vi.fn<(req: Request) => Promise<Response>>(
      async () =>
        new Response(JSON.stringify({ name: "team", display_name: "Team", created_at: "", role: "owner" }), {
          status: 201,
          headers: { "Content-Type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await api.POST("/api/v1/workspaces", { body: { name: "team", display_name: "Team" } });
    const req = fetchMock.mock.calls[0]?.[0];
    expect(req?.method).toBe("POST");
    expect(req?.headers.get("Content-Type")).toBe("application/json");
  });
});
