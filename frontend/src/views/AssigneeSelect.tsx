// The task assignee picker (#263). The roster is a closed set — the company's
// desks and its teammates — and the host already enforces it at the write
// boundary (`src/runtime/assignee.rs`, reached from `server::ops::tasks`). A
// free-text field therefore turned a pick into a spelling test whose only
// feedback was a rejected submit. This is the one control every task surface
// uses instead: the board's create dialog, the edit dialog, and the detail
// screen's reassign row.
//
// Two invariants this component must not break, both from #205/#214:
//
//   1. **A desk assignment stays a desk.** `AssigneeResolution::links_working_agent`
//      is true only for `Unassigned | Agent(_)`, precisely so a card assigned to
//      a desk is never silently rewritten to that desk's lead. So the picker
//      submits the id it was given, verbatim, and never resolves a desk to its
//      lead here.
//   2. **Blank is a real choice**, not the absence of one — it hands the card to
//      the orchestrator (`resolve("") -> Unassigned -> canonical ""`). It gets
//      its own labelled row saying what it does.

import { useEffect, useMemo, useState } from "react";
import { AlertCircle } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { DeskDto, TeamMemberDto } from "@/api/types";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";

/**
 * The select's stand-in for the unassigned wire value.
 *
 * Base UI reads a value that stringifies to `""` as "nothing selected"
 * (`SelectRoot`'s `hasSelectedValue`), so binding the empty string directly
 * would put the trigger in its placeholder state and leave the Unassigned row
 * unticked — the deliberate choice would look like no choice at all. The
 * sentinel is mapped back at this component's edge, so consumers only ever see
 * the wire value: `""`, a desk id, or a teammate id.
 *
 * The leading space is what makes it uncollidable: `assignee::resolve` trims
 * its input before matching, so no canonical id the host stores can begin with
 * one.
 */
const UNASSIGNED = " unassigned";

const UNASSIGNED_LABEL = "Unassigned";

/** One pickable row, reduced to what the trigger and the list render. */
interface Option {
  /** React key — namespaced, because a desk id and a teammate id can collide. */
  key: string;
  /** The **wire** value, submitted verbatim: a desk id or a teammate id. */
  value: string;
  /** The primary label. */
  label: string;
  /** The muted trailing note (member count, role, or why it is unrecognised). */
  hint?: string;
  /** Set on the synthesised row for a value the roster no longer contains. */
  offRoster?: boolean;
}

export function AssigneeSelect({
  client,
  company,
  value,
  onChange,
  id,
  disabled,
  className,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** The current wire value: `""` (unassigned), a desk id, or a teammate id. */
  value: string;
  /** Receives the next wire value, verbatim — never resolved client-side. */
  onChange: (next: string) => void;
  /** Forwarded to the trigger so a `<Label htmlFor>` points at it. */
  id?: string;
  disabled?: boolean;
  className?: string;
}) {
  const [desks, setDesks] = useState<DeskDto[]>([]);
  const [team, setTeam] = useState<TeamMemberDto[]>([]);
  // Distinguishes "the roster has not arrived yet" from "the roster arrived and
  // this value is not on it". Without it, every assigned card would flash as
  // off-roster on the first paint.
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setLoaded(false);
    // Drop the previous company's roster with it. `loaded` alone only stops the
    // off-roster marker from flashing; the option lists would keep rendering
    // the old company's desks and teammates until the new fetch resolves, and a
    // pick made in that window would name something that does not exist here.
    setDesks([]);
    setTeam([]);
    // Both halves are best-effort, the stance `DesksView` already takes: a host
    // that does not serve one of these surfaces still gets a usable picker
    // rather than a dialog that fails to render.
    void Promise.all([
      client.listDesks(company).catch(() => [] as DeskDto[]),
      client.listTeam(company).catch(() => [] as TeamMemberDto[]),
    ]).then(([desksRes, teamRes]) => {
      if (cancelled) return;
      setDesks(desksRes);
      setTeam(teamRes);
      setLoaded(true);
    });
    return () => {
      cancelled = true;
    };
  }, [client, company]);

  const deskOptions = useMemo<Option[]>(
    () =>
      desks.map((desk) => ({
        key: `desk:${desk.id}`,
        value: desk.id,
        label: desk.name,
        // An empty desk stays listed on purpose: `AssigneeResolution::EmptyDesk`
        // is `names_something_real()`, so assigning work to a desk you are about
        // to staff is a legitimate write. Say that it is empty; don't hide it.
        hint:
          desk.members.length === 0
            ? "no members yet"
            : `${desk.members.length} teammate${desk.members.length === 1 ? "" : "s"}`,
      })),
    [desks],
  );

  const teamOptions = useMemo<Option[]>(() => {
    const deskIds = new Set(desks.map((d) => d.id));
    return (
      team
        // Desks resolve first (`assignee::resolve`), so on an id collision the
        // bare string can only ever come back as the desk. Offering the
        // teammate row would promise an assignment the host cannot honour.
        .filter((member) => !deskIds.has(member.id))
        .map((member) => ({
          key: `agent:${member.id}`,
          // Always the canonical id: the resolver matches the id namespace
          // before any display name, so this is the one key that can never be
          // shadowed by another teammate or come back ambiguous.
          value: member.id,
          // Manifest agents carry no `name`, so the id *is* the handle
          // operators know them by.
          label: member.name ?? member.id,
          hint: member.role,
        }))
    );
  }, [team, desks]);

  // A value the roster does not contain — a teammate since removed, a desk
  // since deleted, or something typed before this picker existed. It gets its
  // own row so the control renders what the card actually holds instead of
  // silently showing something else.
  const stale = useMemo<Option | null>(() => {
    if (!value) return null;
    if (deskOptions.some((o) => o.value === value)) return null;
    if (teamOptions.some((o) => o.value === value)) return null;
    // Only claim it is off-roster once a roster actually came back: a host that
    // served neither surface tells us nothing about this value.
    const known = loaded && (desks.length > 0 || team.length > 0);
    return {
      key: `stale:${value}`,
      value,
      label: value,
      hint: known ? "not on roster" : undefined,
      offRoster: known,
    };
  }, [value, deskOptions, teamOptions, loaded, desks.length, team.length]);

  /**
   * The trigger's rendering of a wire value.
   *
   * The off-roster warning is an icon here, not the words — the trigger is
   * often a third of a dialog row and an overlay teammate id already fills it,
   * so spelling it out would truncate the id away and leave a warning with no
   * subject. The icon keeps both visible; the words stay on the popup row
   * (which has the width) and in an `sr-only` span, so screen readers and text
   * assertions still get them.
   */
  const renderValue = useMemo(() => {
    const byValue = new Map<string, Option>();
    for (const option of [...deskOptions, ...teamOptions]) byValue.set(option.value, option);
    if (stale) byValue.set(stale.value, stale);
    return (wire: string) => {
      if (!wire) return <span className="min-w-0 truncate">{UNASSIGNED_LABEL}</span>;
      const option = byValue.get(wire);
      return (
        <>
          {option?.offRoster && (
            <>
              <AlertCircle
                className="size-3.5 shrink-0 text-amber-600 dark:text-amber-400"
                aria-hidden
              />
              <span className="sr-only">not on roster:</span>
            </>
          )}
          <span className="min-w-0 truncate">{option?.label ?? wire}</span>
        </>
      );
    };
  }, [deskOptions, teamOptions, stale]);

  return (
    <Select
      value={value === "" ? UNASSIGNED : value}
      // `null` arrives when Base UI clears the selection; it and the sentinel
      // mean the same wire value.
      onValueChange={(next) => onChange(next == null || next === UNASSIGNED ? "" : String(next))}
      disabled={disabled}
    >
      {/* `min-w-0` matters: the trigger's content is `whitespace-nowrap`, and a
          long overlay-teammate id would otherwise set the min-content width of
          whatever grid or flex track holds this control — squashing its
          neighbours (the edit dialog's Column and Priority) to nothing. */}
      <SelectTrigger id={id} className={cn("w-full min-w-0", className)}>
        <SelectValue>
          {(selected) => renderValue(selected === UNASSIGNED ? "" : String(selected ?? ""))}
        </SelectValue>
      </SelectTrigger>
      {/* The popup defaults to the trigger's width, which clips the trailing
          hints ("— hand it to the orchestrator", "— 2 teammates"). Let it grow
          to its content instead, bounded so a long role cannot run off-screen —
          the label truncates first. */}
      <SelectContent className="max-h-72 w-auto min-w-(--anchor-width) max-w-[min(26rem,90vw)]">
        <SelectGroup>
          <SelectItem value={UNASSIGNED}>
            <OptionRow label={UNASSIGNED_LABEL} hint="hand it to the orchestrator" />
          </SelectItem>
        </SelectGroup>

        {stale && (
          <>
            <SelectSeparator />
            <SelectGroup>
              <SelectItem value={stale.value}>
                <OptionRow label={stale.label} hint={stale.hint} warn={stale.offRoster} />
              </SelectItem>
            </SelectGroup>
          </>
        )}

        {deskOptions.length > 0 && (
          <>
            <SelectSeparator />
            <SelectGroup>
              <SelectLabel>Desks</SelectLabel>
              {deskOptions.map((option) => (
                <SelectItem key={option.key} value={option.value}>
                  <OptionRow label={option.label} hint={option.hint} />
                </SelectItem>
              ))}
            </SelectGroup>
          </>
        )}

        {teamOptions.length > 0 && (
          <>
            <SelectSeparator />
            <SelectGroup>
              <SelectLabel>Teammates</SelectLabel>
              {teamOptions.map((option) => (
                <SelectItem key={option.key} value={option.value}>
                  <OptionRow label={option.label} hint={option.hint} />
                </SelectItem>
              ))}
            </SelectGroup>
          </>
        )}
      </SelectContent>
    </Select>
  );
}

function OptionRow({ label, hint, warn }: { label: string; hint?: string; warn?: boolean }) {
  return (
    <>
      {warn && (
        <AlertCircle
          className="size-3.5 shrink-0 text-amber-600 dark:text-amber-400"
          aria-hidden
        />
      )}
      <span className="min-w-0 truncate">{label}</span>
      {hint && <span className="shrink-0 text-xs text-muted-foreground">— {hint}</span>}
    </>
  );
}
