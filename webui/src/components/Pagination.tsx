import { useEffect, useState } from "react";

export interface PaginationLabels {
  pageSizeLabel: string;
  pageSizeOption: string;
  total: string;
  range: string;
  pageOf: string;
  first: string;
  prev: string;
  next: string;
  last: string;
  goTo: string;
  go: string;
}

interface PaginationProps {
  page: number;
  pageCount: number;
  total: number;
  limit: number;
  offset: number;
  pageSizeOptions: readonly number[];
  onPageChange: (next: number) => void;
  onPageSizeChange: (next: number) => void;
  labels: PaginationLabels;
}

function interpolate(template: string, values: Record<string, string | number>) {
  return template.replace(/{{\s*(\w+)\s*}}/g, (match, key) =>
    Object.prototype.hasOwnProperty.call(values, key) ? String(values[key]) : match,
  );
}

export function Pagination({
  page,
  pageCount,
  total,
  limit,
  offset,
  pageSizeOptions,
  onPageChange,
  onPageSizeChange,
  labels,
}: PaginationProps) {
  const rangeStart = total === 0 ? 0 : offset + 1;
  const rangeEnd = Math.min(offset + limit, total);
  return (
    <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 border-t border-border px-4 py-3 text-sm text-text-muted">
      <label className="flex items-center gap-2">
        <span>{labels.pageSizeLabel}</span>
        <select
          aria-label={labels.pageSizeLabel}
          className="h-9 rounded-md border border-border bg-surface px-2 text-sm text-text outline-none focus:border-accent"
          value={limit}
          onChange={(e) => onPageSizeChange(Number(e.target.value))}
        >
          {pageSizeOptions.map((n) => (
            <option key={n} value={n}>
              {interpolate(labels.pageSizeOption, { count: n })}
            </option>
          ))}
        </select>
      </label>
      <span className="tabular-nums">
        {total === 0
          ? interpolate(labels.total, { count: 0 })
          : interpolate(labels.range, {
              from: rangeStart,
              to: rangeEnd,
              total,
            })}
        <span className="mx-2 text-text-subtle">·</span>
        {interpolate(labels.pageOf, { page, total: pageCount })}
      </span>
      <PageNav
        page={page}
        pageCount={pageCount}
        onChange={onPageChange}
        labels={labels}
      />
    </div>
  );
}

function PageNav({
  page,
  pageCount,
  onChange,
  labels,
}: {
  page: number;
  pageCount: number;
  onChange: (next: number) => void;
  labels: Pick<PaginationLabels, "first" | "prev" | "next" | "last" | "goTo" | "go">;
}) {
  const visible = buildVisiblePages(page, pageCount);
  const atFirst = page <= 1;
  const atLast = page >= pageCount;
  const baseBtn =
    "inline-flex h-8 min-w-[2rem] items-center justify-center rounded-md border border-border bg-surface px-2 text-xs text-text transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-surface";
  const activeBtn =
    "inline-flex h-8 min-w-[2rem] items-center justify-center rounded-md border border-accent bg-accent/10 px-2 text-xs font-semibold text-accent";
  return (
    <div className="flex flex-wrap items-center gap-1">
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.first}
        disabled={atFirst}
        onClick={() => onChange(1)}
      >
        «
      </button>
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.prev}
        disabled={atFirst}
        onClick={() => onChange(page - 1)}
      >
        ‹
      </button>
      {visible.map((item, i) =>
        item === "…" ? (
          <span
            key={`gap-${i}`}
            className="inline-flex h-8 min-w-[2rem] items-center justify-center text-xs text-text-subtle select-none"
          >
            …
          </span>
        ) : (
          <button
            key={item}
            type="button"
            className={item === page ? activeBtn : baseBtn}
            aria-current={item === page ? "page" : undefined}
            disabled={item === page}
            onClick={() => onChange(item)}
          >
            {item}
          </button>
        ),
      )}
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.next}
        disabled={atLast}
        onClick={() => onChange(page + 1)}
      >
        ›
      </button>
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.last}
        disabled={atLast}
        onClick={() => onChange(pageCount)}
      >
        »
      </button>
      <GotoPage page={page} pageCount={pageCount} onChange={onChange} labels={labels} />
    </div>
  );
}

function GotoPage({
  page,
  pageCount,
  onChange,
  labels,
}: {
  page: number;
  pageCount: number;
  onChange: (next: number) => void;
  labels: Pick<PaginationLabels, "goTo" | "go">;
}) {
  const [draft, setDraft] = useState<string>(String(page));
  useEffect(() => setDraft(String(page)), [page]);
  function commit() {
    const n = Number(draft);
    if (!Number.isFinite(n)) {
      setDraft(String(page));
      return;
    }
    const clamped = Math.max(1, Math.min(pageCount, Math.trunc(n)));
    setDraft(String(clamped));
    if (clamped !== page) onChange(clamped);
  }
  return (
    <form
      className="ml-2 flex items-center gap-1"
      onSubmit={(e) => {
        e.preventDefault();
        commit();
      }}
    >
      <span className="text-xs text-text-subtle">{labels.goTo}</span>
      <input
        type="number"
        min={1}
        max={pageCount}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        className="h-8 w-14 rounded-md border border-border bg-surface px-2 text-xs text-text outline-none focus:border-accent tabular-nums"
      />
      <button
        type="submit"
        className="inline-flex h-8 items-center rounded-md border border-border bg-surface px-2 text-xs text-text hover:bg-surface-muted"
      >
        {labels.go}
      </button>
    </form>
  );
}

function buildVisiblePages(
  page: number,
  pageCount: number,
): (number | "…")[] {
  if (pageCount <= 7) {
    return Array.from({ length: pageCount }, (_, i) => i + 1);
  }
  const set = new Set<number>([1, pageCount, page]);
  for (let i = page - 2; i <= page + 2; i += 1) {
    if (i >= 1 && i <= pageCount) set.add(i);
  }
  const sorted = Array.from(set).sort((a, b) => a - b);
  const out: (number | "…")[] = [];
  for (let i = 0; i < sorted.length; i += 1) {
    if (i > 0 && sorted[i] - sorted[i - 1] > 1) out.push("…");
    out.push(sorted[i]);
  }
  return out;
}
