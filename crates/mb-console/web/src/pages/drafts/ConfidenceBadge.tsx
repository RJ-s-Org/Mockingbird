import { confidenceLevel } from "./openapi";

const STYLES = {
  High: "border-green-700 bg-green-50 text-green-900",
  Medium: "border-amber-700 bg-amber-50 text-amber-900",
  Low: "border-red-700 bg-red-50 text-red-900",
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
