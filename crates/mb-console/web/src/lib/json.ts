/** A line and column (both 1-based) of `offset` in `text`. */
function lineColumn(text: string, offset: number): { line: number; column: number } {
  const before = text.slice(0, offset);
  const line = before.split("\n").length;
  const column = offset - before.lastIndexOf("\n");
  return { line, column };
}

/**
 * A friendly explanation of why `text` failed `JSON.parse`, naming the line
 * and column when the parser reports a position, e.g.
 * `"The file is not valid JSON (line 3, column 7)."`. `subject` starts the sentence.
 */
export function invalidJsonMessage(subject: string, text: string, error: unknown): string {
  const raw = error instanceof Error ? error.message : "";
  const lc = /line (\d+) column (\d+)/.exec(raw);
  if (lc) return `${subject} is not valid JSON (line ${lc[1]}, column ${lc[2]}).`;
  const pos = /position (\d+)/.exec(raw);
  if (pos) {
    const { line, column } = lineColumn(text, Number(pos[1]));
    return `${subject} is not valid JSON (line ${line}, column ${column}).`;
  }
  if (/end of (JSON )?input/i.test(raw)) return `${subject} is not valid JSON: it ends unexpectedly.`;
  return `${subject} is not valid JSON.`;
}

/** `text` parsed, or a friendly error message (see [`invalidJsonMessage`]). */
export function parseJson(subject: string, text: string): { value: unknown } | { error: string } {
  try {
    return { value: JSON.parse(text) as unknown };
  } catch (e) {
    return { error: invalidJsonMessage(subject, text, e) };
  }
}
