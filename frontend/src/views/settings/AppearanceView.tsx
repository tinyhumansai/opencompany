// Light, dark, or follow the system — and, since issue #2493, the accent.
//
// A row on the rail rather than a card most of the way down General, and the
// reason is what it belongs to. Every other thing on General is a fact about
// the *company* — its connection, its domain, its mail, its lifecycle — and is
// the same for everyone who signs in. The theme is a fact about **this
// browser**: it is stored per client, changing it changes nothing for anybody
// else, and it is the one control on that page an operator goes looking for by
// name rather than meeting on the way past.
//
// Two cards now, not one: mode and accent are independent axes (a preset holds
// across a light/dark switch), so they get separate controls rather than one
// getting folded into the other's `CardAction`. Theme stays first — the theme
// e2e spec (`test/e2e/theme-toggle-visible.spec.ts`) measures it sitting below
// the fold at a short viewport, and putting Accent above it would change what
// that spec is testing (`roadblocks.md` R15).

import { PageHeader } from "@/components/page-header";
import { ThemeToggle } from "@/components/theme-toggle";
import { AccentPresetPicker } from "@/components/accent-preset-picker";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

export function AppearanceView() {
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Appearance" width="full" />
      <div className="min-h-0 w-full flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {/* The trailing control goes in `CardAction`, not a bare child:
            `CardHeader` is a grid, so `flex-row justify-between` on it is inert
            and the control drops onto a row of its own below the description.
            `CardAction` is what switches the header to `grid-cols-[1fr_auto]`
            and parks the control at the top right. */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Theme</CardTitle>
            <CardDescription>Switch between light, dark, and system themes.</CardDescription>
            <CardAction>
              <ThemeToggle />
            </CardAction>
          </CardHeader>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Accent</CardTitle>
            <CardDescription>
              Choose the interaction hue — the colour of buttons, links, and the active
              nav row. Independent of light and dark.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <AccentPresetPicker />
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
