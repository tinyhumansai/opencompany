import { cn } from "@/lib/utils";

import anthropicMark from "@/assets/provider-icons/anthropic.svg";
import appleMark from "@/assets/provider-icons/apple.svg";
import cerebrasMark from "@/assets/provider-icons/cerebras.svg";
import deepinfraMark from "@/assets/provider-icons/deepinfra.svg";
import deepseekMark from "@/assets/provider-icons/deepseek.svg";
import fireworksMark from "@/assets/provider-icons/fireworks.svg";
import gmiMark from "@/assets/provider-icons/gmi.ico";
import googleMark from "@/assets/provider-icons/google.svg";
import groqMark from "@/assets/provider-icons/groq.svg";
import huggingfaceMark from "@/assets/provider-icons/huggingface.svg";
import kilocodeMark from "@/assets/provider-icons/kilocode.ico";
import lmstudioMark from "@/assets/provider-icons/lmstudio.svg";
import minimaxMark from "@/assets/provider-icons/minimax.svg";
import modelscopeMark from "@/assets/provider-icons/modelscope.svg";
import moonshotMark from "@/assets/provider-icons/moonshot.svg";
import novitaMark from "@/assets/provider-icons/novita.svg";
import nvidiaMark from "@/assets/provider-icons/nvidia.svg";
import ollamaMark from "@/assets/provider-icons/ollama.svg";
import openaiMark from "@/assets/provider-icons/openai.svg";
import openrouterMark from "@/assets/provider-icons/openrouter.svg";
import orcarouterMark from "@/assets/provider-icons/orcarouter.ico";
import stepfunMark from "@/assets/provider-icons/stepfun.svg";
import sumopodMark from "@/assets/provider-icons/sumopod.ico";
import togetherMark from "@/assets/provider-icons/together.svg";
import veniceMark from "@/assets/provider-icons/venice.ico";
import vercelMark from "@/assets/provider-icons/vercel.svg";
import xaiMark from "@/assets/provider-icons/xai.svg";
import zaiMark from "@/assets/provider-icons/zai.ico";

/**
 * Brand marks for provider rows and the add-provider dialog.
 *
 * **Bundled, never fetched.** A console that pulled logos from a CDN would tell
 * whoever hosts it which providers a company is looking at, one request per row,
 * and the marks would vanish on an offline host. Provenance and licensing are in
 * `src/assets/provider-icons/README.md`.
 *
 * **No `react-icons` dependency.** Ten of these are Simple Icons marks, and the
 * path data was extracted into standalone SVGs rather than installing an 83 MB
 * package for ten files. The console already carries `lucide-react`, which has
 * no brand marks at all, so there was nothing to reuse.
 *
 * ## The letter is not a placeholder to be embarrassed about
 *
 * A provider with no sourced mark gets a lettered swatch, and that is the right
 * answer rather than a fallback: sourced marks cover roughly two-thirds of this
 * catalogue, and drawing approximations of the other companies' logos would be
 * worse than a letter that is at least unambiguous. `providerMark` returns
 * `null` instead of a generic cloud glyph so the caller's letter takes over —
 * twelve identical clouds carry less information than twelve letters.
 *
 * Mistral is the visible case: the Simple Icons release these came from carries
 * no Mistral mark, so it renders `M`.
 */
type Mark = { src: string; monochrome: boolean };

/**
 * Slug → mark. Keys are catalogue slugs, so a rename there shows up here as a
 * silently missing icon rather than a wrong one — which is why `iconCoverage`
 * in the tests pins these keys against the real provider list.
 *
 * `monochrome` says the file is a **silhouette with no background plate**. Those
 * are rendered as a mask over `currentColor` rather than as an image, because a
 * near-black glyph disappears against a dark row — and the alternative openhuman
 * uses (`brightness-0 invert`) only works when the swatch behind it is dark,
 * which assumes a theme.
 *
 * It is not "is the file one colour". A mark that paints a plate behind its
 * glyph masks to a solid square, because a mask reads alpha and a plate is
 * opaque everywhere. Those are images, and their own colours carry them in both
 * themes anyway.
 */
const MARKS: Record<string, Mark> = {
  // Codex signs in as an OpenAI credential and is stored under `openai`, so it
  // never reaches this map under its own name.
  openai: { src: openaiMark, monochrome: true },
  anthropic: { src: anthropicMark, monochrome: true },
  "claude-code": { src: anthropicMark, monochrome: true },
  google: { src: googleMark, monochrome: true },
  huggingface: { src: huggingfaceMark, monochrome: true },
  nvidia: { src: nvidiaMark, monochrome: true },
  "vercel-ai-gateway": { src: vercelMark, monochrome: true },
  xai: { src: xaiMark, monochrome: true },
  ollama: { src: ollamaMark, monochrome: true },
  omlx: { src: appleMark, monochrome: true },
  openrouter: { src: openrouterMark, monochrome: true },
  cerebras: { src: cerebrasMark, monochrome: true },
  deepinfra: { src: deepinfraMark, monochrome: true },
  deepseek: { src: deepseekMark, monochrome: true },
  fireworks: { src: fireworksMark, monochrome: true },
  // Not a silhouette: this file paints a full-bleed background plate behind the
  // glyph, so masking it yields a solid square of `currentColor`. Which is what
  // it did. The distinction is "does the file carry a plate", not "is it one
  // colour" — a plated mark has to be an image.
  groq: { src: groqMark, monochrome: false },
  lmstudio: { src: lmstudioMark, monochrome: true },
  minimax: { src: minimaxMark, monochrome: true },
  modelscope: { src: modelscopeMark, monochrome: true },
  moonshot: { src: moonshotMark, monochrome: true },
  novita: { src: novitaMark, monochrome: true },
  stepfun: { src: stepfunMark, monochrome: true },
  together: { src: togetherMark, monochrome: true },
  gmi: { src: gmiMark, monochrome: false },
  kilocode: { src: kilocodeMark, monochrome: false },
  orcarouter: { src: orcarouterMark, monochrome: false },
  sumopod: { src: sumopodMark, monochrome: false },
  venice: { src: veniceMark, monochrome: false },
  zai: { src: zaiMark, monochrome: false },
};

/** Slugs with a mark — exported for the coverage test, not for rendering. */
export const MARKED_SLUGS = Object.keys(MARKS);

/** Whether this slug has a sourced mark. */
export function hasMark(slug: string): boolean {
  return MARKS[slug.trim()] !== undefined;
}

/**
 * The mark for a provider slug, or `null` when none is shipped.
 *
 * `aria-hidden` in both branches: the provider's name is right beside it, and
 * announcing "O, OpenAI" helps nobody.
 */
export function ProviderMark({ slug, className }: { slug: string; className?: string }) {
  const mark = MARKS[slug.trim()];
  if (!mark) return null;
  if (mark.monochrome) {
    // A mask, not an image. The silhouette takes `currentColor`, so one file
    // reads correctly in both themes with no filter and no colour of its own.
    return (
      <span
        aria-hidden
        className={cn("bg-current", className)}
        style={{
          // **Quoted.** Vite inlines a small SVG as a `data:` URI, and these
          // files carry `fill:#7624F4` in a `<style>` block — an unquoted
          // `url()` treats that `#` as a fragment delimiter and truncates the
          // URI, so the mask fails to load and the element renders as a solid
          // block of `currentColor`. Which is exactly what it did.
          maskImage: `url("${mark.src}")`,
          WebkitMaskImage: `url("${mark.src}")`,
          maskRepeat: "no-repeat",
          WebkitMaskRepeat: "no-repeat",
          maskPosition: "center",
          WebkitMaskPosition: "center",
          maskSize: "contain",
          WebkitMaskSize: "contain",
        }}
      />
    );
  }
  return <img src={mark.src} alt="" aria-hidden className={cn("object-contain", className)} />;
}
