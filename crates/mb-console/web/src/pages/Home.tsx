/** Shown at `/` when the caller has no workspace to open. */
export function Home() {
  return (
    <main className="flex min-h-[70vh] items-center justify-center bg-canvas px-4 py-16 text-fg">
      <div className="max-w-md text-center">
        <h1 className="mb-2 text-2xl font-semibold tracking-tight">Mockingbird</h1>
        <p className="text-sm text-fg-muted">
          You do not have access to any workspace yet. Ask an administrator to grant you a role.
        </p>
      </div>
    </main>
  );
}
