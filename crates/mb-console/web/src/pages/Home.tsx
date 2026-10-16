/** Shown at `/` when the caller has no workspace to open. */
export function Home() {
  return (
    <main className="mx-auto max-w-xl px-6 py-16 text-slate-900">
      <h1 className="mb-2 text-2xl font-semibold">Mockingbird</h1>
      <p className="text-slate-700">
        You do not have access to any workspace yet. Ask an administrator to grant you a role.
      </p>
    </main>
  );
}

/** A page whose content arrives in a later version of the console. */
export function Pending({ title }: { title: string }) {
  return (
    <section>
      <h1 className="mb-2 text-2xl font-semibold">{title}</h1>
      <p className="text-slate-700">Nothing to show here yet.</p>
    </section>
  );
}
