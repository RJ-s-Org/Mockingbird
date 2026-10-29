import { describe, expect, it, vi } from "vitest";
import { ApiError, api, loginUrl, signOut } from "../api/client";

function problem(status: number, title: string): Response {
  return new Response(JSON.stringify({ type: "about:blank", title, status, detail: title }), {
    status,
    headers: { "Content-Type": "application/problem+json" },
  });
}

describe("sign out", () => {
  const logout = (response: Response | Error) =>
    vi.fn<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>(async () => {
      if (response instanceof Error) throw response;
      return response;
    });

  it("goes to the provider's end-session URL when the server returns one", async () => {
    const url = "https://idp.example/logout?client_id=mockingbird&post_logout_redirect_uri=https%3A%2F%2Fmb.example%2F";
    const fetchMock = logout(
      new Response(JSON.stringify({ end_session_url: url }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await expect(signOut()).resolves.toBe(url);
    const [input, init] = fetchMock.mock.calls[0] ?? [];
    expect(String(input)).toBe("/auth/logout");
    expect(init?.method).toBe("POST");
    expect(new Headers(init?.headers).get("Content-Type")).toBe("application/json");
  });

  it("signs in again when the provider has no end-session endpoint or logout fails", async () => {
    vi.stubGlobal("fetch", logout(new Response(null, { status: 204 })));
    await expect(signOut()).resolves.toBe("/auth/login?return_to=/");
    vi.stubGlobal("fetch", logout(new Error("offline")));
    await expect(signOut()).resolves.toBe("/auth/login?return_to=/");
    // Only an http(s) URL is followed.
    vi.stubGlobal(
      "fetch",
      logout(new Response(JSON.stringify({ end_session_url: "javascript:alert(1)" }), { status: 200 })),
    );
    await expect(signOut()).resolves.toBe("/auth/login?return_to=/");
  });
});

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
