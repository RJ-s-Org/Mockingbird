import { Link } from "@tanstack/react-router";

export function NotFound() {
  return (
    <main className="mx-auto max-w-xl px-6 py-16 text-slate-900">
      <h1 className="mb-2 text-2xl font-semibold">Page not found</h1>
      <p className="mb-6 text-slate-700">There is nothing at this address.</p>
      <Link to="/" className="text-indigo-700 underline focus-visible:outline-2 focus-visible:outline-indigo-700">
        Back to the console
      </Link>
    </main>
  );
}
