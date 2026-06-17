import { Boxes } from "lucide-react";
import { cn } from "@/lib/cn";

// Brand marks are shipped as local SVG assets that use `fill="currentColor"`,
// so they inherit the surrounding `text-*` color and adapt to light/dark
// themes without needing a separate asset per theme.
import openaiBrand from "@/assets/brand/openai.svg?raw";
import anthropicBrand from "@/assets/brand/anthropic.svg?raw";
import geminiBrand from "@/assets/brand/googlegemini.svg?raw";
import deepseekBrand from "@/assets/brand/deepseek.svg?raw";
import moonshotBrand from "@/assets/brand/moonshot.svg?raw";
import xaiBrand from "@/assets/brand/xai.svg?raw";
import zhipuBrand from "@/assets/brand/zhipu.svg?raw";
import ollamaBrand from "@/assets/brand/ollama.svg?raw";
import awsBrand from "@/assets/brand/aws.svg?raw";
import zenmuxBrand from "@/assets/brand/zenmux.svg?raw";

// Brand icon lookup keyed by the provider catalog `id` (also the value
// persisted as `provider.vendor`). The supported *set* of vendors is
// decided by the server (`GET /admin/v1/provider-catalog`); this map only
// supplies a local brand mark for ids we have an asset for. Any id without
// an entry falls back to a generic Lucide icon, so adding a server-side
// provider never requires touching this file to remain functional.
const BRAND_BY_ID: Record<string, string> = {
  openai: openaiBrand,
  "openai-compatible": openaiBrand,
  openai_compatible: openaiBrand,
  anthropic: anthropicBrand,
  google: geminiBrand,
  gemini: geminiBrand,
  deepseek: deepseekBrand,
  moonshot: moonshotBrand,
  xai: xaiBrand,
  zhipu: zhipuBrand,
  ollama: ollamaBrand,
  bedrock: awsBrand,
  aws: awsBrand,
  zenmux: zenmuxBrand,
};

/**
 * Renders a vendor brand icon by catalog `id`. Inline SVGs inherit the
 * surrounding `text-*` color via `fill="currentColor"`, so the mark adapts
 * to dark mode. Ids without a local brand asset fall back to a generic
 * Lucide icon.
 */
export function VendorIcon({
  vendor,
  className,
}: {
  vendor: string;
  className?: string;
}) {
  const brand = BRAND_BY_ID[vendor];
  if (!brand) {
    return (
      <Boxes
        size={16}
        aria-label={vendor}
        className={cn("shrink-0 text-text-subtle", className)}
      />
    );
  }
  return (
    <span
      role="img"
      aria-label={vendor}
      className={cn(
        "inline-flex h-4 w-4 shrink-0 items-center justify-center [&_svg]:h-full [&_svg]:w-full",
        className,
      )}
      // The SVG markup is author-controlled and shipped as a local asset, so
      // it is safe to inject.
      dangerouslySetInnerHTML={{ __html: brand }}
    />
  );
}
