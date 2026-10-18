import { confidenceLevel } from "./openapi";

const STYLES = {
  High: "border-green-700 dark:border-green-500 bg-green-50 dark:bg-green-950/60 text-green-900 dark:text-green-200",
  Medium: "border-amber-700 dark:border-amber-500 bg-amber-50 dark:bg-amber-950/60 text-amber-900 dark:text-amber-200",
  Low: "border-red-700 dark:border-red-500 bg-red-50 dark:bg-red-950/60 text-red-900 dark:text-red-200",
} as const;

/** An operation's confidence as a label and a percentage (`High · 95%`); colour only repeats the label. */
export function ConfidenceBadge({ confidence }: { confidence: number }) {
  const level = confidenceLevel(confidence);
  return (
    <span
      className={`inline-block rounded border px-1.5 py-0.5 text-xs font-medium whitespace-nowrap ${STYLES[level]}`}
    >
      {`${level} · ${Math.round(confidence * 100)}%`}
    </span>
  );
}
