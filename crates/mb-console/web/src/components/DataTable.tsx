import { createColumnHelper, tableFeatures, useTable, type ColumnDef, type RowData } from "@tanstack/react-table";

/** The table features every console table uses (core only). */
export const features = tableFeatures({});
export type Features = typeof features;

/** A typed column helper for rows of type `T`. */
export function columnHelper<T extends RowData>() {
  return createColumnHelper<Features, T>();
}

/** A plain accessible table of `data`; `empty` is shown when there are no rows. */
export function DataTable<T extends RowData>({
  caption,
  columns,
  data,
  empty = "Nothing here yet.",
}: {
  caption: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  columns: ColumnDef<Features, T, any>[];
  data: T[];
  empty?: string;
}) {
  const table = useTable({ features, columns, data });
  const rows = table.getRowModel().rows;
  return (
    <table className="w-full border-collapse text-left text-sm">
      <caption className="sr-only">{caption}</caption>
      <thead className="border-b border-slate-300 text-slate-700">
        {table.getHeaderGroups().map((group) => (
          <tr key={group.id}>
            {group.headers.map((header) => (
              <th key={header.id} scope="col" className="px-3 py-2 font-semibold">
                {header.isPlaceholder ? null : <table.FlexRender header={header} />}
              </th>
            ))}
          </tr>
        ))}
      </thead>
      <tbody>
        {rows.length === 0 ? (
          <tr>
            <td colSpan={columns.length} className="px-3 py-6 text-center text-slate-600">
              {empty}
            </td>
          </tr>
        ) : (
          rows.map((row) => (
            <tr key={row.id} className="border-b border-slate-200">
              {row.getAllCells().map((cell) => (
                <td key={cell.id} className="px-3 py-2 align-top">
                  <table.FlexRender cell={cell} />
                </td>
              ))}
            </tr>
          ))
        )}
      </tbody>
    </table>
  );
}
