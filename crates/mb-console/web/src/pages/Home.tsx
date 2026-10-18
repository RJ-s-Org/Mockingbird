/** Shown at `/` when the caller has no workspace to open. */
export function Home() {
  return (
    <main className="mx-auto max-w-xl px-6 py-16 text-slate-900 dark:text-slate-100">
      <h1 className="mb-2 text-2xl font-semibold">Mockingbird</h1>
      <p className="text-slate-700 dark:text-slate-300">
        You do not have access to any workspace yet. Ask an administrator to grant you a role.
      </p>
    </main>
  );
}

