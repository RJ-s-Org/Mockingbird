import { describe, expect, it } from "vitest";
import {
  applyOperationEdit,
  confidenceLevel,
  parseSchemaText,
  readOperation,
  removeOperation,
  type OpenApiDoc,
} from "../pages/drafts/openapi";

const PET = { type: "object", properties: { id: { type: "integer" }, name: { type: "string" } } };
const NEW_PET = { type: "object", properties: { name: { type: "string" } }, required: ["name"] };

function doc(): OpenApiDoc {
  return {
    openapi: "3.1.0",
    info: { title: "Pets", version: "1" },
    paths: {
      "/pets": {
        get: {
          operationId: "listPets",
          summary: "List pets",
          responses: {
            "200": { description: "OK", content: { "application/json": { schema: { type: "array", items: PET } } } },
          },
        },
        post: {
          operationId: "createPet",
          summary: "Create a pet",
          requestBody: { content: { "application/json": { schema: NEW_PET, example: { name: "Rex" } } } },
          responses: {
            "201": { description: "Created", content: { "application/json": { schema: PET } } },
            "400": { description: "Bad Request" },
          },
        },
      },
      "/pets/{petId}": {
        get: {
          operationId: "getPet",
          parameters: [{ name: "petId", in: "path", required: true, schema: { type: "integer" } }],
          responses: { "200": { description: "OK", content: { "application/json": { schema: PET } } } },
        },
      },
    },
  };
}

describe("openapi", () => {
  it("readOperation reads schemas", () => {
    const edit = readOperation(doc(), { method: "POST", path: "/pets" });
    expect(edit).toEqual({
      method: "POST",
      path: "/pets",
      summary: "Create a pet",
      requestSchema: NEW_PET,
      responses: [
        { status: "201", schema: PET },
        { status: "400", schema: null },
      ],
    });
    const get = readOperation(doc(), { method: "GET", path: "/pets/{petId}" });
    expect(get.summary).toBe("");
    expect(get.requestSchema).toBeNull();
    expect(() => readOperation(doc(), { method: "DELETE", path: "/pets" })).toThrow("No operation DELETE /pets");
  });

  it("applyOperationEdit renames path and method", () => {
    const before = doc();
    const edit = readOperation(before, { method: "GET", path: "/pets/{petId}" });
    const after = applyOperationEdit(
      before,
      { method: "GET", path: "/pets/{petId}" },
      { ...edit, method: "PUT", path: "/animals/{petId}", summary: "Replace a pet", requestSchema: NEW_PET },
    );
    const paths = after.paths as Record<string, Record<string, Record<string, unknown>>>;
    expect(paths["/pets/{petId}"]).toBeUndefined();
    const put = paths["/animals/{petId}"]?.put;
    expect(put?.operationId).toBe("getPet");
    expect(put?.summary).toBe("Replace a pet");
    expect(put?.parameters).toEqual([{ name: "petId", in: "path", required: true, schema: { type: "integer" } }]);
    expect(put?.requestBody).toEqual({ content: { "application/json": { schema: NEW_PET } } });
    expect(readOperation(after, { method: "PUT", path: "/animals/{petId}" }).responses).toEqual([
      { status: "200", schema: PET },
    ]);

    // An unchanged schema keeps its examples; a cleared summary is removed.
    const same = readOperation(before, { method: "POST", path: "/pets" });
    const kept = applyOperationEdit(before, { method: "POST", path: "/pets" }, { ...same, summary: "" });
    const post = (kept.paths as Record<string, Record<string, Record<string, unknown>>>)["/pets"]?.post;
    expect(post?.summary).toBeUndefined();
    expect(post?.requestBody).toEqual({
      content: { "application/json": { schema: NEW_PET, example: { name: "Rex" } } },
    });

    // A response schema set where there was none gets JSON content.
    const withBody = applyOperationEdit(
      before,
      { method: "POST", path: "/pets" },
      {
        ...same,
        responses: [
          { status: "201", schema: PET },
          { status: "400", schema: { type: "object" } },
        ],
      },
    );
    expect(readOperation(withBody, { method: "POST", path: "/pets" }).responses[1]).toEqual({
      status: "400",
      schema: { type: "object" },
    });
  });

  it("applyOperationEdit rejects collisions", () => {
    const before = doc();
    const edit = readOperation(before, { method: "POST", path: "/pets" });
    expect(() => applyOperationEdit(before, { method: "POST", path: "/pets" }, { ...edit, method: "GET" })).toThrow(
      "An operation GET /pets already exists",
    );
    // Keeping the same target is not a collision.
    expect(() => applyOperationEdit(before, { method: "POST", path: "/pets" }, edit)).not.toThrow();
  });

  it("removeOperation drops empty path items", () => {
    const one = removeOperation(doc(), { method: "GET", path: "/pets/{petId}" });
    expect(Object.keys(one.paths as object)).toEqual(["/pets"]);
    const two = removeOperation(doc(), { method: "GET", path: "/pets" });
    expect(Object.keys((two.paths as Record<string, object>)["/pets"] ?? {})).toEqual(["post"]);
  });

  it("parseSchemaText rejects non-objects", () => {
    expect(parseSchemaText("")).toEqual({ schema: null });
    expect(parseSchemaText("  \n")).toEqual({ schema: null });
    expect(parseSchemaText('{"type":"string"}')).toEqual({ schema: { type: "string" } });
    for (const text of ["[]", "null", "42", '"string"', "true"]) {
      const r = parseSchemaText(text);
      expect(r.schema).toBeNull();
      expect(r.error).toBe("A schema must be a JSON object.");
    }
    const bad = parseSchemaText("{oops");
    expect(bad.schema).toBeNull();
    expect(bad.error).toMatch(/^Not valid JSON/);
  });

  it("inputs are not mutated", () => {
    const before = doc();
    const snapshot = JSON.parse(JSON.stringify(before));
    const ref = { method: "POST", path: "/pets" };
    const edit = readOperation(before, ref);
    applyOperationEdit(before, ref, { ...edit, path: "/animals", summary: "x", requestSchema: { type: "object" } });
    removeOperation(before, { method: "GET", path: "/pets/{petId}" });
    expect(before).toEqual(snapshot);
    // What readOperation returns is a copy, too.
    (edit.responses[0]?.schema as Record<string, unknown>).type = "string";
    expect(before).toEqual(snapshot);
  });

  it("confidence levels", () => {
    expect(confidenceLevel(1)).toBe("High");
    expect(confidenceLevel(0.8)).toBe("High");
    expect(confidenceLevel(0.79)).toBe("Medium");
    expect(confidenceLevel(0.5)).toBe("Medium");
    expect(confidenceLevel(0.49)).toBe("Low");
    expect(confidenceLevel(0)).toBe("Low");
  });
});
