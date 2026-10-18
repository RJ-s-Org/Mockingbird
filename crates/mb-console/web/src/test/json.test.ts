import { describe, expect, it } from "vitest";
import { invalidJsonMessage, parseJson } from "../lib/json";

describe("json", () => {
  it("parses valid JSON", () => {
    expect(parseJson("The file", '{"a":1}')).toEqual({ value: { a: 1 } });
  });

  it("hides the parser's raw message", () => {
    const r = parseJson("The file", "[not json");
    expect("error" in r && r.error).toMatch(/^The file is not valid JSON/);
    expect("error" in r && r.error).not.toContain("Unexpected token");
  });

  it("names the line and column when the parser gives a position", () => {
    const text = '{\n  "a": 1,\n  oops\n}';
    expect(invalidJsonMessage("The file", text, new SyntaxError("Unexpected token in JSON at position 14"))).toBe(
      "The file is not valid JSON (line 3, column 3).",
    );
    expect(
      invalidJsonMessage("Set", text, new SyntaxError("Expected ',' in JSON at position 14 (line 3 column 3)")),
    ).toBe("Set is not valid JSON (line 3, column 3).");
  });

  it("explains truncated input", () => {
    expect(invalidJsonMessage("The file", "[1,", new SyntaxError("Unexpected end of JSON input"))).toBe(
      "The file is not valid JSON: it ends unexpectedly.",
    );
    expect(invalidJsonMessage("The file", "x", new SyntaxError("Unexpected token 'x', \"x\" is not valid JSON"))).toBe(
      "The file is not valid JSON.",
    );
  });
});
