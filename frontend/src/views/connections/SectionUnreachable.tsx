import { Card, CardContent } from "@/components/ui/card";

/**
 * The stand-in a connections section renders when the host could not answer its
 * read (issue #1470).
 *
 * Distinct from a 404, which means the surface is genuinely absent and the
 * section hides entirely. Here the host was reached but did not answer — a 5xx,
 * an expired session, a dropped connection — so the state is unknown, NOT empty.
 * Saying so keeps a transient failure from reading as "this host has no such
 * feature", which is what sent operators looking for a rebuild. Modelled on
 * `CompanyCredentialCard`'s error card.
 */
export function SectionUnreachable({ label }: { label: string }) {
  return (
    <Card>
      <CardContent>
        <p className="text-xs text-muted-foreground">
          {label} — the host could not answer, so this is unknown rather than absent. Reload to try
          again.
        </p>
      </CardContent>
    </Card>
  );
}
