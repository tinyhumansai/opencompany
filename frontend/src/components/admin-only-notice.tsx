import { Info } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";

/**
 * Why a surface is read-only for the person looking at it.
 *
 * Rendered instead of — never alongside — the controls it explains. A page that
 * shows this and still offers an enabled Save has said two things and meant the
 * worse one.
 *
 * # What belongs in `title` and `children`
 *
 * `title` names the refusal from the viewer's side ("Only an admin can change
 * this company's model"), and `children` says *why the company draws the line
 * there*, then what the viewer can still do. The reason is the part that stops
 * this reading as an apology: a search provider decides whose retention policy
 * every teammate's query lands under, and someone told that will ask the right
 * admin for the right thing instead of assuming the page is broken.
 *
 * Do not write "you do not have permission" and stop. That is the sentence this
 * component exists to replace.
 */
export function AdminOnlyNotice({
  title,
  children,
  testId,
}: {
  /** The refusal, in the viewer's language. */
  title: string;
  /** Why the line is drawn here, and what the viewer can still do. */
  children: React.ReactNode;
  /**
   * Stable hook for the E2E spec that drives a real member through these pages.
   *
   * Each page keeps its own id rather than sharing one: the spec walks several
   * pages in a single context, and a shared id could not tell "the page I am on
   * explains itself" apart from "some page did".
   */
  testId: string;
}) {
  return (
    <Alert data-testid={testId}>
      <Info className="size-4" />
      <AlertTitle>{title}</AlertTitle>
      <AlertDescription>{children}</AlertDescription>
    </Alert>
  );
}
