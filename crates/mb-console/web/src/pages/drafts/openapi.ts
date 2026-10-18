
/**
 * Pure helpers that read and edit one operation of an OpenAPI 3.x document.
 * None of them mutates its input: edits return a changed deep copy.
 */

import { parseJson } from "../../lib/json";

export type OpenApiDoc = Record<string, unknown>;

/** An operation's place in the document; `method` is upper-case. */
export interface OperationRef {
  method: string;
  path: string;
}

/** The editable parts of an operation. */
export interface OperationEdit {
  method: string;
  path: string;
  summary: string;
  requestSchema: object | null;
  responses: { status: string; schema: object | null }[];
}

type Obj = Record<string, unknown>;

/** The HTTP methods a path item can hold, upper-case. */
export const HTTP_METHODS = ["GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE"];

const JSON_CONTENT_TYPE = "application/json";

function isObject(v: unknown): v is Obj {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function clone<T>(v: T): T {
  return structuredClone(v);
}

function target(ref: OperationRef): string {
  return `${ref.method.toUpperCase()} ${ref.path}`;
}

function pathsOf(doc: OpenApiDoc): Obj {
  return isObject(doc.paths) ? doc.paths : {};
}

function operationOf(doc: OpenApiDoc, ref: OperationRef): Obj | undefined {
  const item = pathsOf(doc)[ref.path];
  if (!isObject(item)) return undefined;
  const op = item[ref.method.toLowerCase()];
  return isObject(op) ? op : undefined;
}

/** The content type whose schema an editor shows: JSON if present, else the first. */
function primaryMedia(content: unknown): [string, Obj] | undefined {
  if (!isObject(content)) return undefined;
  const json = content[JSON_CONTENT_TYPE];
  if (isObject(json)) return [JSON_CONTENT_TYPE, json];
  for (const [ct, media] of Object.entries(content)) {
    if (isObject(media)) return [ct, media];
  }
  return undefined;
}

function schemaOf(holder: unknown): object | null {
  if (!isObject(holder)) return null;
  const schema = primaryMedia(holder.content)?.[1].schema;
  return isObject(schema) ? clone(schema) : null;
}

/** The editable parts of the operation at `ref`; throws if there is none. */
export function readOperation(doc: OpenApiDoc, ref: OperationRef): OperationEdit {
  const op = operationOf(doc, ref);
  if (!op) throw new Error(`No operation ${target(ref)}`);
  const responses = isObject(op.responses) ? op.responses : {};
  return {
    method: ref.method.toUpperCase(),
    path: ref.path,
    summary: typeof op.summary === "string" ? op.summary : "",
    requestSchema: schemaOf(op.requestBody),
    responses: Object.entries(responses).map(([status, r]) => ({ status, schema: schemaOf(r) })),
  };
}

/**
 * Sets the schema of `holder`'s primary media type (a request body or a
 * response). An unchanged schema leaves everything as it was; a changed
 * one drops that media type's examples, which may no longer match; `null`
 * removes the schema.
 */
function withSchema(holder: Obj, schema: object | null): Obj {
  const before = schemaOf(holder);
  if (JSON.stringify(before) === JSON.stringify(schema)) return holder;
  const content: Obj = isObject(holder.content) ? { ...holder.content } : {};
  const [ct, media] = primaryMedia(content) ?? [JSON_CONTENT_TYPE, {}];
  const next: Obj = { ...media };
  delete next.example;
  delete next.examples;
  if (schema === null) delete next.schema;
  else next.schema = clone(schema);
  content[ct] = next;
  return { ...holder, content };
}

function editedOperation(op: Obj, edit: OperationEdit): Obj {
  const next: Obj = clone(op);
  const summary = edit.summary.trim();
  if (summary === "") delete next.summary;
  else next.summary = summary;

  if (isObject(next.requestBody)) {
    next.requestBody = withSchema(next.requestBody, edit.requestSchema);
  } else if (edit.requestSchema !== null) {
    next.requestBody = { content: { [JSON_CONTENT_TYPE]: { schema: clone(edit.requestSchema) } } };
  }

  const before = isObject(next.responses) ? next.responses : {};
  const responses: Obj = {};
  for (const { status, schema } of edit.responses) {
    const existing = before[status];
    responses[status] = withSchema(isObject(existing) ? existing : { description: "Response" }, schema);
  }
  next.responses = responses;
  return next;
}

/** Whether a path item still holds an operation. */
function hasOperations(item: Obj): boolean {
  return HTTP_METHODS.some((m) => m.toLowerCase() in item);
}

/** `paths` without the operation at `ref`; a path item left without operations is dropped. */
function pathsWithout(paths: Obj, ref: OperationRef): Obj {
  const item = paths[ref.path];
  if (!isObject(item)) return paths;
  const rest: Obj = { ...item };
  delete rest[ref.method.toLowerCase()];
  const next: Obj = { ...paths };
  if (hasOperations(rest)) next[ref.path] = rest;
  else delete next[ref.path];
  return next;
}

/**
 * The document with the operation at `ref` replaced by `edit`, moved when the
 * method or path changed. Throws when another operation already has the
 * edited method and path, or when there is no operation at `ref`.
 */
export function applyOperationEdit(doc: OpenApiDoc, ref: OperationRef, edit: OperationEdit): OpenApiDoc {
  const op = operationOf(doc, ref);
  if (!op) throw new Error(`No operation ${target(ref)}`);
  const to: OperationRef = { method: edit.method.toUpperCase(), path: edit.path };
  const moved = target(to) !== target(ref);
  if (moved && operationOf(doc, to)) {
    throw new Error(`An operation ${target(to)} already exists`);
  }
  const next = editedOperation(op, edit);
  const out = clone(doc);
  let paths = pathsOf(out);
  if (moved) paths = pathsWithout(paths, ref);
  const item = isObject(paths[to.path]) ? { ...(paths[to.path] as Obj) } : {};
  item[to.method.toLowerCase()] = next;
  out.paths = { ...paths, [to.path]: item };
  return out;
}

/** The document without the operation at `ref`; an emptied path item is dropped. */
export function removeOperation(doc: OpenApiDoc, ref: OperationRef): OpenApiDoc {
  const out = clone(doc);
  out.paths = pathsWithout(pathsOf(out), ref);
  return out;
}

/** Parses an editor's schema text: empty means no schema; anything but a JSON object is an error. */
export function parseSchemaText(text: string): { schema: object | null; error?: string } {
  if (text.trim() === "") return { schema: null };
  const parsed = parseJson("The schema", text);
  if ("error" in parsed) return { schema: null, error: parsed.error };
  const value = parsed.value;
  if (!isObject(value)) return { schema: null, error: "A schema must be a JSON object." };
  return { schema: value };
}

/** A confidence's label: `High` from 0.8, `Medium` from 0.5, else `Low`. */
export function confidenceLevel(c: number): "High" | "Medium" | "Low" {
  if (c >= 0.8) return "High";
  if (c >= 0.5) return "Medium";
  return "Low";
}
