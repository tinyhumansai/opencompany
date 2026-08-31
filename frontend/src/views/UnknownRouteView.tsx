import { MapPinOff } from "lucide-react";

import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { withHostParam } from "@/hooks/use-host-route";

/** Explains a hash address that the console does not recognize. */
export function UnknownRouteView({ address }: { address: string | null }) {
  const path = address ? `#/${address}` : "that address";

  return (
    <div className="flex flex-1 items-center justify-center p-6">
      {/*
        Issue #1763: `hidden`, because the card *is* the page — a title bar
        over a centred recovery card would be chrome above the one thing on
        screen. What it was missing is the other half: with no `h1` at all,
        this was a page a screen reader could not announce, and it is the page
        an operator lands on precisely when they are already lost.
      */}
      <PageHeader title="Page not found" hidden />
      <Card className="w-full max-w-lg">
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <MapPinOff className="size-4" /> Page not found
          </CardTitle>
          <CardDescription>
            {path} does not name a page in this console. Check the address or return to Overview.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {/* Same host-scope rule as every recovery anchor: a new tab boots with
              the address as written, and a `#/overview` without `?host=` would
              land on the bootstrap/default host instead of the one the operator
              was on (issue #1417 review). */}
          <Button render={<a href={withHostParam("overview")} />}>Go to Overview</Button>
        </CardContent>
      </Card>
    </div>
  );
}
