import { Link } from "@tanstack/react-router";
import { linkClass } from "../components/ui";

export function NotFound() {
  return (
    <main className="flex min-h-[70vh] items-center justify-center bg-canvas px-4 py-16 text-fg">
      <div className="max-w-md text-center">
        <h1 className="mb-2 text-2xl font-semibold tracking-tight">Page not found</h1>
        <p className="mb-6 text-sm text-fg-muted">There is nothing at this address.</p>
        <Link to="/" className={linkClass}>
          Back to the console
        </Link>
      </div>
    </main>
  );
}
