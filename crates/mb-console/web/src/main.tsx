import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { ApiError } from "./api/client";
import "./index.css";
import { initTheme } from "./lib/theme";
import { createAppRouter } from "./router";

// Before the first render, so the page never flashes the wrong theme.
initTheme();

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // Client errors (401, 403, 404, …) will not change on a retry.
      retry: (count, error) => !(error instanceof ApiError && error.status < 500) && count < 2,
    },
  },
});
const router = createAppRouter({ queryClient });

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <QueryClientProvider client={queryClient}>
        <RouterProvider router={router} />
      </QueryClientProvider>
    </StrictMode>,
  );
}
