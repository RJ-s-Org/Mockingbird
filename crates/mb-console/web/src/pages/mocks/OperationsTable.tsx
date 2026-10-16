import { useContract, type ContractDetail } from "../../api/queries";
import { DataTable, columnHelper } from "../../components/DataTable";
import { Drawer } from "../../components/Drawer";
import { ProblemAlert } from "../../components/ProblemAlert";

type Operation = ContractDetail["operations"][number];

const col = columnHelper<Operation>();
const columns = [
  col.accessor("method", { header: "Method", cell: (info) => <code>{info.getValue()}</code> }),
  col.accessor("path", { header: "Path", cell: (info) => <code className="break-all">{info.getValue()}</code> }),
  col.accessor("id", { header: "Operation ID" }),
  col.accessor("kind", { header: "Kind" }),
];

/** A drawer listing the operations of one contract version. */
export function OperationsTable({
  ws,
  mock,
  version,
  onClose,
}: {
  ws: string;
  mock: string;
  version: number;
  onClose: () => void;
}) {
  const contract = useContract(ws, mock, version);
  return (
    <Drawer title={`Operations of version ${version}`} onClose={onClose}>
      {contract.error ? <ProblemAlert error={contract.error} /> : null}
      {contract.data ? (
        <div className="space-y-3">
          <p className="text-sm text-slate-700">
            {contract.data.title} {contract.data.api_version}
          </p>
          <DataTable
            caption={`Operations of version ${version}`}
            columns={columns}
            data={contract.data.operations}
            empty="This version has no operations."
          />
        </div>
      ) : contract.isPending ? (
        <p className="text-slate-700">Loading…</p>
      ) : null}
    </Drawer>
  );
}
