import { directionLabel } from "../../api/channels";

/** A direction from the mock's point of view; the tooltip names the AsyncAPI terms. */
export function Direction({ value }: { value: string }) {
  const { label, hint } = directionLabel(value);
  return (
    <span
      title={hint}
      className="cursor-help whitespace-nowrap underline decoration-fg-muted decoration-dotted underline-offset-2"
    >
      {label}
    </span>
  );
}
