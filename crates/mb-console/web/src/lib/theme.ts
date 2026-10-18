import { useSyncExternalStore } from "react";

/** The colour theme choice: follow the system (`prefers-color-scheme`), or force one. */
export type Theme = "system" | "light" | "dark";

const STORAGE_KEY = "mb.theme";
const QUERY = "(prefers-color-scheme: dark)";

function isTheme(v: unknown): v is Theme {
  return v === "system" || v === "light" || v === "dark";
}

/** The saved choice; `system` when none is saved or storage is unavailable. */
export function savedTheme(): Theme {
  try {
    const v = window.localStorage.getItem(STORAGE_KEY);
    return isTheme(v) ? v : "system";
  } catch {
    return "system";
  }
}

function systemDark(): boolean {
  try {
    return typeof window.matchMedia === "function" && window.matchMedia(QUERY).matches;
  } catch {
    return false;
  }
}

/** Whether `theme` renders dark right now. */
export function resolvesDark(theme: Theme): boolean {
  return theme === "dark" || (theme === "system" && systemDark());
}

let current: Theme = "system";
const listeners = new Set<() => void>();

/** Puts `theme` on the document: the `dark` class (for `dark:` variants) and `color-scheme`. */
function apply(theme: Theme): void {
  const dark = resolvesDark(theme);
  const root = document.documentElement;
  root.classList.toggle("dark", dark);
  root.style.colorScheme = dark ? "dark" : "light";
}

/** Reads the saved choice, applies it and follows system changes while it is `system`. Call once at start-up. */
export function initTheme(): void {
  current = savedTheme();
  apply(current);
  try {
    window.matchMedia?.(QUERY).addEventListener("change", () => {
      if (current === "system") apply(current);
    });
  } catch {
    // No media queries (old browsers, tests): the saved choice still applies.
  }
}

/** Saves (best effort) and applies `theme`. */
export function setTheme(theme: Theme): void {
  current = theme;
  try {
    if (theme === "system") window.localStorage.removeItem(STORAGE_KEY);
    else window.localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // Storage may be unavailable (private mode, blocked): the choice lasts for this page.
  }
  apply(theme);
  for (const l of listeners) l();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The current theme choice and its setter. */
export function useTheme(): [Theme, (theme: Theme) => void] {
  const theme = useSyncExternalStore(subscribe, () => current);
  return [theme, setTheme];
}
