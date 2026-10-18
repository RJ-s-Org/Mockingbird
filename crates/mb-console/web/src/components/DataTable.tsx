import { Fragment, type ReactNode } from "react";
import { createColumnHelper, tableFeatures, useTable, type ColumnDef, type RowData } from "@tanstack/react-table";

/** The table features every console table uses (core only). */
export const features = tableFeatures({});
export type Features = typeof features;

/** A typed column helper for rows of type `T`. */
export function columnHelper<T extends RowData>() {
  return createColumnHelper<Features, T>();
}

/**
 * Renders a column's `header` or `cell` definition: a plain value as is, a
 * function by calling it. TanStack's `FlexRender` would instead mount the
 * function as a component, and since pages rebuild their column definitions
 * on every render, each render would then remount every cell: local state
 * (an open toggle) reset and a focused button lost focus. Called this way,
 * the elements a cell returns keep their identity across renders, so cell
 * renderers must not call hooks: put stateful UI in a component they return.
 */
function render(def: unknown, context: unknown, fallback: () => ReactNode): ReactNode {
  if (typeof def === "function") return (def as (c: unknown) => ReactNode)(context);
  return def === undefined ? fallback() : (def as ReactNode);
}

/** The classes of a table's horizontal scroll container (tables scroll, never the page). */
export const tableScroll = "max-w-full overflow-x-auto";

/**
 * A plain accessible table of `data`; `empty` is shown when there are no rows.
 * `expanded` may return content shown in a full-width row right under a row
 * (an inline editor, a detail pane); `null` keeps the row collapsed.
 * The table scrolls horizontally inside its own container on narrow screens.
 */
export function DataTable<T extends RowData>({
  caption,
  columns,
  data,
  empty = "Nothing here yet.",
  expanded,
}: {
  caption: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  columns: ColumnDef<Features, T, any>[];
  data: T[];
  empty?: string;
  expanded?: (row: T) => ReactNode;
}) {
  const table = useTable({ features, columns, data });
  const rows = table.getRowModel().rows;
  return (
    <div className={tableScroll}>
      <table className="w-full border-collapse text-left text-sm">
        <caption className="sr-only">{caption}</caption>
        <thead className="border-b border-slate-300 dark:border-slate-700 text-slate-700 dark:text-slate-300">
          {table.getHeaderGroups().map((group) => (
            <tr key={group.id}>
              {group.headers.map((header) => (
                <th key={header.id} scope="col" className="px-3 py-2 font-semibold">
                  {header.isPlaceholder
                    ? null
                    : render(header.column.columnDef.header, header.getContext(), () => header.column.id)}
                </th>
              ))}
            </tr>
          ))}
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td colSpan={columns.length} className="px-3 py-6 text-center text-slate-600 dark:text-slate-400">
                {empty}
              </td>
            </tr>
          ) : (
            rows.map((row) => {
              const detail = expanded?.(row.original);
              const open = detail !== undefined && detail !== null && detail !== false;
              return (
                <Fragment key={row.id}>
                  <tr
                    className={
                      open
                        ? "bg-slate-100/70 dark:bg-slate-800/60"
                        : "border-b border-slate-200 dark:border-slate-800"
                    }
                  >
                    {row.getAllCells().map((cell) => (
                      <td key={cell.id} className="px-3 py-2 align-top">
                        {render(cell.column.columnDef.cell, cell.getContext(), () => {
                          const v = cell.getValue();
                          return v == null ? null : String(v);
                        })}
                      </td>
                    ))}
                  </tr>
                  {open ? (
                    <tr className="border-b border-slate-200 dark:border-slate-800 bg-slate-100/70 dark:bg-slate-800/60">
                      <td colSpan={columns.length} className="px-3 pb-4">
                        {detail}
                      </td>
                    </tr>
                  ) : null}
                </Fragment>
              );
            })
          )}
        </tbody>
      </table>
    </div>
  );
}
