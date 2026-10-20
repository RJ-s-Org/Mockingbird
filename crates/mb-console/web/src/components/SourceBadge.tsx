import { Badge } from "./ui";

/** Where a resource is managed: `file` resources are read-only in the console. */
export function SourceBadge({ source }: { source: "file" | "api" }) {
  return (
    <Badge
      tone={source === "file" ? "warn" : "neutral"}
      title={source === "file" ? "Managed in the configuration file (read-only here)" : "Managed through the API"}
    >
      {source}
    </Badge>
  );
}
