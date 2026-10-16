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
import { ApiKeys } from "./pages/ApiKeys";
import { Audit } from "./pages/Audit";
import { Home } from "./pages/Home";
import { Contracts } from "./pages/mocks/Contracts";
import { ImportProgress } from "./pages/mocks/ImportProgress";
import { ImportWizard } from "./pages/mocks/ImportWizard";
import { LiveRequests } from "./pages/mocks/LiveRequests";
import { MockLayout } from "./pages/mocks/MockLayout";
import { MocksList } from "./pages/mocks/MocksList";
import { Overview } from "./pages/mocks/Overview";
import { Scenarios } from "./pages/mocks/Scenarios";
import { Sessions } from "./pages/mocks/Sessions";
import { NotFound } from "./pages/NotFound";
import { Profiles } from "./pages/Profiles";
import { Status } from "./pages/Status";

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
  component: function MocksPage() {
    const { ws } = mocksRoute.useParams();
    return <MocksList ws={ws} />;
  },
});

const mockRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/mocks/$mock",
  component: function MockFrame() {
    const { ws, mock } = mockRoute.useParams();
    return <MockLayout ws={ws} mock={mock} />;
  },
});

const mockIndexRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/",
  beforeLoad: ({ params }) => {
    throw redirect({ to: "/w/$ws/mocks/$mock/overview", params });
  },
});

const mockOverviewRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/overview",
  component: function OverviewPage() {
    const { ws, mock } = mockOverviewRoute.useParams();
    return <Overview ws={ws} mock={mock} />;
  },
});

const mockContractsRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/contracts",
  component: function ContractsPage() {
    const { ws, mock } = mockContractsRoute.useParams();
    return <Contracts ws={ws} mock={mock} />;
  },
});

/** The import tab's search: `job` resumes a started import's progress. */
interface ImportSearch {
  job?: string;
}

const mockImportRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/import",
  validateSearch: (search: Record<string, unknown>): ImportSearch =>
    typeof search.job === "string" && search.job !== "" ? { job: search.job } : {},
  component: function ImportPage() {
    const { ws, mock } = mockImportRoute.useParams();
    const { job } = mockImportRoute.useSearch();
    return job ? <ImportProgress key={job} ws={ws} mock={mock} job={job} /> : <ImportWizard ws={ws} mock={mock} />;
  },
});

/** A draft version's review page; a placeholder until the review page lands. */
const mockDraftRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/contracts/$version/draft",
  component: function DraftPage() {
    const { version } = mockDraftRoute.useParams();
    return (
      <section className="space-y-2">
        <h2 className="text-lg font-semibold">Draft version {version}</h2>
        <p className="text-sm text-slate-700">Draft review is not available yet.</p>
      </section>
    );
  },
});

const mockScenariosRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/scenarios",
  component: function ScenariosPage() {
    const { ws, mock } = mockScenariosRoute.useParams();
    return <Scenarios ws={ws} mock={mock} />;
  },
});

const mockSessionsRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/sessions",
  component: function SessionsPage() {
    const { ws, mock } = mockSessionsRoute.useParams();
    return <Sessions ws={ws} mock={mock} />;
  },
});

const mockRequestsRoute = createRoute({
  getParentRoute: () => mockRoute,
  path: "/requests",
  component: function LiveRequestsPage() {
    const { ws, mock } = mockRequestsRoute.useParams();
    return <LiveRequests ws={ws} mock={mock} />;
  },
});

const profilesRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/llm-profiles",
  component: function ProfilesPage() {
    const { ws } = profilesRoute.useParams();
    return <Profiles ws={ws} />;
  },
});

const apiKeysRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/api-keys",
  component: function ApiKeysPage() {
    const { ws } = apiKeysRoute.useParams();
    return <ApiKeys ws={ws} />;
  },
});

const auditRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/audit",
  component: function AuditPage() {
    const { ws } = auditRoute.useParams();
    return <Audit ws={ws} />;
  },
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
  component: Status,
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
      mockDraftRoute,
      mockImportRoute,
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
