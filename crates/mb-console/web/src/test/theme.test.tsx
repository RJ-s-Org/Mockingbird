import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { initTheme, resolvesDark, savedTheme, setTheme } from "../lib/theme";
import { localMe, renderAt, serve, shell } from "./harness";

function stubSystemDark(dark: boolean) {
  vi.stubGlobal(
    "matchMedia",
    vi.fn((query: string) => ({
      matches: dark && query === "(prefers-color-scheme: dark)",
      addEventListener: () => {},
      removeEventListener: () => {},
    })),
  );
}

afterEach(() => {
  setTheme("system");
  document.documentElement.classList.remove("dark");
});

describe("theme", () => {
  it("follows prefers-color-scheme by default", () => {
    stubSystemDark(true);
    initTheme();
    expect(savedTheme()).toBe("system");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    expect(document.documentElement.style.colorScheme).toBe("dark");

    stubSystemDark(false);
    initTheme();
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });

  it("a manual choice overrides the system and is remembered", () => {
    stubSystemDark(false);
    setTheme("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    expect(window.localStorage.getItem("mb.theme")).toBe("dark");
    expect(savedTheme()).toBe("dark");
    setTheme("system");
    expect(window.localStorage.getItem("mb.theme")).toBeNull();
    expect(resolvesDark("light")).toBe(false);
  });

  it("works without storage", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(savedTheme()).toBe("system");
    expect(() => setTheme("dark")).not.toThrow();
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("the header offers the theme choice", async () => {
    const user = userEvent.setup();
    serve({ ...shell(localMe), "GET /api/v1/workspaces/default/mocks": [] });
    renderAt("/w/default/mocks");
    const select = (await screen.findByLabelText("Theme")) as HTMLSelectElement;
    expect(select.value).toBe("system");
    await user.selectOptions(select, "dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    expect(select.value).toBe("dark");
  });
});
