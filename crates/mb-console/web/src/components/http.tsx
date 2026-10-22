import { Badge, type Tone } from "./ui";

const METHOD_STYLE: Record<string, string> = {
  GET: "text-sky-700 dark:text-sky-300",
  POST: "text-emerald-700 dark:text-emerald-300",
  PUT: "text-amber-700 dark:text-amber-300",
  PATCH: "text-violet-700 dark:text-violet-300",
  DELETE: "text-red-700 dark:text-red-300",
};

/** An HTTP method (or another protocol's verb), coloured by kind. */
export function Method({ method }: { method: string }) {
  const style = METHOD_STYLE[method.toUpperCase()] ?? "text-fg-muted";
  return <code className={`font-semibold ${style}`}>{method}</code>;
}

/** The tone of a status code: success, client error or server error. */
export function statusTone(status: number): Tone {
  if (status >= 500) return "bad";
  if (status >= 400) return "warn";
  if (status >= 200 && status < 300) return "ok";
  return "neutral";
}

/** A response status code as a badge. */
export function StatusCode({ status }: { status: number }) {
  return <Badge tone={statusTone(status)}>{status}</Badge>;
}

const SOURCE_TONE: Record<string, Tone> = { llm: "accent", repaired: "info", fallback: "neutral", state: "info", cache: "neutral" };

/** Where a response came from (`X-Mockingbird-Source`), or a dash. */
export function ResponseSource({ source }: { source?: string | null }) {
  if (!source) return <>—</>;
  return <Badge tone={SOURCE_TONE[source] ?? "neutral"}>{source}</Badge>;
}
