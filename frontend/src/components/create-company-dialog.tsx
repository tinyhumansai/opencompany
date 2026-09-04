// Create a clean company, or reset a junk one, from the console (issue #1807).
//
// The host has provisioned a company over `POST /api/v1/companies` since #605,
// but nothing in the console reached it: an operator could archive or suspend a
// company (Settings → Lifecycle, #1401) yet had no way to make a fresh one, and
// no first-class "reset". This dialog is both.
//
// There is no purge route, so "reset" is not a wipe. The only honest reset the
// host can do is **archive the old company and provision a clean one**: archive
// retires it and removes it from the registry (its data is retained, not
// deleted — `server/provision.rs`), which also frees its id, name and quota slot
// for the replacement. So the reset path archives first, then creates.
//
// Every trigger for this dialog is gated on `client.carriesPlatformBearer`
// (see `canCreateCompanies`): provisioning, archiving and suspending are all
// `PlatformScope` routes a session cookie can never reach, so offering an
// enabled control to a magic-link operator would be the exact #1401
// dishonest-button bug one surface over. The gated call sites show a disabled
// control with an honest note instead.

import { useEffect, useState } from "react";
import { Loader2, TriangleAlert } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { adminEmailProblem } from "@/lib/company-setup";
import {
  MAX_EXPLICIT_ID_LENGTH,
  autoCompanyId,
  buildManifestToml,
  collidesWithArchived,
  describeProvisionError,
  explicitIdProblem,
  resetReplacementId,
  walletAddressProblem,
  wasAlreadyArchived,
  wasAmbiguousProvisionOutcome,
  wasArchiveOutcomeAmbiguous,
} from "@/lib/company-manifest";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { COMPANY_SWITCHING_HIDDEN } from "@/product-scope";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

/**
 * What the operator asked to open the dialog for.
 *
 * `create` is a clean new company. `reset` carries the company being replaced —
 * its id (to archive) and name (to prefill and to name in the copy).
 */
export type CreateCompanyRequest =
  { kind: "create" } | { kind: "reset"; company: string; name: string };

/** The host default the form starts on; the host injects this when omitted. */
const DEFAULT_POLICY_MODE = "auto";

/** The approval tiers the host accepts (`POLICY_MODES` in `company/types.rs`). */
const POLICY_MODES: { value: string; label: string }[] = [
  { value: "readonly", label: "Read-only — never acts on its own" },
  { value: "supervised", label: "Supervised — conservative execution" },
  { value: "auto", label: "Auto — balanced execution" },
  { value: "full", label: "Full — broadest execution" },
];

/** The honest line a gated trigger shows in place of an enabled control. */
export const CREATE_UNAVAILABLE_NOTE =
  "Creating a company needs a platform credential, which a person signed in here doesn't hold.";

/**
 * Whether this client can reach the provisioning + archive routes at all.
 *
 * A fact about the caller, not about the product: it answers "may this
 * principal create a company", and the dialog's own preflight and submit read
 * it for exactly that. It must stay free of product scope — a scope flag that
 * reached in here would make the shipped dialog inert while its tests went on
 * passing against logic nothing could run.
 */
export function canCreateCompanies(client: OpenCompanyClient): boolean {
  return client.carriesPlatformBearer;
}

/**
 * Whether a company-creation entry point should render.
 *
 * The presentation half, asked once where every trigger already asks: creation
 * is reachable from the switcher's "New company", the picker's own button, the
 * picker's per-card Reset, the no-company screen, and Settings' "Reset / Start
 * clean" — which archives and re-provisions through this same dialog. Gating
 * them one at a time is how four stayed live after the first was hidden.
 *
 * Separate from {@link canCreateCompanies} on purpose. "Do we offer this in
 * this product configuration" and "may this caller do it" are different
 * questions, and answering both with one predicate is what turned a hidden
 * button into a disabled code path.
 */
export function offersCompanyCreation(client: OpenCompanyClient): boolean {
  return !COMPANY_SWITCHING_HIDDEN && canCreateCompanies(client);
}

interface Props {
  client: OpenCompanyClient;
  /** The open request, or `null` when the dialog is closed. */
  request: CreateCompanyRequest | null;
  /**
   * Close the dialog (operator cancelled, or it finished).
   *
   * `archived` is true when a reset's archive leg already landed before the
   * dialog closed — cancelled after the archive, or after a create failure
   * the operator gave up retrying. The parent must refresh whatever roster
   * or company it is showing in that case: the company the picker or console
   * still displays is the one this reset just removed, and nothing else
   * tells it that (codex review on #1828, PR comment 3863028405).
   *
   * Also true when a provisioning attempt (reset or plain create) answered
   * ambiguously and this dialog could not safely reconcile it — see
   * `createMaybe` below. The company may already exist under the id this
   * attempt sent, even though the dialog is closing on an error: the parent
   * must refresh its roster so the operator can find it by hand (codex
   * review on #1828, PR comment 3874553506).
   */
  onClose: (archived: boolean) => void;
  /**
   * A company was provisioned. The parent switches the console into it; on a
   * reset the old company has already been archived by the time this fires.
   */
  onCreated: (status: CompanyStatus) => void;
}

export function CreateCompanyDialog({
  client,
  request,
  onClose,
  onCreated,
}: Props) {
  const [name, setName] = useState("");
  const [adminEmail, setAdminEmail] = useState("");
  const [wallet, setWallet] = useState("");
  // The host's sign-in mode, from the provisioning preflight. Defaults to
  // "email" — the mode a console-built manifest lands in when no host override
  // forces otherwise — and is upgraded to "wallet"/"none" once the preflight
  // resolves.
  const [authMode, setAuthMode] = useState<"wallet" | "email" | "none">("email");
  // The preflight could not be read, so the mode is unknown. Submit refuses
  // before the destructive archive leg rather than archiving blind.
  const [preflightFailed, setPreflightFailed] = useState(false);
  // The preflight is still in flight — `authMode` above is still on its
  // "email" default, not a confirmed answer. Starts `true` so the very first
  // render (before either `useEffect` below has run) already treats the mode
  // as unconfirmed, not "email". Without this, a wallet-mode host with a
  // slow preflight left the name field pre-filled and the submit button
  // enabled while `authMode` still read its email default and
  // `preflightFailed` was still false — an operator who typed an email and
  // submitted before the promise settled archived the existing company and
  // only then learned, from the provisioning response, that this host never
  // took an email at all (codex review on #1943, PR comment 3894416362).
  const [preflightPending, setPreflightPending] = useState(true);
  const [policyMode, setPolicyMode] = useState(DEFAULT_POLICY_MODE);
  const [explicitId, setExplicitId] = useState("");
  const [advanced, setAdvanced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Whether the reset's archive already landed, so a retry after a failed
  // *create* does not archive a second time (the id is already gone) and the
  // error copy can say the old company is already retired.
  const [archived, setArchived] = useState(false);
  // Whether the archive leg landed is UNKNOWN, as distinct from `archived`
  // being definitely false. Set when both the archive attempt's own response
  // AND the reconciliation status lookup that follows it fail — the "the
  // reconciliation lookup is itself ambiguous" branch below. `archived`
  // stays false there on purpose (a retry must remain possible: asserting
  // `true` would skip the archive call on retry for a company that might
  // still be live), but `onClose` cannot tell "definitely not archived" from
  // "don't know" from `archived` alone, and used to read the latter as the
  // former: an operator who closed the dialog here got `onClose(false)`, so
  // the parent skipped its roster refresh and persisted-default cleanup for
  // a company that may in fact already be gone (issue #1828 comment
  // 3873186315). OR'd into every `onClose` call below so any doubt forces a
  // refresh — a spurious refresh is cheap; a missed one leaves the console
  // scoped to an archived company.
  const [archiveMaybe, setArchiveMaybe] = useState(false);
  // Whether a provisioning attempt answered ambiguously
  // (`wasAmbiguousProvisionOutcome`) and the dialog reaches `submit`'s
  // ordinary error path anyway — either because the id was operator-typed
  // (reconciliation is deliberately skipped for those, see the comment at
  // the reconciliation lookup below) or because the self-generated id's own
  // reconciliation lookup also failed. Either way the company may already
  // exist under the id this attempt sent; the error shown ("already in
  // use", or the reset's "couldn't create") reads as a definite refusal, but
  // isn't necessarily one. Same treatment as `archiveMaybe` above: OR'd into
  // every `onClose` call so the parent refreshes its roster regardless,
  // letting the operator find the company by hand if it did land (codex
  // review on #1828, PR comment 3874553506) — a spurious refresh is cheap;
  // a missed one strands the operator on a stale roster with no way back
  // into a company that was, in fact, created.
  const [createMaybe, setCreateMaybe] = useState(false);
  // Whether the operator has directly edited the id field since the dialog
  // opened, as opposed to it merely holding the value `resetReplacementId`
  // pre-filled it with. Distinct from "is `explicitId` non-blank": a reset's
  // id field is *always* non-blank on open (see below), so blankness alone
  // cannot tell "the operator typed this" from "this is our own generated
  // default" — which `submit` needs to know before it may safely reconcile
  // an ambiguous provision response by looking the id up (an operator-typed
  // id could belong to someone else's company; a value we generated
  // ourselves cannot).
  const [idTouched, setIdTouched] = useState(false);

  const isReset = request?.kind === "reset";

  // Reset the form to the request each time the dialog opens. Keyed on the
  // request object so reopening for a different company re-seeds the name.
  useEffect(() => {
    if (!request) return;
    setName(request.kind === "reset" ? request.name : "");
    setAdminEmail("");
    setWallet("");
    setAuthMode("email");
    setPreflightFailed(false);
    // Re-armed on every open, gated the same way the fetch effect below is:
    // a client with no platform bearer never runs that fetch (its trigger
    // is already disabled per `canCreateCompanies`), so nothing will ever
    // resolve this — leaving it `true` would refuse every submit forever.
    setPreflightPending(canCreateCompanies(client));
    setPolicyMode(DEFAULT_POLICY_MODE);
    // Reset pre-seeds a fresh id rather than leaving this blank: the name
    // field above is pre-filled with the archived company's own name, and an
    // unset id here would have the host derive that same name back into the
    // same id (`company_id_from_name`) — reprovisioning over the archived
    // company's own durable record instead of a clean one. See
    // `resetReplacementId`. The operator can still overwrite it from
    // Advanced; `create` leaves this blank on open regardless — its name
    // field is still empty at this point, so there is nothing yet to derive
    // an id from. `submit` generates one (`autoCompanyId`) once a name
    // exists, same as the reset default, if the operator never fills this in.
    setExplicitId(
      request.kind === "reset" ? resetReplacementId(request.company) : "",
    );
    setAdvanced(false);
    setBusy(false);
    setError(null);
    setArchived(false);
    setArchiveMaybe(false);
    setCreateMaybe(false);
    setIdTouched(false);
  }, [request, client]);

  // Read the host's sign-in mode on open, so the form asks for a wallet address
  // on a wallet-mode host and an admin email otherwise — and so a wallet-mode
  // refusal is caught in `submit` BEFORE the reset's archive leg, not after it.
  // Only a platform bearer can reach the preflight; the triggers are already
  // gated on that, so a client without one keeps the default email behaviour.
  useEffect(() => {
    if (!request || !canCreateCompanies(client)) return;
    let cancelled = false;
    client
      .provisioningInfo()
      .then((info) => {
        if (!cancelled) {
          setAuthMode(info.auth_mode);
          setPreflightPending(false);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setPreflightFailed(true);
          setPreflightPending(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [request, client]);

  if (!request) return null;

  async function submit() {
    if (!request) return;
    const trimmedName = name.trim();
    if (!trimmedName || busy) return;

    // Validate every replacement field before the destructive archive leg
    // runs, not just before provisioning. A malformed admin email used to
    // reach the host only after the old company was already gone: the
    // manifest validator has no way to see it until `provisionCompany` is
    // called, which — on a reset — is the second half of the request, after
    // `client.lifecycle("archive", …)` already ran. The operator would see
    // "Archived X, but couldn't create the new company" for a typo that a
    // pure check catches for free. Same rule `company-setup.ts` uses for the
    // same reason (codex review on #1828, PR comment 3862711345).
    //
    // `required: true` — this dialog cannot tell whether the host has a
    // deployment-wide `OPENCOMPANY_ADMIN_EMAIL` bootstrap grant (`serve`
    // without a manager injecting it is a documented no-op, AGENTS.md
    // "OPENCOMPANY_ADMIN_EMAIL"), so it must not assume one exists the way
    // the help text below used to. Leaving this blank on a host with no
    // bootstrap admin provisions — or, on a reset, *reprovisions* —  a
    // company whose manifest names nobody: `no_env_admin_leaves_a_provisioned
    // _company_refusing_everyone` confirms no address can then sign in. On a
    // reset that is destructive: the usable old company is archived before
    // its now-inaccessible replacement exists (codex review on #1828, PR
    // comment 3864885200).
    // The preflight hasn't settled yet, so `authMode` is still on its "email"
    // default rather than a confirmed answer — the button is disabled for
    // this too (see the JSX below), but `submit` is reachable directly from a
    // still-in-flight click handler, and this is the check that actually
    // stops the destructive archive leg from racing the preflight promise.
    if (preflightPending) {
      setError(
        "Still checking this host's sign-in mode — try again in a moment.",
      );
      return;
    }

    // A preflight we could not read leaves the host's sign-in mode unknown, so
    // refuse before archiving rather than committing the destructive leg and
    // discovering the mode only when provisioning is refused.
    if (preflightFailed) {
      setError(
        "Couldn't check this host's sign-in mode, so nothing was changed. Try again.",
      );
      return;
    }

    // The required identity is mode-dependent, and — like the id checks below —
    // is validated BEFORE the destructive archive leg on a reset. A wallet-mode
    // host needs a wallet address; every other mode with a sign-in needs an
    // admin email. Discovering a bad or missing identity only after the archive
    // ran is the exact half-state this ordering exists to prevent.
    if (authMode === "wallet") {
      const problem = walletAddressProblem(wallet);
      if (problem) {
        setError(problem);
        return;
      }
    } else if (authMode !== "none") {
      const emailProblem = adminEmailProblem(adminEmail, true);
      if (emailProblem) {
        setError(emailProblem);
        return;
      }
    }

    // Derive the id this submission will actually send, up front — before
    // any validation and before the archive leg. `explicit` wins when the
    // operator typed one into Advanced; otherwise this is the exact same
    // fallback `resetReplacementId`/`autoCompanyId` compute further down for
    // a fresh id, just computed here instead so the checks below can see it.
    // Undoes a gap where the length check right after this validated only
    // the raw (often still-blank) Advanced field: a reset's fallback id is
    // `${request.company}-<suffix>`, so an existing company already close to
    // the bound the check enforces produced a fallback that blew past it —
    // discovered only after `provisionCompany` failed, with the old company
    // already archived (codex review on #1828, PR comments 3874344483 and
    // 3874326084).
    const explicit = explicitId.trim();
    const id =
      explicit ||
      (request.kind === "reset"
        ? resetReplacementId(request.company)
        : autoCompanyId(trimmedName));
    // Whether `id` is one this client generated itself, rather than one the
    // operator typed. NOT simply `!explicit`: a reset's Advanced field is
    // pre-filled with a generated id the moment the dialog opens (see the
    // `useEffect` above), so `explicit` is routinely non-blank even when the
    // operator never touched the field. `idTouched` is what actually tracks
    // operator intent — see its fuller explanation below, where this same
    // value is used to gate the reconciliation lookup.
    const selfGenerated = !idTouched || explicit === "";

    // Reject an id shape the host can never durably store, before the
    // destructive archive leg runs on a reset — not just before
    // provisioning. `explicitIdProblem` only checks the id's own shape
    // (independent of collisions, checked separately below); an id past
    // its length bound reaches `FsCompanyStore::load` as a non-`NotFound`
    // I/O error instead of the ordinary outcomes this client already
    // handles, and on a reset that is discovered only after the old
    // company is already gone (codex review on #1828, PR comment
    // 3873186322). Applies to a plain create too — same host-side failure,
    // just without the archive collateral. Checked against `id` — the id
    // that will actually be sent — not the raw Advanced field, so a
    // self-generated fallback that landed over the bound is caught here
    // too, not just an operator-typed one.
    const idProblem = explicitIdProblem(id);
    if (idProblem) {
      // `selfGenerated`, not `explicit` — a reset's Advanced field usually
      // already holds a generated id (see `selfGenerated`'s comment above),
      // so testing blankness here would tell an operator who never typed
      // anything to "leave the field blank", when it already is and that
      // changes nothing.
      //
      // Only the length bound is re-worded below into a fallback-specific
      // message — `autoCompanyId`/`resetReplacementId` only ever land over
      // that bound (both derive from a name or an existing id via a slug
      // function that already restricts itself to `[a-z0-9-]`, so they can
      // never trip `explicitIdProblem`'s charset check). The one case that
      // still could is `resetReplacementId` inheriting an unsafe id from a
      // company that predates the charset check (created before this
      // client enforced it, or provisioned outside this client entirely) —
      // for that, `idProblem` itself is shown rather than a hardcoded
      // length message that would misdescribe the actual problem.
      const lengthProblem = id.length > MAX_EXPLICIT_ID_LENGTH;
      setError(
        !selfGenerated
          ? idProblem
          : !lengthProblem
            ? idProblem
            : request.kind === "reset"
              ? `Couldn't generate a short enough replacement id for ${request.company} (would be ${id.length} characters) — type a shorter id in Advanced before continuing.`
              : `Couldn't generate a short enough id for "${trimmedName}" (would be ${id.length} characters) — type a shorter id in Advanced before continuing.`,
      );
      return;
    }

    // Reject a replacement id that is the same one about to be archived —
    // full id or, under shared-single-DB tenant namespacing, its bare form.
    // `resetReplacementId` seeds a fresh default, but the field stays
    // editable from Advanced, and typing the archived company's own id back
    // in — a likely move for an operator trying to keep the slug — recreates
    // the exact collision that default exists to avoid: `RuntimeBuilder::build`
    // reloads any existing durable record for an id before building over it,
    // so the "clean" replacement would come back carrying the archived
    // company's lifecycle, ledger and overlays. Caught before archiving, not
    // just before provisioning, so a bad id never leaves the operator with
    // the old company already gone and no way to retry cleanly (codex review
    // on #1828, PR comments 3861770475 and 3862711330).
    if (
      request.kind === "reset" &&
      collidesWithArchived(explicit, request.company)
    ) {
      setError(
        `The replacement id can't be ${request.company} — that's the company being archived. Leave the field blank for an auto-generated id, or choose a different one.`,
      );
      return;
    }

    setBusy(true);
    setError(null);

    // Archive FIRST on a reset — it frees the id, name and quota slot the new
    // company would otherwise collide with. Done once: `archived` guards a
    // retry after the *create* half failed.
    let didArchive = archived;
    if (request.kind === "reset" && !archived) {
      try {
        await client.lifecycle("archive", request.company);
        setArchived(true);
        didArchive = true;
      } catch (err) {
        if (wasAlreadyArchived(err)) {
          // Our own earlier attempt already archived it; the response just
          // never arrived. Proceed to the create leg instead of reporting a
          // failure that didn't happen.
          setArchived(true);
          didArchive = true;
        } else if (wasArchiveOutcomeAmbiguous(err)) {
          // This attempt's own reply may have been lost after the host
          // already archived the company — the same ambiguity
          // `wasAlreadyArchived` covers for a later retry's
          // `company_not_found`. Not just a dropped connection
          // (`network_error`): the host's `set_lifecycle` persists the
          // archived record *before* it appends the lifecycle event, and
          // `transition()` then reads status back before answering — either
          // step can fail AFTER the write already landed, and that failure
          // reaches this client as an ordinary error response (e.g.
          // `store_error`), indistinguishable by code alone from a request
          // that never wrote anything. Reconcile with a status lookup
          // instead of asserting "Nothing was changed", which may be false
          // (issue #1828 comments 3865803912 and 3874840062).
          try {
            const reconciled = await client.status(request.company);
            if (reconciled.lifecycle === "archived") {
              // The lookup resolved, but that alone doesn't mean "still
              // live": `set_lifecycle` persists the archived record to the
              // store BEFORE it appends the lifecycle event, and `archive`
              // (`src/server/provision.rs`) only removes the runtime from
              // the registry once `transition()` answers `200` — so a
              // post-write failure here leaves the runtime registered
              // (this lookup still resolves) with `archived` already
              // persisted. Reconcile the same as `wasAlreadyArchived`
              // below: the write landed, only the reply (or a step after
              // it) failed (codex review on #1828, PR comment 3874947935).
              setArchived(true);
              didArchive = true;
            } else {
              // Resolved AND not archived: the archive genuinely did not
              // take. This is a DEFINITIVE outcome, so it overrides any
              // `archiveMaybe` a still-earlier retry's own ambiguous
              // reconciliation left set — leaving that stale would have
              // Cancel/close call `onClose(true)` for a company this lookup
              // just confirmed is still live, wrongly telling the parent to
              // drop its persisted default and reload the picker (codex
              // review on #1828, PR comment 3874326104).
              setArchiveMaybe(false);
              setError(
                `Couldn't archive ${request.name}: ${describeProvisionError(err)} Nothing was changed.`,
              );
              setBusy(false);
              return;
            }
          } catch (lookupErr) {
            if (wasAlreadyArchived(lookupErr)) {
              // Gone: this attempt's own archive landed and only its reply
              // was lost.
              setArchived(true);
              didArchive = true;
            } else {
              // The reconciliation lookup is itself ambiguous — don't
              // assert either outcome. Leave `archived` false so a retry
              // stays possible, rather than closing over an unknown state,
              // but record the ambiguity separately so closing the dialog
              // from here still triggers the parent's refresh (see
              // `archiveMaybe`'s comment above).
              setArchiveMaybe(true);
              setError(
                `Couldn't confirm whether ${request.name} was archived: ${describeProvisionError(err)} Check the company roster before retrying.`,
              );
              setBusy(false);
              return;
            }
          }
        } else {
          // `wasArchiveOutcomeAmbiguous` now covers every `ApiError` the
          // host can throw for this call except `company_not_found`
          // (handled above), so reaching here means `err` isn't an
          // `ApiError` at all — a client-side exception before the request
          // could have reached the host (e.g. an encoding failure). Nothing
          // was sent, so "Nothing was changed" is safe to assert without a
          // reconciliation lookup.
          setError(
            `Couldn't archive ${request.name}: ${describeProvisionError(err)} Nothing was changed.`,
          );
          setBusy(false);
          return;
        }
      }
    }

    // `explicit`/`id` are already computed above (before the length check
    // and the archive leg) — both a reset and a plain create always send an
    // explicit id, even if the operator cleared the Advanced field back to
    // empty: falling through to the unset-id default would have the host
    // derive one itself — on a reset, re-deriving the archived company's own
    // id from the (possibly untouched) name field above (see
    // `resetReplacementId`); on a create, an id this client can never safely
    // reconcile against if the response is lost (see `autoCompanyId`).
    //
    // `selfGenerated` is already computed above (before the length check),
    // for the same reason `id`/`explicit` were hoisted — needed there to
    // pick the right validation message. Its own reasoning: whether `id` is
    // one this client generated itself, rather than one the operator typed.
    // Two cases count as ours: the field still holds exactly what the
    // fallback above seeded it with (`!idTouched`), or the operator cleared
    // it back to blank, which — per that same fallback — still lands on a
    // freshly generated id, never the unset-id default. Anything else the
    // operator has typed in is theirs; reconciling that by looking it up
    // could resolve to an unrelated, pre-existing company.
    const autoId = selfGenerated ? id : "";

    // Persist a self-generated id into state immediately, so a *retry*
    // reuses this exact id instead of minting a fresh one. A plain `create`
    // starts `explicitId` blank, and a `reset` normally doesn't — the
    // `useEffect` above seeds it with `resetReplacementId(request.company)`
    // the moment the dialog opens — but the Advanced field is editable on
    // both, and the reset dialog explicitly permits clearing it back to
    // blank. Either way, without this every submit with a blank field
    // recomputes `id` fresh (`autoCompanyId`/`resetReplacementId`, both
    // random-suffixed), so a retry after an ambiguous outcome mints a
    // *different* id than the one actually sent. That mattered whenever a
    // retry was actually needed: a request that succeeded on the host but
    // lost its response, where the immediate reconciliation lookup below
    // *also* failed transiently, left the dialog open for the operator to
    // retry — and that retry, with a different id, provisioned a second
    // company instead of reconciling the first (issue #1828 comments
    // 3865401532 and 3865689239). Skipped once the operator has typed an id
    // themselves (`explicit` non-blank): that value is already theirs to
    // keep across a retry.
    //
    // `idTouched` is reset alongside it. Reaching this branch on a reset
    // means the operator cleared the Advanced field back to blank — which
    // already set `idTouched` true — but the value being cached here is one
    // WE just generated, not one they typed. Leaving `idTouched` true would
    // make the *next* submit's `selfGenerated` computation above read this
    // cached, self-generated id as operator-typed, since by then `explicit`
    // is this same nonblank value: `autoId` would go empty and a retry that
    // lands on `company_exists` — this exact request having succeeded with
    // only its reply lost — would skip the reconciliation lookup that is
    // scoped to self-generated ids for exactly this id (issue #1828 comment
    // 3865803917).
    if (!explicit) {
      setExplicitId(id);
      setIdTouched(false);
    }

    try {
      const manifest_toml = buildManifestToml({
        name: trimmedName,
        adminEmail:
          authMode === "wallet" ? undefined : adminEmail.trim() || undefined,
        wallets: authMode === "wallet" ? [wallet.trim()] : undefined,
        // Omitted at the default so the host records its own `auto`, rather
        // than pinning the tier in the manifest text.
        policyMode: policyMode !== DEFAULT_POLICY_MODE ? policyMode : undefined,
      });
      const body: { manifest_toml: string; id?: string } = { manifest_toml };
      if (id) body.id = id;
      const status = await client.provisionCompany(body);
      onCreated(status);
    } catch (err) {
      // A dropped connection — or a retry that lands on the id this client
      // itself just asked for — is ambiguous by construction: the host may
      // have provisioned the company and only the reply, or the collision
      // check, makes it look like nothing happened. Reconcile with a status
      // lookup before reporting failure. Scoped to `autoId`, never an
      // operator-typed one: `resetReplacementId`'s random suffix can't
      // collide with a pre-existing company, so a hit there can only be this
      // request landing — an operator-typed id could genuinely belong to an
      // unrelated company, and switching the console into that would be
      // worse than the misleading error it replaces (codex review on #1828,
      // PR comment 3863028397).
      if (autoId && wasAmbiguousProvisionOutcome(err)) {
        try {
          onCreated(await client.status(autoId));
          return;
        } catch {
          // Not there under the bare id — on a shared-single-DB host the
          // company may still be there under its *namespaced* id. The host
          // applies `<tenant>--` prefixing server-side
          // (`namespaced_company_id`, `runtime/types.rs`) before storing
          // whatever id this client sent, but this client never learns its
          // own tenant namespace (`collidesWithArchived`'s comment says the
          // same) — so a direct `client.status(autoId)` 404s even though
          // provisioning fully succeeded. `listCompanies` answers with every
          // company's real, already-namespaced id, so match against that
          // instead of the id this client asked for (issue #1828 comment
          // 3865401513).
          try {
            const companies = await client.listCompanies();
            const match = companies.find(
              (c) => c.id === autoId || c.id.endsWith(`--${autoId}`),
            );
            if (match) {
              onCreated(match);
              return;
            }
          } catch {
            // Listing failed too — fall through to the ordinary error path.
          }
        }
      }
      // Reaching this point with an ambiguous outcome means reconciliation
      // either never ran (an operator-typed id — see the comment above) or
      // ran and came up empty (a self-generated id whose own lookup, and
      // its tenant-namespaced fallback, both failed). Either way the
      // company may already exist under the id just sent, unbeknownst to
      // this dialog: `register_and_report_status` on the host registers a
      // company before reading back its status for the response
      // (`server/provision.rs`), so a transient failure in that read still
      // leaves the company live, and a retry against the same id then
      // answers `company_exists` for a request that, from the operator's
      // side, looks like it never succeeded at all (issue #1828 comment
      // 3874553506). `setCreateMaybe` forces `onClose` to report `true`
      // regardless of how the operator closes from here, so the parent
      // refreshes its roster and the operator can find the company by hand.
      if (wasAmbiguousProvisionOutcome(err)) {
        setCreateMaybe(true);
      }
      // `!selfGenerated` — a `company_exists` refusal at this point is about
      // whatever the operator typed into the Advanced id field, not the name
      // field, and the message needs to point at the right one (see
      // `describeProvisionError`).
      const reason = describeProvisionError(err, !selfGenerated);
      // The dangerous half-state: the old company is archived but its
      // replacement did not land. Never swallow it — name both facts so the
      // operator understands the picker no longer lists the old company and
      // can retry the create (the archive won't repeat).
      setError(
        didArchive && request.kind === "reset"
          ? `Archived ${request.name}, but couldn't create the new company: ${reason} Adjust and try again.`
          : reason,
      );
    } finally {
      setBusy(false);
    }
  }

  const title = isReset ? "Reset / start clean" : "New company";
  const description = isReset
    ? `This archives ${request.name} (its data is retained, not deleted) and creates a fresh, empty company in its place.`
    : "Provision a clean company on this host. You'll land in it once it's created.";
  const submitLabel = isReset ? "Archive & start clean" : "Create company";

  return (
    // Ignore a dismiss request while busy — Escape and the built-in ✕ close
    // button both fire the same `onOpenChange(false)` as Cancel (already
    // `disabled={busy}` below), and without this gate they would bypass that
    // guard: `onClose` clears `request`, this component then renders `null`,
    // but the in-flight `submit()` keeps running against a dialog the
    // operator believes they dismissed. On a reset that is the dangerous
    // half-state made invisible — a late archive-succeeded/create-failed
    // writes its warning into a hidden dialog, and a late success still calls
    // `onCreated`, navigating the operator into a company they thought they
    // had cancelled out of.
    <Dialog
      open
      onOpenChange={(open) =>
        !open && !busy && onClose(archived || archiveMaybe || createMaybe)
      }
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>

        {error && (
          <Alert variant="destructive" data-testid="create-company-error">
            <TriangleAlert className="size-4" />
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <div className="grid gap-1.5">
          <Label htmlFor="create-company-name">Company name</Label>
          <Input
            id="create-company-name"
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. Acme Robotics"
            disabled={busy}
          />
        </div>

        {authMode === "wallet" ? (
          <div className="grid gap-1.5">
            <Label htmlFor="create-company-wallet">Admin wallet</Label>
            <Input
              id="create-company-wallet"
              value={wallet}
              onChange={(e) => setWallet(e.target.value)}
              placeholder="base58 wallet address that can sign in as an admin"
              disabled={busy}
              className="font-mono text-xs"
            />
            <p className="text-2xs text-muted-foreground">
              Required — this host signs users in with wallets, so a company
              provisioned with no admin wallet has nobody eligible to sign in.
            </p>
          </div>
        ) : (
          <div className="grid gap-1.5">
            <Label htmlFor="create-company-admin">Admin email</Label>
            <Input
              id="create-company-admin"
              type="email"
              value={adminEmail}
              onChange={(e) => setAdminEmail(e.target.value)}
              placeholder="who can sign in as an admin"
              disabled={busy}
            />
            <p className="text-2xs text-muted-foreground">
              Required — a company provisioned with no admin here has nobody
              eligible to sign in unless this host has its own bootstrap admin
              configured.
            </p>
          </div>
        )}

        <div className="grid gap-1.5">
          <button
            type="button"
            className="w-fit text-xs font-medium text-muted-foreground underline underline-offset-2 hover:text-foreground"
            onClick={() => setAdvanced((v) => !v)}
            aria-expanded={advanced}
          >
            {advanced ? "Hide advanced" : "Advanced"}
          </button>
          {advanced && (
            <div className="grid gap-3 rounded-lg border p-3">
              <div className="grid gap-1.5">
                <Label htmlFor="create-company-policy">Approval tier</Label>
                <Select
                  value={policyMode}
                  onValueChange={(v) =>
                    setPolicyMode((v as string) ?? DEFAULT_POLICY_MODE)
                  }
                  disabled={busy}
                >
                  <SelectTrigger id="create-company-policy">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {POLICY_MODES.map((mode) => (
                      <SelectItem key={mode.value} value={mode.value}>
                        {mode.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <p className="text-2xs text-muted-foreground">
                  Leave on Auto to use the host default.
                </p>
              </div>
              <div className="grid gap-1.5">
                <Label htmlFor="create-company-id">Company id (optional)</Label>
                <Input
                  id="create-company-id"
                  value={explicitId}
                  onChange={(e) => {
                    setExplicitId(e.target.value);
                    setIdTouched(true);
                  }}
                  placeholder={
                    isReset
                      ? "auto-generated, distinct from the archived id"
                      : "auto-generated from the name when left blank"
                  }
                  disabled={busy}
                  className="font-mono text-xs"
                />
              </div>
            </div>
          )}
        </div>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onClose(archived || archiveMaybe || createMaybe)}
            disabled={busy}
          >
            Cancel
          </Button>
          <Button
            variant={
              isReset ? "destructive" : name.trim() ? "default" : "secondary"
            }
            onClick={() => void submit()}
            disabled={busy || !name.trim() || preflightPending}
          >
            {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
            {submitLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
