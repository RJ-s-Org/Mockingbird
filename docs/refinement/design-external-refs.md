# Design: multi-file contracts with external `$ref`s (MB-IMPORT-STUB-2)

Status: the archive and relative-file case ships in F3-REFS. The rest is listed
under [Not in this PR](#not-in-this-pr).

## Problem

OpenAPI 3.x, Swagger 2.0 and AsyncAPI documents are often split into files:
`openapi.yaml` refers to `./schemas/pet.yaml` or `../common.yaml#/Error`.
Before this change, a multi-file upload compiled only the one file that sniffs
as a contract, and every `$ref` that did not start with `#` failed with
"unresolvable $ref". A single uploaded file gave the same message, with no hint
that the referenced files were missing.

## Supported inputs

| Input | How the files arrive | This PR |
|---|---|---|
| Archive upload (zip, tar.gz) through the Admin API or the console | `unpack_archive` gives the member files | yes |
| A local directory through the CLI (`mockingbird import dir/`, `mockingbird validate dir/`, a `contract:` path in config) | `read_source` packs the directory into an archive (`pack_dir`) | yes, through the archive path |
| Loose files through the library (`compile_files`) | the caller's `SourceFile`s | yes |
| A single file whose refs name files that were not uploaded | | refused, with an error that says what to do |
| A URL import whose spec has relative refs | the smart importer fetches one URL | no (see below) |
| `$ref` to an absolute URL (`https://…`) | | never fetched; left as is, and refused with a clear error if the compiler needs it |

## Choosing the root document

`compile_files` keeps its order (proto, then WSDL, then GraphQL, then one
contract). For the "one contract" case, the candidates are the files that
sniff as a contract and are not documentation. When there are several, a
candidate that another candidate references through an external `$ref` is not
a root (a shared `common.yaml` with its own `openapi:` header). If exactly one
root remains it is compiled; otherwise the error names the candidates, as
before.

## Resolution

A bundling pass runs on the root document (OpenAPI 3.x, Swagger 2.0, AsyncAPI
2.x/3.0) before the compiler, and only when the upload has more than one file
and the root holds at least one relative external ref; otherwise compilation
is byte-for-byte what it was.

1. **Which refs.** Every `"$ref": "<string>"` member of the document, except
   under vendor extensions (`x-*`) and literal values (`example`, an Example object's `value`, `enum`,
   `const`, `default`, and `examples` inside a schema), the same positions
   `collect_refs` skips.
2. **Splitting.** A ref is `<path>#<pointer>` (both parts optional and
   percent-decoded). A ref with a URI scheme (`https:`, `file:`, …) or
   starting with `//` is a URL: it is left unchanged and never fetched.
3. **Path resolution.** `<path>` is resolved against the directory of the
   file holding the ref, with `.` and `..` segments folded. An absolute path,
   a backslash, or a `..` that climbs above the upload root is an error
   (traversal guard). The resolved path must name an uploaded file, or the
   error names the ref, the file holding it and the missing path. A ref with
   only a pointer (`#/Pet`) inside a non-root file points into that file.
4. **Parsing.** A referenced file is parsed as JSON or YAML once (cached by
   path); a parse failure names the file. The pointer must exist in it.
5. **Refs back into the root** (`../openapi.yaml#/components/schemas/X`)
   become the local ref `#/components/schemas/X`.

## Bundling into one stored contract

The bundled root is an ordinary single document, which the existing compiler
turns into the one stored `Bundle`. Two placements:

- **Schema positions are hoisted.** A ref in a schema position (under a
  `schema` or `payload` member, inside `components.schemas` or `definitions`,
  or anywhere inside a hoisted schema) is replaced by a local ref to a new
  entry of the definitions section (`#/components/schemas/<name>`, or
  `#/definitions/<name>` for Swagger 2.0). The target's content is bundled
  once, however many refs name it.
- **Other positions are inlined.** Path items, parameters, responses, request
  bodies, headers and messages referenced from another file are copied in
  place (bundled recursively against their own file). Sibling members of the
  ref object (`description`, `summary`) are kept over the target's.

Naming is deterministic so the source digest and the definitions are stable:
the name of a hoisted schema is the last segment of its pointer, or the file
stem when the ref names the whole file (`./schemas/pet.yaml` gives `pet`),
with characters outside `[A-Za-z0-9._-]` replaced by `_`. A name already taken
(by the root's own schemas or an earlier hoist of a different target) gets
`_2`, `_3`, … in the order refs are met, and the walk order is the document's
order. When a root schema is only a ref (`Pet: {$ref: ./pet.yaml}`), the
target's content takes that entry's name instead of a new one.

`source_digest` of a bundled upload is the digest of all its files (the same
digest the WSDL and proto multi-file paths use); an archive keeps the digest
of the archive bytes.

## Cycles

A schema is hoisted under its name before its content is bundled, so a
recursive schema (`Node.children.items -> ./node.yaml`) turns into a local
recursive ref, which the compilers already support. A cycle through inlined
(non-schema) positions cannot be bundled into one document and is an error
naming the ref.

## Limits

- The upload limits already apply (20 MiB archive, 500 files, 64 MiB
  unpacked).
- Bundled content (each inlined or hoisted target, counted as compact JSON
  every time it is placed) is capped at 32 MiB, so a "billion laughs" chain of
  inlined refs fails quickly instead of expanding (a hostile 2^40-copy chain
  fails in 0.5 s with a 190 MB peak, release build); the largest single
  contract upload is 20 MiB.
- Nested refs are followed at most 64 deep.

## SSRF

Nothing is fetched. URL refs stay as they are; when the compiler reaches one
it fails with "external $ref … cannot be resolved", naming the ref and
suggesting to upload the referenced files or bundle the spec. The audit's
"never fetch URLs" rule stands.

## Not in this PR

- **URL imports with relative refs.** The smart importer fetches a single URL
  through the guarded fetcher (`smart::fetch`, private-address blocking and
  redirect checks). Following relative refs would mean fetching more URLs on
  the same origin, with a per-import count and byte cap and the same guards;
  until then such a spec has to be bundled (`redocly bundle`) or uploaded as
  an archive.
- **The CLI pointed at one file of a split spec.** `mockingbird import
  openapi.yaml` reads only that file; the error tells the user to point it at
  the directory. Packing the parent directory automatically is possible but
  could upload unrelated files.
- **Refs to URLs inside the upload** (`https://example.com/schemas/pet.yaml`
  mapped to an uploaded file): not supported.
