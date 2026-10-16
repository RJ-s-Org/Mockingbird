import type { QueryClient } from "@tanstack/react-query";
import {
  Outlet,
  createRootRouteWithContext,
  createRoute,
  createRouter,
  redirect,
  type RouterHistory,
} from "@tanstack/react-router";
import { lastWorkspace, workspacesQuery } from "./api/queries";
import { Layout } from "./components/Layout";
import { ProblemAlert } from "./components/ProblemAlert";
import { Home, Pending } from "./pages/Home";
import { NotFound } from "./pages/NotFound";

export interface RouterContext {
  queryClient: QueryClient;
}

const rootRoute = createRootRouteWithContext<RouterContext>()({
  component: Outlet,
  notFoundComponent: NotFound,
  errorComponent: ({ error }) => (
    <main className="mx-auto max-w-xl px-6 py-16">
      <ProblemAlert error={error} />
    </main>
  ),
});

/** `/`: the last used workspace if it still exists, else the first one. */
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: async ({ context }) => {
    const workspaces = await context.queryClient.ensureQueryData(workspacesQuery);
    const last = lastWorkspace();
    const ws = workspaces.find((w) => w.name === last)?.name ?? workspaces[0]?.name;
    if (ws) throw redirect({ to: "/w/$ws/mocks", params: { ws } });
  },
  component: Home,
});

const workspaceRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/w/$ws",
  component: function WorkspaceFrame() {
    const { ws } = workspaceRoute.useParams();
    return <Layout ws={ws} />;
  },
});

const workspaceIndexRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/",
  beforeLoad: ({ params }) => {
    throw redirect({ to: "/w/$ws/mocks", params });
  },
});

const mocksRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/mocks",
  component: () => <Pending title="Mocks" />,
});

const mockRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/mocks/$mock",
  component: Outlet,
});

const mockIndexRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/",
  beforeLoad: ({ params }) => {
    throw redirect({ to: "/w/$ws/mocks/$mock/overview", params });
  },
});

const mockTab = <P extends string>(path: P, title: string) =>
  createRoute({
    getParentRoute: () => mockRoute,
    path,
    component: () => <Pending title={title} />,
  });

const mockOverviewRoute = mockTab("/overview", "Overview");
const mockContractsRoute = mockTab("/contracts", "Contracts");
const mockScenariosRoute = mockTab("/scenarios", "Scenarios");
const mockSessionsRoute = mockTab("/sessions", "Sessions");
const mockRequestsRoute = mockTab("/requests", "Requests");

const profilesRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/llm-profiles",
  component: () => <Pending title="LLM profiles" />,
});

const apiKeysRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/api-keys",
  component: () => <Pending title="API keys" />,
});

const auditRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/audit",
  component: () => <Pending title="Audit" />,
});

/** `/status`: outside any workspace; the frame keeps the last used one. */
const statusRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/status",
  component: function StatusFrame() {
    return <Layout ws={lastWorkspace() ?? undefined} />;
  },
});

const statusIndexRoute = createRoute({
  getParentRoute: () => statusRoute,
  path: "/",
  component: () => <Pending title="Status" />,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  workspaceRoute.addChildren([
    workspaceIndexRoute,
    mocksRoute,
    mockRoute.addChildren([
      mockIndexRoute,
      mockOverviewRoute,
      mockContractsRoute,
      mockScenariosRoute,
      mockSessionsRoute,
      mockRequestsRoute,
    ]),
    profilesRoute,
    apiKeysRoute,
    auditRoute,
  ]),
  statusRoute.addChildren([statusIndexRoute]),
]);

/** The console's router; `history` defaults to the browser's. */
export function createAppRouter({ queryClient, history }: { queryClient: QueryClient; history?: RouterHistory }) {
  return createRouter({
    routeTree,
    context: { queryClient },
    history,
    defaultPreload: "intent",
  });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createAppRouter>;
  }
}
