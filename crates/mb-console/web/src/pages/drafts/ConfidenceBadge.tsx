import { Badge, type Tone } from "../../components/ui";
import { confidenceLevel } from "./openapi";

const TONES = { High: "ok", Medium: "warn", Low: "bad" } as const satisfies Record<string, Tone>;

/** An operation's confidence as a label and a percentage (`High · 95%`); colour only repeats the label. */
export function ConfidenceBadge({ confidence }: { confidence: number }) {
  const level = confidenceLevel(confidence);
  return <Badge tone={TONES[level]}>{`${level} · ${Math.round(confidence * 100)}%`}</Badge>;
}
