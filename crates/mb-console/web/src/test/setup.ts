import { cleanup } from "@testing-library/react";
import { afterEach, vi } from "vitest";

// jsdom does not implement scrolling; the router restores scroll positions.
window.scrollTo = () => {};

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  window.localStorage.clear();
});
