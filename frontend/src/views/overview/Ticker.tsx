// The live strip: the company's most recent movements, scrolling past.
//
// The track holds two identical copies of the run and translates by exactly
// half its width, so the loop is seamless without measuring anything. It pauses
// on hover — a line you want to read should stop — and does not animate at all
// under `prefers-reduced-motion`.

import { TONE_TEXT } from "./palette";
import type { TickerItem } from "./types";

const STYLE = `
@keyframes oc-ticker { from { transform: translateX(0); } to { transform: translateX(-50%); } }
.oc-ticker-track { display: flex; width: max-content; animation: oc-ticker 42s linear infinite; }
.oc-ticker:hover .oc-ticker-track { animation-play-state: paused; }
@media (prefers-reduced-motion: reduce) { .oc-ticker-track { animation: none; } }
`;

export function Ticker({ items }: { items: TickerItem[] }) {
  if (items.length === 0) {
    return (
      <div className="flex items-center gap-3 rounded-xl border border-dashed px-4 py-3 text-sm text-muted-foreground">
        <span className="size-1.5 shrink-0 rounded-full bg-muted-foreground/50" />
        Nothing has moved yet — the strip fills in as your company works.
      </div>
    );
  }

  return (
    <div className="oc-ticker relative overflow-hidden rounded-xl border bg-card">
      <style dangerouslySetInnerHTML={{ __html: STYLE }} />
      <div className="absolute inset-y-0 left-0 z-10 flex items-center gap-2 border-r bg-card px-3 font-mono text-[9.5px] uppercase tracking-[0.18em] text-muted-foreground">
        <span className="relative flex size-1.5">
          <span className="absolute inline-flex size-full animate-ping rounded-full bg-[#008300] opacity-60" />
          <span className="relative inline-flex size-1.5 rounded-full bg-[#008300]" />
        </span>
        live
      </div>
      <div className="oc-ticker-track py-2.5 pl-[76px]">
        {[0, 1].map((copy) => (
          <div key={copy} className="flex shrink-0 gap-7 pr-7" aria-hidden={copy === 1}>
            {items.map((item) => (
              <span
                key={`${copy}-${item.id}`}
                className="inline-flex items-center gap-2 whitespace-nowrap font-mono text-[11px]"
              >
                <b className={TONE_TEXT[item.tone]}>{item.mark}</b>
                <span className="text-foreground/80">{item.subject}</span>
                <span className="text-muted-foreground">{item.detail}</span>
                <span className="text-border">·</span>
              </span>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}
