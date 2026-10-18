/** Where a resource is managed: `file` resources are read-only in the console. */
export function SourceBadge({ source }: { source: "file" | "api" }) {
  const style =
    source === "file" ? "border-amber-300 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/60 text-amber-900 dark:text-amber-200" : "border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950 text-slate-800 dark:text-slate-200";
  return (
    <span
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium ${style}`}
      title={source === "file" ? "Managed in the configuration file (read-only here)" : "Managed through the API"}
    >
      {source}
    </span>
  );
}
