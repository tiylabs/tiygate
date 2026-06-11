import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { auditApi } from "@/api/resources";
import { Badge, Card, ErrorBox, Spinner, Table, Td, Th } from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";

export default function Audit() {
  const { t } = useTranslation();
  const { data, isLoading, error } = useQuery({
    queryKey: ["audit"],
    queryFn: () => auditApi.list(100),
  });

  return (
    <div>
      <PageHeader title={t("audit.title")} />
      {error ? <ErrorBox message={(error as Error).message} /> : null}
      <Card>
        {isLoading ? (
          <Spinner />
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>{t("audit.ts")}</Th>
                <Th>{t("audit.actor")}</Th>
                <Th>{t("audit.action")}</Th>
                <Th>{t("audit.target")}</Th>
                <Th>{t("audit.details")}</Th>
              </tr>
            </thead>
            <tbody>
              {(data ?? []).map((e) => (
                <tr key={e.id}>
                  <Td className="text-xs text-slate-500">{fmtTime(e.ts)}</Td>
                  <Td>{e.actor}</Td>
                  <Td>
                    <Badge>{e.action}</Badge>
                  </Td>
                  <Td className="text-xs">
                    {e.target_type}/{e.target_id}
                  </Td>
                  <Td className="max-w-[280px] truncate font-mono text-xs text-slate-500">
                    {typeof e.details === "string"
                      ? e.details
                      : JSON.stringify(e.details)}
                  </Td>
                </tr>
              ))}
              {(data ?? []).length === 0 && !isLoading ? (
                <tr>
                  <Td className="text-slate-400">{t("common.empty")}</Td>
                </tr>
              ) : null}
            </tbody>
          </Table>
        )}
      </Card>
    </div>
  );
}
