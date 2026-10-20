import { useContract, type ContractDetail } from "../../api/queries";
import { DataTable, columnHelper } from "../../components/DataTable";
import { Drawer } from "../../components/Drawer";
import { ProblemAlert } from "../../components/ProblemAlert";
import { Badge, Loading } from "../../components/ui";

type Operation = ContractDetail["operations"][number];

const col = columnHelper<Operation>();
// `target` covers every protocol (`GET /orders/{id}`, `query user`,
// `/pkg.Service/Method`, `Service/Port Operation`); `method` and `path` are
// empty outside HTTP.
const columns = [
  col.accessor("protocol", { header: "Protocol", cell: (info) => <Badge>{info.getValue()}</Badge> }),
  col.accessor("target", { header: "Target", cell: (info) => <code className="break-all">{info.getValue()}</code> }),
  col.accessor("id", { header: "Operation ID", cell: (info) => <span className="break-all">{info.getValue()}</span> }),
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
        <div className="space-y-4">
          <p className="text-sm text-fg-muted">
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
        <Loading />
      ) : null}
    </Drawer>
  );
}
