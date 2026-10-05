// The first-run setup wizard.
//
// One flow that configures an instance: pick a company template, choose how
// people sign in, point the brain at a credential, review the tool surfaces
// this build has, and commit. Before this existed the same decisions were
// spread across a hand-edited `config.toml`, a `serve --company` flag and six
// Settings sub-pages, and a freshly spun-up harness with no company dead-ended
// on "No companies are running on this host".
//
// ## What it writes, and what it only stages
//
// Everything here lands in `config.toml`, which is the *second* precedence
// layer (`env ⟵ config.toml ⟵ manifest ⟵ default`). Two consequences the UI
// has to be honest about rather than hide:
//
//   - A field the environment owns cannot be written at all. The host reports
//     `editable: false` for those and refuses the write; we render them
//     read-only with the owning layer shown, so nobody submits a change that
//     silently does nothing.
//   - Host-level fields are read once, at boot, so a change to some of them is
//     *staged* rather than applied. The host applies what it can in place (it
//     rebuilds companies for a new sign-in mode) and reports what is genuinely
//     left; the completion screen shows that answer, not a guess, and never
//     implies its own button performed the restart.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, ExternalLink, Loader2, Lock, RotateCw } from "lucide-react";

import { loginWithPassword, type SignIn } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import type { AddProviderInput } from "@/api/inference";
import { SETUP_HANDOFF_FRAGMENT } from "@/setup/state";
import {
  changedFields,
  fieldsFor,
  getSetup,
  SETUP_INFERENCE_OPTIONS,
  proposeSetupRoster,
  testInference,
  submitSetup,
  type SetupApplied,
  type SetupField,
  type SetupRoster,
  type SetupStatus,
} from "@/api/setup";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { OnboardingShell } from "@/components/onboarding-shell";
import { PageHeader } from "@/components/page-header";
import { Button, buttonVariants } from "@/components/ui/button";
import { useOptionalHosts } from "@/connections/HostsContext";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { NewPasswordField } from "@/components/new-password-field";
import { generatePassword, passwordProblem } from "@/lib/generate-password";
import { Textarea } from "@/components/ui/textarea";
import { clampToSetupCompanyNameLimit } from "@/lib/company-name";
import { TEAM_TONES, initials, toneFor } from "@/lib/team";
import { fieldCopy, fieldPlaceholder } from "@/lib/setup-fields";
import type { Step } from "@/components/ui/stepper";
import {
  adminEmailProblem,
  emptySetupDraft,
  jobItems,
  type SetupDraft,
} from "@/lib/company-setup";
import { isDesktopRuntime } from "@/api/transport";
import { probeEndpoint } from "@/inference/connect";
import { TINYHUMANS_API_KEYS_URL } from "@/lib/links";
import { SelfManagedConnectStep } from "./SelfManagedConnectStep";
import type { ComposioDraft } from "./SelfManagedConnectStep";
import { cn } from "@/lib/utils";
import { HOST_SETTINGS_HIDDEN } from "@/product-scope";

/**
 * The flow, in order. `fields` names the config keys each step owns.
 *
 * The order is the whole design. Cheap questions about *them* come before the
 * asks that cost something, because nobody abandons "what do you sell" and
 * plenty abandon `bind` — and everything with a working default stays behind
 * Advanced, since a knob that already works is not a decision worth a screen.
 * Two steps are placed against that grain, each for a reason:
 *
 * **Model comes first, and it did not used to.** It sat third, after the
 * questions, on the reasoning that cheap interesting questions earn the right to
 * ask for a credential. That reasoning was sound about *motivation* and wrong
 * about *consequence*: the design pass is silent when a credential is missing or
 * bad — it falls back to a curated team — so a wrong key produced a plausible
 * company and an operator found out two screens later, if at all. The one step
 * whose failure invalidates every answer after it belongs before them. It is a
 * gate, not a wall: the step can be skipped outright, and skipping is what the
 * curated fallback is for.
 *
 * **Sign-in comes before the address it decides the need for.** It is the other
 * step whose answer changes what follows: on `none` there is nobody to invite,
 * so the address screen is not shown at all (see `visibleSteps`). This flow used
 * to ask the address on step three and offer the choice on step four, buried in
 * Advanced under copy inviting the operator to press straight past it — so
 * someone on a laptop was asked for an address they need never have supplied,
 * by a wizard that already knew it might not want one. It does not move any
 * further forward than this: it is a question about the machine, and the
 * questions about *them* are what earn the right to ask one of those.
 */
const STEPS: readonly (Step & { fields: readonly string[] })[] = [
  { id: "setup-way", label: "Setup", fields: [] },
  { id: "managed-login", label: "Connect", fields: ["tinyhumans_api_key"] },
  { id: "self-managed-connect", label: "Connect", fields: ["tinyhumans_api_key"] },
  { id: "business", label: "Business", fields: [] },
  { id: "signin", label: "Sign-in", fields: ["auth_mode"] },
  { id: "account", label: "You", fields: [] },
  { id: "advanced", label: "Advanced", fields: [] },
  { id: "review", label: "Review", fields: [] },
];

/** How the operator wants this instance set up, as answered on step 0. */
type SetupWay = "managed" | "self-managed";

/** The step-1 screen each way leads to. */
const STEP_ONE_FOR: Record<SetupWay, string> = {
  managed: "managed-login",
  "self-managed": "self-managed-connect",
};

/** The branch: step 0 and the two step-1 screens it chooses between. */
const BRANCH_STEP_IDS: readonly string[] = ["setup-way", ...Object.values(STEP_ONE_FOR)];

/**
 * The route a TinyHumans account key serves — the managed branch's only one.
 *
 * Not a provider the company declares. A key tested against this is stored as
 * the company's own credential and fanned out from there, where every other
 * route's key is written onto the manifest as that company's provider.
 */
const MANAGED_PROVIDER = "managed";

/** Whether a step id is one of the two step-1 screens. */
function isStepOne(id: string): boolean {
  return id === STEP_ONE_FOR.managed || id === STEP_ONE_FOR["self-managed"];
}

/** How each setup way is described, in what the operator gets rather than in mechanism. */
const SETUP_WAY_COPY: Record<SetupWay, { label: string; hint: string }> = {
  managed: {
    label: "Managed with TinyHumans",
    hint: "One key covers the model, the integrations and search. We look after the credentials.",
  },
  "self-managed": {
    label: "Set it up yourself",
    hint: "Bring your own provider and your own integration keys. Nothing is brokered for you.",
  },
};

/**
 * Where a TinyHumans key is minted by hand: the API-keys tab of the hub **this
 * host is on**, as the host reports it (`inference.keys_url`). Not a constant:
 * a host on staging sends its operator to staging, because a key minted on
 * production would be refused by the platform every other surface of this
 * host talks to. A host too old to report one falls back to the production
 * link the console always carried; a host whose `api_url` follows no known
 * convention reports `null`, and then there is no link to offer.
 */
function tinyhumansKeySource(
  status: SetupStatus | null,
): { label: string; url: string } | undefined {
  const url =
    status?.inference.keys_url === undefined ? TINYHUMANS_API_KEYS_URL : status.inference.keys_url;
  return url ? { label: "TinyHumans", url } : undefined;
}


/**
 * Advanced: the settings that already work, grouped by subject.
 *
 * These were separate screens before the merge, and collapsing them into one
 * scrolling accordion made "advanced settings" mean "everything we could not
 * place". Each subject keeps a bounded card of its own instead — where it runs,
 * what it can reach — so the reader can see where one ends. The difference from
 * the main flow is that this is opt-in, and leaving it never blocks anything.
 *
 * Two groups now, not four, and the two that left were not demoted — they were
 * promoted to steps. What thinks became step one, because a bad credential is
 * silent everywhere else. Who may sign in became step three, because it is a
 * *question*, not a knob with a working default, and this flow gives a screen of
 * its own to every question. What remains here genuinely has a default that
 * works, which is what earns something a place behind "press on if none of it
 * matters to you".
 */
const ALL_ADVANCED_GROUPS: readonly {
  id: string;
  label: string;
  title: string;
  hint: string;
  fields: readonly string[];
}[] = [
  {
    id: "host",
    label: "Host",
    title: "How this host runs",
    hint: "The address it serves on and how much room its workspace gets. Defaults are fine for a laptop.",
    fields: ["bind", "public_url", "workspace.max_blob_mb", "workspace.storage_quota_gb"],
  },
];

/** Advanced groups on offer — the Host group is hidden while hosts are. */
const ADVANCED_GROUPS = ALL_ADVANCED_GROUPS.filter(
  (group) => group.id !== "host" || !HOST_SETTINGS_HIDDEN,
);

/** How each sign-in mode is described, in consequences rather than mode names. */
const AUTH_MODE_COPY: Record<string, { label: string; hint: string }> = {
  email: {
    label: "Email",
    hint: "People sign in with a magic link sent to an invited address.",
  },
  wallet: {
    label: "Wallet",
    hint: "People sign in by signing a challenge with an invited wallet.",
  },
  none: {
    label: "No sign-in",
    hint: "Anyone who can reach this host is the owner. Only offered because this host is loopback-only.",
  },
};


interface Props {
  client: OpenCompanyClient;
  /**
   * Called once setup has been applied, so the caller can re-enter the console.
   *
   * Handed the sign-in the wizard arranged, when it arranged one: a
   * cross-origin console (the desktop) receives the session in the body
   * rather than as a cookie, and the caller is where a credential is stored.
   */
  onDone: (signIn?: SignIn) => void;
  /**
   * Whether the operator can leave without finishing. False on a genuine first
   * run, where there is no console to go back to.
   */
  onCancel?: () => void;
  /**
   * Whether `onDone` hands off to a **fresh** shell mount. The connection
   * console's re-probe does (it boots a new `AppShell`), so its completion
   * button writes the one-shot hand-off marker for that shell to consume. The
   * in-shell dialog does not — it closes in place and the running shell
   * suppresses the welcome through `onCompleted` — and a marker with no
   * consuming mount would be read as a fresh hand-off on the next reload.
   */
  expectsShellRemount?: boolean;
}

/**
 * Whether a finished wizard should hand the host a **template slug** rather than
 * a designed company.
 *
 * Pure, and exported, because this one boolean decides what an operator
 * actually gets: a template seeded whole — its roster, its `[tools]` belt, its
 * prompts, its provenance — or a company rebuilt from the review screen, which
 * can carry none of those and is capped at six teammates.
 *
 * The rules, and why each is here:
 *
 * - **A host with a company seeds nothing.** Setup must never hand an operator
 *   a second starter company on a re-run.
 * - **Only a `preset` roster.** A designed team was built for *them*; shipping
 *   the template instead would throw the design pass away. A `fallback` roster
 *   is the curated team matched from their answers, which is not any template's.
 * - **Only an untouched one.** Edits exist nowhere but the review screen, so an
 *   edited roster has to travel as a designed company.
 * - **Only when no credential is carried.** The designed path writes the tested
 *   provider onto the manifest and stores the key against the company; a
 *   template seed has nowhere to put either, and silently dropping a key the
 *   operator just watched pass is the worse trade.
 * - **Except when nothing will be carried.** Where the submit omits inference
 *   anyway — the host already reaches it, or the credential that passed was the
 *   house's rather than this operator's — the designed path trades the template
 *   away for a credential that was never going to be written: a pure loss, and
 *   an invisible one, since the review screen shows the template's roster either
 *   way.
 *
 *   Asked as `writesInference` rather than `provider === "managed"`. That
 *   literal was the same assumption spelled as a string, and it silently stopped
 *   firing the moment the model step stopped adopting a provider it does not
 *   offer — the caller passes the very condition its submit uses, so the two
 *   cannot answer differently.
 */
export function shouldSeedTemplate(input: {
  hasCompany: boolean;
  source: "model" | "fallback" | "preset" | null;
  rosterEdited: boolean;
  template: string;
  credentialTested: boolean;
  /** Whether the submit will actually write inference for this company. */
  writesInference: boolean;
}): boolean {
  if (input.hasCompany) return false;
  if (input.source !== "preset") return false;
  if (input.rosterEdited) return false;
  if (!input.template.trim()) return false;
  return !input.credentialTested || !input.writesInference;
}

/**
 * The name to offer for a company nobody has named yet.
 *
 * Mirrors what the host derives when no name is sent (`company_name` in
 * `src/company/setup.rs`) so the suggestion is the name the operator would
 * otherwise have been given silently — a template's own name when one was
 * picked, else the first clause of the industry answer.
 *
 * Deliberately a *suggestion in a visible field* rather than a better silent
 * derivation: the id is minted from this and then permanent, and the one screen
 * where that is still changeable is the one this fills.
 */
export function suggestedCompanyName(industry: string, templateName: string | null): string {
  if (templateName?.trim()) return templateName.trim();
  const raw = industry.trim();
  if (!raw) return "";
  // The same clause rule the host applies: a spaced hyphen is a break, a bare
  // one is part of a word — so "E-commerce — homeware" gives "E-commerce", not
  // "E".
  const normalised = raw.replace(/ [-–] /g, "—");
  const head = normalised
    .split(/[—,.:;\n]/)
    .map((part) => part.trim())
    .find((part) => part.length > 0);
  return (head ?? raw).slice(0, 60).trim();
}

export function SetupWizard({ client, onDone, onCancel, expectsShellRemount }: Props) {
  // Optional on purpose: this wizard also renders with no console assembled
  // around it. Undefined in the browser and on a remote host besides — see
  // `onNameLocalHost`.
  const onNameLocalHost = useOptionalHosts()?.onNameLocalHost;
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  /**
   * Where the operator is, held as a step **id** rather than an index.
   *
   * The list of steps can lose one behind their back — choosing "no sign-in"
   * removes the address screen, and a host that reaches its own model removes
   * the model screen — and an index silently means a *different screen* the
   * moment it does. An id survives both, which is the point: the next person to
   * reorder these will not think to restate the argument.
   */
  const [stepId, setStepId] = useState<string>(STEPS[0].id);
  /**
   * The way picked on step 0, or `null` while it is unanswered.
   *
   * Only ever the operator's own answer. Where the question is not asked at
   * all the way is resolved from the host instead — see `setupWay`.
   */
  const [chosenWay, setChosenWay] = useState<SetupWay | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [applied, setApplied] = useState<SetupApplied | null>(null);

  /** The three answers. */
  const [draft, setDraft] = useState<SetupDraft>(emptySetupDraft);
  /** The shipped company template the operator explicitly chose. */
  const [template, setTemplate] = useState("");
  /**
   * What to call the company, once the operator has typed one.
   *
   * Asked with the other questions about the business, because the host mints
   * the company id from it and there is no rename anywhere in the product — so
   * it is the most permanent answer in the flow, and used to be collected last.
   */
  const [companyName, setCompanyName] = useState("");
  /**
   * Whether the name on screen is the operator's or ours.
   *
   * A suggestion has to be *replaceable*, and "is it blank?" cannot tell the
   * two apart: picking one template, going back, and picking another left the
   * first template's name in the field — no longer blank, never typed, and
   * about to become the second company's permanent id.
   */
  const [nameTouched, setNameTouched] = useState(false);
  /**
   * Whether the operator has changed the proposed roster.
   *
   * Load-bearing, not bookkeeping: an untouched `preset` roster is sent back as
   * a template slug so the host seeds that template whole — its tool belt and
   * prompts included — while an edited one has to go as a designed company,
   * because the edits exist nowhere else.
   */
  const [rosterEdited, setRosterEdited] = useState(false);
  /** The address that will be able to sign in. */
  const [email, setEmail] = useState("");
  /**
   * The first admin's password, generated up front and editable on the "You"
   * step. It is what signs the operator in the moment setup applies — and
   * the same one they use tomorrow — so the finish line no longer depends on
   * a mailbox this laptop probably does not have.
   */
  const [adminPassword, setAdminPassword] = useState(() => generatePassword());
  /** Whether the operator has been shown a problem on the current step yet. */
  const [touched, setTouched] = useState(false);
  /**
   * The route a tested credential belongs to.
   *
   * Starts from what the host already holds, so a hosted operator — who has no
   * key and cannot get one — arrives at a step that is already answered and
   * only needs testing. Seeded from the host once its status arrives — see the
   * fetch effect; not an initialiser, because `status` is null until then.
   */
  const [provider, setProvider] = useState<string>(SETUP_INFERENCE_OPTIONS[0].id);
  /**
   * The verdict on the credential, and the reason the step can gate on it.
   *
   * `"untested"` blocks Next; `"ok"` releases it; `"failed"` blocks with the
   * reason shown. `"hosted"` releases it as well, and says the model came with
   * the host rather than from this operator. Only the managed branch gates on
   * this at all — self-managed collects a provider draft instead, and both of
   * its connections are optional. There is no state in which the operator
   * cannot proceed at all: decision D3 says nobody gets stuck, and a credential
   * they cannot obtain must not be the one thing that traps them.
   */
  const [tested, setTested] = useState<TestState>({ kind: "untested" });
  /**
   * The provider the self-managed branch connected, staged for the apply.
   *
   * Held apart from {@link tested} rather than folded into it. `tested` is one
   * verdict slot read by the managed branch's gate *and* by `design`'s
   * `modelless`, so a second meaning in it would make one branch's answer
   * change the other's behaviour.
   */
  const [providerDraft, setProviderDraft] = useState<AddProviderInput | null>(null);
  /**
   * The Composio credential the self-managed branch collected, staged for the
   * apply. Independent of {@link providerDraft}: either connection can be made
   * without the other, and skipping one says nothing about the other.
   */
  const [composioDraft, setComposioDraft] = useState<ComposioDraft | null>(null);
  /**
   * The team, once the host has designed one — and `null` until then.
   *
   * Held as state rather than refetched per render because the operator edits
   * it: what they approve on Review is exactly what gets built, and a second
   * pass could return a different team.
   */
  const [roster, setRoster] = useState<SetupRoster | null>(null);
  const [designing, setDesigning] = useState(false);
  const [designError, setDesignError] = useState<string | null>(null);
  /**
   * Bumped whenever the setup way changes. A test verdict or a designed
   * roster that resolves after the branch it was asked under has been left
   * carries this number from the render that started it, so a stale one can
   * be told apart from a current one even after the step that requested it
   * has unmounted.
   */
  const setupWayGenerationRef = useRef(0);
  /** How many teammates have landed, once the apply is building them. */
  const [built, setBuilt] = useState<number | null>(null);
  /**
   * The sign-in the wizard arranges on the operator's behalf, once the company
   * exists.
   *
   * Without this the flow forgot them at the finish line: they typed an address
   * on step four, and the console then handed them an **empty** email box with
   * no explanation — on a laptop with no mail configured, waiting for a link
   * that was never going to arrive. The wizard knows who they are and the
   * password they just set, so it signs them in itself.
   *
   * - `signed-in` — done; the button opens the console as them.
   * - `password` — the apply landed but the sign-in did not (a store hiccup, a
   *   host that answered the apply and then not the login). The account
   *   exists with the password they saw; say so, and the sign-in screen takes
   *   it.
   * - `open` — nothing to arrange: a host with no sign-in, or one that already
   *   had a company and asked for no address.
   */
  /** Guards the hand-off against re-running; see the effect below. */
  const arranged = useRef(false);
  const [handoff, setHandoff] = useState<
    | { kind: "arranging" }
    | { kind: "signed-in"; signIn: SignIn }
    | { kind: "password" }
    | { kind: "open" }
    | null
  >(null);

  useEffect(() => {
    let cancelled = false;
    getSetup(client)
      .then((s) => {
        if (cancelled) return;
        setStatus(s);
        // Seed the form from what the file already holds, so an operator
        // re-running setup edits their configuration rather than a blank one.
        const seeded: Record<string, string> = {};
        for (const f of s.fields) if (f.value !== null) seeded[f.key] = f.value;
        // Answer the sign-in question for a desktop install, because where this
        // console is running has already answered it. The packaged app is a
        // `none`-mode host — one machine, one person, no mailbox to send a link
        // to — so asking would be asking an operator to re-derive a fact about
        // their own computer, and then asking them for an address to go with the
        // wrong answer. Seeding it removes the address step before they see it
        // rather than taking it away after (see `visibleSteps`).
        //
        // A preselection, not a lock: the mode is still on screen and still
        // changeable, which is what someone sharing their instance with a
        // colleague needs. The host reads the choice back out of `config.toml`
        // at the next launch, so it survives the quit.
        //
        // Both conditions are load-bearing. `auth_modes` is checked because this
        // console can be pointed at a *remote* host through the switcher, and a
        // routable host withholds `none` on purpose — it would be an
        // unauthenticated admin console — so seeding it there would walk the
        // operator into a choice the apply refuses. And only when the file names
        // nothing: an operator re-running setup is editing their own
        // configuration, not being told what it should have been.
        //
        // The host says so itself when it can (`default_auth_mode`, reported
        // by a desktop host whatever is drawing this console — a browser tab
        // pointed at it included); the webview sniff stays for a host too old
        // to report it.
        if (
          seeded.auth_mode === undefined &&
          s.auth_modes.includes("none") &&
          (s.default_auth_mode === "none" || isDesktopRuntime())
        ) {
          seeded.auth_mode = "none";
        }
        setValues(seeded);
        // Pre-fill the model step from what the host already holds. A hosted
        // operator has a credential injected by the control plane, no key of
        // their own, and no way to get one — the step should arrive answered.
        // Only adopt a provider the step actually offers. The host reports the
        // nothing-configured case as `managed`, which is not a tile here any
        // more — adopting it would select nothing and leave the step looking
        // answered when it is not.
        if (
          s.inference.provider &&
          SETUP_INFERENCE_OPTIONS.some((option) => option.id === s.inference.provider)
        ) {
          setProvider(s.inference.provider);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) setLoadError(err instanceof Error ? err.message : String(err));
      });
    return () => {
      cancelled = true;
    };
  }, [client]);

  /**
   * Prove the house credential before the model step is taken away.
   *
   * `inference.ready` is built from the environment: a credential and a URL
   * resolve, which is not the same fact as the endpoint answering. Hiding the
   * step on that alone removes the only live connection check in first run, so
   * an expired key or a URL that no longer resolves finishes setup looking
   * healthy and surfaces several screens later as a curated team nobody chose.
   *
   * The same probe the Test button runs, sent once with no key so the host
   * resolves its own injected credential (`resolve_endpoint`). A pass settles
   * `hosted` and the step goes; a failure is a verdict on the model step, which
   * stays and shows it.
   */
  useEffect(() => {
    if (!status?.inference.ready) return;
    let cancelled = false;
    const generation = setupWayGenerationRef.current;
    setTested({ kind: "testing" });
    testInference(client, {
      provider: status.inference.provider ?? SETUP_INFERENCE_OPTIONS[0].id,
      key: null,
      baseUrl: null,
    })
      .then((result) => {
        if (cancelled || generation !== setupWayGenerationRef.current) return;
        setTested(
          result.ok
            ? { kind: "hosted" }
            : { kind: "failed", error: result.error ?? "Could not reach the provider." },
        );
      })
      .catch((err: unknown) => {
        if (cancelled || generation !== setupWayGenerationRef.current) return;
        setTested({ kind: "failed", error: err instanceof Error ? err.message : String(err) });
      });
    return () => {
      cancelled = true;
    };
  }, [client, status]);

  const set = useCallback((key: string, value: string) => {
    setValues((prev) => ({ ...prev, [key]: value }));
  }, []);

  /**
   * Arrange the operator's way in, the moment the company exists.
   *
   * The apply created the admin account with the password from the "You"
   * step, so this is an ordinary password sign-in against the company that
   * was just seeded — no mailbox, no echoed code, no link to hand over. It
   * used to be a magic-link request whose outcome depended on the host's mail
   * (mailed, echoed back, or undeliverable), each with its own explanation;
   * a laptop with no transport ended setup by pointing at an inbox that
   * would stay empty forever.
   *
   * Failure is not fatal: the account exists, and the sign-in screen takes
   * the same password.
   */
  useEffect(() => {
    // Guarded by a ref, not by `handoff`.
    //
    // Depending on the state this effect *sets* made it cancel itself: setting
    // `arranging` re-ran the effect, whose cleanup flipped `cancelled` on the
    // request still in flight, so the answer was always discarded and the screen
    // sat on "Getting you signed in…" forever. A ref settles before the next
    // render and is not a dependency.
    if (!applied || arranged.current) return;
    arranged.current = true;
    const company = applied.seeded_company;
    const address = email.trim();

    // No status means the read failed and we cannot tell what mode this host is
    // in; opening the console is the answer that cannot be wrong.
    if (!company || !address || !status || !requiresSignIn(status, values)) {
      setHandoff({ kind: "open" });
      return;
    }

    setHandoff({ kind: "arranging" });
    loginWithPassword(client, company, address, adminPassword)
      .then((signIn) => setHandoff({ kind: "signed-in", signIn }))
      .catch(() => setHandoff({ kind: "password" }));
  }, [applied, email, adminPassword, client, status, values]);

  // See `changedFields`: unchanged fields are omitted, env-owned ones are never
  // sent (the host refuses them and an apply is all-or-nothing), and a secret
  // goes only when the operator typed one.
  const changed = useMemo(() => {
    const fields = status ? changedFields(status, values) : {};
    // A key typed here is never a host-wide config write. A BYOK/local
    // credential belongs to the new company's write-only inference store, and
    // a TinyHumans one is that company's own account key — fanned out to
    // Composio and the LLM row the moment it lands. `tinyhumans_api_key` is
    // the instance's identity for companies that have none of their own, it
    // is read once at boot, and writing the same bytes there would duplicate
    // the secret and report a restart nobody needs.
    delete fields.tinyhumans_api_key;
    return fields;
  }, [status, values]);

  /**
   * Whether the managed way can actually be completed on this host.
   *
   * Two host-reported facts, both of which the managed branch dead-ends
   * without: the TinyHumans route has to be on offer at all, and the host has
   * to accept a key for it from this operator rather than owning that field
   * from the environment. Null status counts as offered — the read has not
   * landed, and a branch that appears once it does is better than one that
   * vanishes under someone already reading it.
   */
  const managedWayOffered = useMemo(() => {
    if (!status) return true;
    if (!SETUP_INFERENCE_OPTIONS.some((option) => option.id === "managed")) return false;
    const field = status.fields.find((f) => f.key === "tinyhumans_api_key");
    return field === undefined || field.editable;
  }, [status]);

  /**
   * Whether step 0 is a question worth asking.
   *
   * Not on a host with only one answer available, and not on a re-run: an
   * instance that has already been configured is being edited, and the way it
   * was set up is not back on the table.
   */
  const asksSetupWay = managedWayOffered && !status?.complete;

  /**
   * The way in force. Unasked resolves to self-managed, which is the branch
   * that needs nothing brokered and so can never dead-end.
   */
  const setupWay: SetupWay | null = asksSetupWay ? chosenWay : "self-managed";

  /**
   * Answer step 0, clearing what the other branch had already collected.
   *
   * A key typed against one way would otherwise be presented to the other, and
   * a roster designed under the old answer would ride through Review.
   */
  const chooseSetupWay = (way: SetupWay) => {
    if (chosenWay !== null && chosenWay !== way) {
      setupWayGenerationRef.current += 1;
      set("tinyhumans_api_key", "");
      setTested({ kind: "untested" });
      // Cleared with the rest of the other branch's answers. A provider
      // connected under Self-managed is that branch's answer to "what does this
      // company run on", and carrying it into Managed would submit a BYOK row
      // for an operator who has just said they want none.
      setProviderDraft(null);
      setComposioDraft(null);
      setRoster(null);
    }
    // Managed is TinyHumans, so its step 1 has no provider to pick — and the
    // probe it runs has to reach the managed endpoint rather than whichever
    // route the host happened to seed.
    if (way === "managed") setProvider(MANAGED_PROVIDER);
    setChosenWay(way);
  };

  /**
   * The steps this host actually shows. `STEPS` stays the source of order.
   *
   * A host that asks nobody to sign in has nobody to invite, so the address step
   * is absent rather than optional — and an absent step gets no slot in the
   * progress bar either, or the bar counts a screen that will never arrive.
   *
   * A host whose own model answered is the same shape of fact. Its operator has
   * a credential injected by the control plane, no key of their own and no way
   * to get one, so the model step is a question with one possible answer — and
   * asking it demands a key from the one person who cannot supply one.
   *
   * Keyed on the verdict rather than on `inference.ready`, because those are
   * different claims: readiness says a credential resolved from the
   * environment, and only `hosted` says the endpoint replied. A host whose
   * credential is expired or whose URL no longer resolves keeps the step, which
   * is the one screen able to say so.
   *
   * `status` is null until the first read lands, and that counts as "show it":
   * the mode it would be judged against has not been read yet, and a bar that
   * changes length under someone already looking at it is worse than one that
   * starts at its longest. The probe is the same — the step stays while it is
   * in flight.
   */
  const visibleSteps = useMemo(
    () =>
      STEPS.filter(
        (s) =>
          (!BRANCH_STEP_IDS.includes(s.id) || tested.kind !== "hosted") &&
          (s.id !== "setup-way" || asksSetupWay) &&
          (!isStepOne(s.id) || (setupWay !== null && STEP_ONE_FOR[setupWay] === s.id)) &&
          (s.id !== "account" || !status || requiresSignIn(status, values)) &&
          (s.id !== "advanced" || ADVANCED_GROUPS.length > 0),
      ),
    [status, values, tested, asksSetupWay, setupWay],
  );

  // A position whose step is no longer shown falls back to the start. That is
  // how a hosted host leaves the model step: `stepId` begins there, and the
  // probe that answers it removes the screen underneath.
  const step = Math.max(0, visibleSteps.findIndex((s) => s.id === stepId));

  const restartKeys = useMemo(() => {
    if (!status) return [];
    return Object.keys(changed).filter(
      (k) => status.fields.find((f) => f.key === k)?.requires_restart,
    );
  }, [status, changed]);

  /**
   * Whether the house is supplying inference rather than this operator.
   *
   * The credential the model step tests is the HOST's whenever the operator
   * leaves the key box empty, so a pass there says the house works — not that
   * the provider selected beside it does.
   *
   * This used to be spelled `provider !== "managed"` at the two call sites
   * below, and it held only because the step adopted the host's provider
   * verbatim: a hosted tenant selected `managed`, that check withheld the
   * config, and the platform default went on applying. The step no longer
   * adopts a route it does not offer, so `provider` is now a real one and the
   * literal check stopped firing — testing the house credential with a blank
   * key would write `openrouter` at the managed endpoint with no key over a
   * configuration that was working.
   *
   * A key the operator actually typed is theirs, so it stays configuration.
   */
  const houseSuppliesInference =
    !!status?.inference.ready &&
    !SETUP_INFERENCE_OPTIONS.some((option) => option.id === status.inference.provider);
  const operatorConfiguredInference =
    !houseSuppliesInference || !!values.tinyhumans_api_key?.trim();
  /**
   * Whether this company will finish setup with nothing to think with.
   *
   * What it decides is the **design brief**: `automate` and `teamHint` are read
   * by a model and by nothing else, so with none the Business step does not ask
   * for them and the design pass is told to return its curated team outright
   * rather than reaching for a credential that is not there.
   *
   * Asked of all three ways a model can arrive — a proved TinyHumans key, a
   * host that answers for itself, a staged provider — rather than of a single
   * verdict. It used to be `tested.kind === "skipped"`, one option in a picker
   * that no longer exists; keyed on that alone it would have silently stopped
   * firing, and an operator who connected nothing would be asked to describe
   * what to automate by a wizard that had already decided not to read it.
   */
  const modelless =
    tested.kind !== "ok" && tested.kind !== "hosted" && providerDraft === null;
  /**
   * Whether finishing will write inference onto this company.
   *
   * Read by both the submit payload and {@link shouldSeedTemplate}: the seed
   * decision exists to avoid trading a template away for a credential that is
   * never written, so it has to be asking the same question the payload answers.
   */
  const writesInference =
    tested.kind === "ok" && provider !== MANAGED_PROVIDER && operatorConfiguredInference;
  /**
   * Whether finishing will store this key as the new company's own TinyHumans
   * credential, for the host to fan out.
   *
   * The other half of {@link writesInference}: every tested key goes to
   * exactly one of the two, because a TinyHumans key is an account identity
   * that fills the model, Composio and search slots at once, where every other
   * route's key is a single provider's and belongs on the manifest. Asked as
   * "which route passed the probe" rather than "which branch was chosen", so
   * an operator who took the self-managed way and left the picker on
   * TinyHumans still gets the account-key treatment their key actually needs.
   */
  const writesAccountKey =
    tested.kind === "ok" &&
    provider === MANAGED_PROVIDER &&
    !!values.tinyhumans_api_key?.trim();

  const chosenTemplateName =
    status?.templates.find((candidate) => candidate.id === template)?.name ?? null;
  /**
   * The name this company is heading for.
   *
   * Derived rather than stored while it is still ours: a suggestion has to
   * track the answers it was drawn from, and a cached one went stale the moment
   * the operator went back and picked a different template. Once they type, the
   * answer is theirs and the derivation stops.
   */
  const name = nameTouched
    ? companyName
    : suggestedCompanyName(draft.industry, chosenTemplateName);

  /**
   * Ask the host to design a team, on the way into Review.
   *
   * Never throws upward: the host answers with its curated team rather than an
   * error when it cannot reach a model, so a rejection here is a genuine
   * transport failure — and even then the operator gets a roster to review,
   * because being stranded five screens in is the one outcome worse than an
   * imperfect team.
   */
  const design = useCallback(async () => {
    const generation = setupWayGenerationRef.current;
    setDesigning(true);
    setDesignError(null);
    try {
      const proposed = await proposeSetupRoster(client, {
        industry: draft.industry,
        teamHint: modelless ? "" : draft.teamHint,
        automate: modelless ? "" : draft.automate,
        template: template || null,
        // The design brief's credential, from whichever branch collected one.
        //
        // The self-managed branch no longer settles `tested` at all — its
        // provider is staged as a draft — so keying these on the verdict alone
        // would have quietly stopped sending them: a designed roster for an
        // operator with a working key, replaced by the curated team, with
        // nothing on screen saying why.
        inferenceKey: providerDraft?.key || values.tinyhumans_api_key || null,
        inferenceProvider:
          providerDraft?.kind ??
          (tested.kind === "ok" && operatorConfiguredInference ? provider : null),
        // The endpoint the draft's own probe reached, not the field it was
        // typed into: a cloud provider never types one, and the catalogue is
        // where its address comes from.
        inferenceBaseUrl: providerDraft
          ? probeEndpoint(providerDraft.kind, providerDraft.baseUrl)
          : tested.kind === "ok" && operatorConfiguredInference
            ? tested.baseUrl
            : null,
        inferenceModel: providerDraft?.model ?? (tested.kind === "ok" ? tested.model : null),
        forceCurated: modelless,
      });
      // The host is contracted never to answer with an empty roster, so a
      // missing or empty one is a failure rather than a team of nobody — and
      // trusting the shape here crashed Review on `.map` of undefined.
      if (!Array.isArray(proposed?.agents) || proposed.agents.length === 0) {
        throw new Error("The host answered without a team to review.");
      }
      // A design asked under a setup way the operator has since left must not
      // land on the one they switched to — see `setupWayGenerationRef`.
      if (generation !== setupWayGenerationRef.current) return;
      setRoster(proposed);
      setRosterEdited(false);
    } catch (err: unknown) {
      if (generation === setupWayGenerationRef.current) {
        setDesignError(err instanceof Error ? err.message : String(err));
        setRoster(null);
      }
    } finally {
      setDesigning(false);
    }
  }, [
    client,
    draft,
    template,
    modelless,
    provider,
    providerDraft,
    tested,
    values.tinyhumans_api_key,
  ]);

  const submit = useCallback(async () => {
    if (!status) return;
    // A host with no company and no designed roster would finish setup into
    // exactly the dead end this flow exists to remove: a configured instance
    // with nothing to sign in to and no way back into setup.
    if (status.companies.length === 0 && !roster?.agents.length) return;
    setSaving(true);
    setSaveError(null);
    setBuilt(roster?.agents.length ?? null);
    try {
      const seedTemplate = shouldSeedTemplate({
        hasCompany: status.companies.length > 0,
        source: roster?.source ?? null,
        rosterEdited,
        template,
        // A staged provider is a proved credential that this submit will write,
        // which is exactly what both of these ask. Answered at the call site
        // rather than inside the function: the function's job is the rule, and
        // "which of the two branches carried a credential" is this component's.
        credentialTested: tested.kind === "ok" || providerDraft !== null,
        writesInference: writesInference || providerDraft !== null,
      });

      const result = await submitSetup(client, {
        fields: changed,
        name: name.trim() || null,
        // Sent for either path. The designed company carries its own copy
        // below; a seeded template has no other way to learn it, and no shipped
        // template names an admin — so without this, choosing a template *and*
        // a sign-in finishes setup into a company the operator cannot
        // administer.
        admin_email: email.trim() || null,
        // The password that makes that address an *account* rather than a
        // standing invite, so the hand-off below can sign them straight in.
        // Only where somebody will sign in: a `none`-mode host has nobody to
        // distinguish and the host ignores it there anyway.
        admin_password: requiresSignIn(status, values) ? adminPassword : null,
        // Deferred to here rather than sent from the step that collected it:
        // the fan-out writes a company's slots, and the company is what this
        // request creates. Carried past the template/designed fork because the
        // key belongs to whichever company comes out of it.
        tinyhumans_key: writesAccountKey ? (values.tinyhumans_api_key?.trim() ?? null) : null,
        tinyhumans_model:
          writesAccountKey && tested.kind === "ok" ? (tested.model ?? null) : null,
        // Deferred to here for the same reason: the add writes a company's
        // rows, and the company is what this request creates. Carried past the
        // template/designed fork because the provider belongs to whichever
        // company comes out of it.
        provider_draft: providerDraft,
        composio_draft: composioDraft,
        template: seedTemplate ? template : null,
        company:
          status.companies.length === 0 && roster && !seedTemplate
            ? {
                industry: draft.industry,
                teamHint: draft.teamHint,
                automate: draft.automate,
                // As reviewed, not as proposed.
                agents: roster.agents,
                adminEmail: email.trim() || null,
                inference: writesInference
                  ? {
                      provider,
                      baseUrl: tested.baseUrl,
                      model: tested.model ?? null,
                      key: values.tinyhumans_api_key?.trim() || null,
                    }
                  : null,
              }
            : null,
      });
      setApplied(result);
      // The host takes the company's name. Best-effort and after the fact: the
      // company exists either way, and a rename that fails is a label, not a
      // company. Never a reason to show an error on a screen that just
      // succeeded.
      if (result.seeded_company && name.trim() && onNameLocalHost) {
        try {
          await onNameLocalHost(name.trim());
        } catch (error: unknown) {
          console.warn("[setup] could not name the host after the company", error);
        }
      }
    } catch (err: unknown) {
      setSaveError(err instanceof Error ? err.message : String(err));
      setBuilt(null);
    } finally {
      setSaving(false);
    }
  }, [
    client,
    status,
    changed,
    roster,
    rosterEdited,
    template,
    name,
    draft,
    email,
    provider,
    providerDraft,
    composioDraft,
    tested,
    values.tinyhumans_api_key,
    onNameLocalHost,
  ]);

  if (loadError) {
    return (
      <OnboardingShell>
        {/*
          The first-run flow is outside the console shell, but "outside the
          shell" is not "unnamed" (codex review, #1785): these two states run
          before the wizard's own `h1` exists, so an operator on a host that
          cannot read its own setup got a screen a reader could not announce.
          `hidden` — the shell already frames the one thing on screen.
        */}
        <PageHeader title="Set up this instance" hidden />
        <Alert variant="destructive">
          <AlertTitle>Can&apos;t read this instance&apos;s setup</AlertTitle>
          <AlertDescription>{loadError}</AlertDescription>
        </Alert>
      </OnboardingShell>
    );
  }

  if (!status) {
    return (
      <OnboardingShell>
        {/* See `loadError` above. */}
        <PageHeader title="Set up this instance" hidden />
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" /> Reading this instance…
        </div>
      </OnboardingShell>
    );
  }

  if (applied) {
    return (
      <OnboardingShell>
        <div className="space-y-4" data-testid="setup-done">
          <h1 className="text-xl font-semibold">You&apos;re set up</h1>
          <p className="text-sm text-muted-foreground">
            Written to <code className="font-mono text-xs">{applied.config_path}</code>.
          </p>
          {applied.seeded_company && (
            <p className="text-sm text-muted-foreground">
              {/* No template was chosen — the team was designed from the
                  answers, and saying otherwise would credit a menu the
                  operator never saw. */}
              Built <strong>{applied.seeded_company}</strong> with{" "}
              {roster?.agents.length ?? 0}{" "}
              {roster?.agents.length === 1 ? "agent" : "agents"}.
            </p>
          )}
          {/* The host's own words about what the key actually reached, verbatim.
              The fan-out reports the slots it left alone — a model it was never
              given, a surface that already had a key of its own — and those are
              exactly the parts a cheerful "all set" would bury. */}
          {applied.credential_note && (
            <p className="text-sm text-muted-foreground" data-testid="setup-credential-note">
              {applied.credential_note}
            </p>
          )}
          {/* And the same for the provider the self-managed branch connected.
              It carries the refusal too — the company is already built by the
              time the add runs, so an endpoint that stopped answering between
              the probe and the finish is said here rather than turned into a
              failed setup. */}
          {applied.provider_note && (
            <p className="text-sm text-muted-foreground" data-testid="setup-provider-note">
              {applied.provider_note}
            </p>
          )}
          {applied.composio_note && (
            <p className="text-sm text-muted-foreground" data-testid="setup-composio-note">
              {applied.composio_note}
            </p>
          )}
          {/* The button below cannot restart the host — it only re-enters the
              console — so this must not read as something already handled.
              Naming the setting and the action keeps the two apart. */}
          {applied.restart_required.length > 0 && (
            <Alert>
              <RotateCw />
              <AlertTitle>
                You need to restart the host for {applied.restart_required.length} setting(s)
              </AlertTitle>
              <AlertDescription>
                <span className="block">
                  These are read once, when the host starts, so they are saved but{" "}
                  <strong>not yet in force</strong>:{" "}
                  <span className="font-mono text-xs">
                    {applied.restart_required.join(", ")}
                  </span>
                </span>
                <span className="mt-2 block">
                  Stop the <code className="font-mono text-xs">opencompany serve</code> process
                  and start it again. Opening the console now works, but with the previous
                  values for those settings.
                </span>
              </AlertDescription>
            </Alert>
          )}
          {/* What happens next, said before they have to guess it. */}
          {handoff?.kind === "arranging" && (
            <p className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" /> Getting you signed in…
            </p>
          )}

          {handoff?.kind === "signed-in" && (
            <Alert data-testid="setup-handoff-signed-in">
              <AlertTitle>You&apos;re signed in</AlertTitle>
              <AlertDescription>
                As <strong className="text-foreground">{email.trim()}</strong>, with the
                password from the previous step. Use it to sign in next time.
              </AlertDescription>
            </Alert>
          )}

          {handoff?.kind === "password" && (
            <Alert data-testid="setup-handoff-password">
              <AlertTriangle />
              <AlertTitle>Your account is ready, but we couldn&apos;t sign you in</AlertTitle>
              <AlertDescription>
                Sign in as <strong className="text-foreground">{email.trim()}</strong> with
                the password you set on the previous step.
              </AlertDescription>
            </Alert>
          )}

          {(
            <Button
              onClick={() => {
                // The link branch above carries the landing fragment inside its
                // URL. This branch hands off through `onDone`, and
                // `expectsShellRemount` says that hands off to a fresh
                // `AppShell` (the connection console's re-probe), so write the
                // same fragment first: the fresh shell reads it, routes to the
                // roster setup just built, suppresses the tour welcome, and
                // clears the one-shot marker. Without it a no-sign-in host —
                // and the "anyway" escapes for a mailed sign-in — lands on
                // Overview with the tour free to open over that roster.
                //
                // The in-shell dialog must NOT write it: `onDone` there closes
                // the dialog in place and the running shell already suppresses
                // the welcome via `onCompleted`, so the marker would have no
                // consuming mount and would be read as a fresh hand-off on the
                // next reload.
                if (expectsShellRemount && window.location.hash !== SETUP_HANDOFF_FRAGMENT) {
                  window.location.hash = SETUP_HANDOFF_FRAGMENT;
                }
                onDone(handoff?.kind === "signed-in" ? handoff.signIn : undefined);
              }}
              disabled={handoff?.kind === "arranging"}
              data-testid="setup-open-console"
            >
              {/* "Anyway" wherever something is genuinely outstanding — a
                  staged setting, or a sign-in we could not arrange. That word is
                  the only thing saying this button does not finish the job. */}
              {applied.restart_required.length > 0 || handoff?.kind === "password"
                ? "Open the console anyway"
                : "Open the console"}
            </Button>
          )}
        </div>
      </OnboardingShell>
    );
  }

  const current = visibleSteps[step];
  const last = step === visibleSteps.length - 1;
  const needsCompany = status.companies.length === 0;
  // The one thing that must never be reachable: a configured instance with
  // nothing to sign in to and no way back into setup.
  const noRoster = needsCompany && !roster?.agents.length;

  /** Whether this step can be left, and why not when it cannot. */
  const problem = (): string | undefined => {
    if (current.id === "setup-way" && !chosenWay) {
      return "Pick how you'd like to set this up.";
    }
    // The gate, and only on the managed branch. Untested is not "probably
    // fine": the whole reason this step moved to the front is that a bad
    // credential is silent everywhere else.
    //
    // Self-managed has no gate at all, because both of its connections are
    // optional — it asks for a provider and, once slice 4b-ii lands, a Composio
    // credential, either of which an operator may reasonably not have yet. A
    // provider they *did* connect was proved by the dialog's own probe before
    // it was staged, so there is no unproved credential here to hold anyone on.
    if (
      current.id === STEP_ONE_FOR.managed &&
      tested.kind !== "ok" &&
      tested.kind !== "hosted"
    ) {
      return tested.kind === "failed"
        ? "That connection did not work. Fix it, or continue without a model."
        : "Test the connection first, or continue without a model.";
    }
    if (current.id === "business" && needsCompany && status.templates.length > 0 && !template) {
      return "Choose the kind of company you want to start with.";
    }
    if (
      current.id === "business" &&
      needsCompany &&
      status.templates.length === 0 &&
      !draft.industry.trim()
    ) {
      return "Tell us a little about the company first.";
    }
    if (current.id === "business" && needsCompany && !name.trim()) {
      return "Give your company a name.";
    }
    if (current.id === "account" && needsCompany) {
      // Checked here rather than left to the manifest validator on the last
      // screen, which reported it as "`[users].admins` has an invalid entry"
      // after the roster had been designed — a configuration error about a
      // mistake made four steps earlier, in the language of a file the operator
      // has never seen.
      const problem = adminEmailProblem(email, requiresSignIn(status, values));
      if (problem) return problem;
      if (requiresSignIn(status, values)) {
        const weak = passwordProblem(adminPassword);
        if (weak) return weak;
      }
    }
    return undefined;
  };

  const advance = () => {
    if (problem()) {
      setTouched(true);
      return;
    }
    setTouched(false);
    // Designing happens on the way into Review, so the wait sits between two
    // screens rather than in front of one.
    if (visibleSteps[step + 1]?.id === "review" && !roster && !designing) void design();
    const next = visibleSteps[step + 1];
    if (next) setStepId(next.id);
  };

  return (
    <OnboardingShell
      header={
        <div className="space-y-4">
          <div className="space-y-0.5">
            <h1 className="text-xl font-semibold tracking-tight">
              {status.complete ? "Reconfigure this instance" : "Let's build your company"}
            </h1>
            <p className="text-xs leading-snug text-muted-foreground">
              {status.complete
                ? "Change what this host is configured with."
                : "A few questions, then we'll put a team together."}
            </p>
          </div>
          {/* A bar, not a numbered stepper.
              `1 Business — 2 You — 3 Model — 4 Advanced — 5 Review` is the
              language of enterprise configuration: it tells you that you are
              inside a multi-page form. This is sixty seconds long. Progress
              should be felt, and the one thing worth naming is where you are. */}
          <div className="space-y-2">
            <div className="flex gap-1" aria-hidden>
              {visibleSteps.map((s, i) => (
                <button
                  key={s.id}
                  type="button"
                  tabIndex={-1}
                  disabled={i >= step}
                  onClick={() => setStepId(s.id)}
                  data-testid={`step-${s.id}`}
                  className={cn(
                    "h-1 flex-1 rounded-full transition-colors",
                    i < step && "bg-primary/70 hover:bg-primary",
                    i === step && "bg-primary",
                    i > step && "bg-border",
                  )}
                />
              ))}
            </div>
            <p className="text-xs text-muted-foreground">
              <span className="text-foreground">{current.label}</span> · step {step + 1} of{" "}
              {visibleSteps.length}
            </p>
          </div>
        </div>
      }
      footer={
        <div className="flex items-center justify-between gap-3">
          {onCancel ? (
            <Button variant="ghost" size="sm" onClick={onCancel}>
              Cancel
            </Button>
          ) : (
            <span />
          )}
          <div className="flex gap-2">
            {current.id === STEP_ONE_FOR.managed &&
              tested.kind !== "ok" && tested.kind !== "hosted" && (
                <Button
                  variant="ghost"
                  onClick={() => {
                    // Retire this branch's key and any probe still in flight.
                    chooseSetupWay("self-managed");
                    setTouched(false);
                    setStepId("business");
                  }}
                >
                  Continue without a model
                </Button>
              )}
            <Button
              variant="outline"
              disabled={step === 0}
              onClick={() => setStepId(visibleSteps[step - 1].id)}
            >
              Back
            </Button>
            {last ? (
              <Button
                onClick={() => void submit()}
                disabled={saving || noRoster || designing}
                data-testid="setup-finish"
              >
                {saving && <Loader2 className="animate-spin" />}
                Build my company
              </Button>
            ) : (
              <Button onClick={advance} data-testid="setup-next">
                {current.id === "advanced" ? "Looks good" : "Next"}
              </Button>
            )}
          </div>
        </div>
      }
    >
      <div className="space-y-6" data-testid="setup-wizard">
        {current.id === "setup-way" && (
          <SetupWayStep value={chosenWay} onChange={chooseSetupWay} />
        )}

        {current.id === "business" && (
          <BusinessStep
            draft={draft}
            templates={status.templates}
            template={template}
            name={name}
            onTemplate={(id) => {
              setTemplate(id);
              const selected = status.templates.find((candidate) => candidate.id === id);
              if (selected && !draft.industry.trim()) {
                setDraft((current) => ({ ...current, industry: selected.name }));
              }
              setRoster(null);
            }}
            onName={(next) => {
              setCompanyName(next);
              setNameTouched(true);
            }}
            onChange={setDraft}
            onEnter={advance}
            modelless={modelless}
          />
        )}

        {current.id === "signin" && (
          <SignInStep
            status={status}
            value={values.auth_mode ?? ""}
            onChange={(v) => set("auth_mode", v)}
          />
        )}

        {current.id === "account" && (
          <AccountStep
            value={email}
            onChange={setEmail}
            password={adminPassword}
            onPasswordChange={setAdminPassword}
            passwordProblem={touched ? passwordProblem(adminPassword) : undefined}
            onEnter={advance}
            required={needsCompany && requiresSignIn(status, values)}
            mailed={status.mail.wired}
          />
        )}

        {current.id === STEP_ONE_FOR.managed && (
          <ManagedLoginStep
            status={status}
            client={client}
            value={values.tinyhumans_api_key ?? ""}
            onChange={(v) => {
              set("tinyhumans_api_key", v);
              setTested({ kind: "untested" });
            }}
            tested={tested}
            onTested={((generation) => (t: TestState) => {
              // A verdict asked under a setup way the operator has since left
              // must not land on the one they switched to — see
              // `setupWayGenerationRef`. The dialog's own staleness rule cannot
              // answer this one: it compares the answers the step was holding,
              // and switching away unmounts the step with those answers intact.
              if (generation === setupWayGenerationRef.current) setTested(t);
            })(setupWayGenerationRef.current)}
          />
        )}

        {current.id === STEP_ONE_FOR["self-managed"] && (
          <SelfManagedConnectStep
            client={client}
            draft={providerDraft}
            onDraft={((generation) => (next: AddProviderInput | null) => {
              // A draft assembled under a setup way the operator has since left
              // must not land on the one they switched to — see
              // `setupWayGenerationRef`. Same rule the test verdict follows, and
              // the same reason: the probe behind it is a network round trip.
              if (generation !== setupWayGenerationRef.current) return;
              setProviderDraft(next);
              // And the roster with it. `advance` only designs when there is
              // none, so a team designed before this provider answered would
              // otherwise survive into Review as a curated one under copy
              // promising a designed one.
              setRoster(null);
            })(setupWayGenerationRef.current)}
            composio={composioDraft}
            onComposio={((generation) => (next: ComposioDraft | null) => {
              // Same rule as the provider draft — see `setupWayGenerationRef`.
              // The roster is not cleared here: Composio is what the team can
              // reach, not what designs it.
              if (generation === setupWayGenerationRef.current) setComposioDraft(next);
            })(setupWayGenerationRef.current)}
          />
        )}

        {current.id === "advanced" && (
          <AdvancedStep status={status} values={values} set={set} />
        )}

        {current.id === "review" && (
          <ReviewStep
            designing={designing}
            designError={designError}
            roster={roster}
            name={name}
            onRoster={(next) => {
              setRoster(next);
              // Any edit takes the template path off the table — see
              // `seedTemplate` in `submit`.
              setRosterEdited(true);
            }}
            onRetry={() => void design()}
            changed={changed}
            restartKeys={restartKeys}
            status={status}
            hostModel={tested.kind === "hosted"}
            email={email}
            built={built}
          />
        )}

        {problem() && touched && (
          <p className="text-sm text-destructive" data-testid="setup-problem">
            {problem()}
          </p>
        )}

        {saveError && (
          <Alert variant="destructive">
            <AlertTriangle />
            <AlertTitle>That didn&apos;t apply</AlertTitle>
            <AlertDescription>{saveError}</AlertDescription>
          </Alert>
        )}
      </div>
    </OnboardingShell>
  );
}

/**
 * Whether this host will ask anyone to sign in.
 *
 * On `none` there is nobody to invite and an address would be a field with no
 * consequence; on every other mode the address is the only thing standing
 * between the operator and a company they cannot get into.
 */
function requiresSignIn(status: SetupStatus, values: Record<string, string>): boolean {
  const chosen =
    values.auth_mode ?? status.fields.find((f) => f.key === "auth_mode")?.value ?? "email";
  return chosen !== "none";
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------
/**
 * The sign-in modes this wizard may offer, out of the ones the host accepts.
 *
 * `wallet` is withheld, and this is a lockout guard rather than a preference.
 * A wallet company is bootstrapped by `[users].wallets` — "listing an address
 * makes it eligible, signing a challenge mints the admin" — and nothing in this
 * flow can collect one: the account step asks for an email, and both seed paths
 * write `[users].admins`, which `wallet` mode never reads. Choosing it
 * therefore finishes setup on a company with **no eligible administrator**, and
 * the door closes behind it: once a company exists and setup is stamped
 * complete, `server::setup::authorize` stops answering an anonymous caller, so
 * the console cannot be used to undo it.
 *
 * That was previously unreachable on the instance most operators have, because
 * it seeded a company and never opened this wizard at all. Making the wizard
 * the way in is what makes withholding this necessary rather than tidy.
 *
 * A mode already in force is always offered, even when withheld: an operator
 * re-running setup on a wallet host is looking at their own configuration, and
 * a screen that silently omits the answer it is currently showing would read as
 * the setting having been lost.
 *
 * Wallet remains available the way it is actually set up today — `auth_mode` in
 * `config.toml` beside a `[users].wallets` list on the company. Collecting a
 * wallet key here, and writing the mode and its list onto the seeded manifest
 * together, is the fuller fix and is its own change.
 */
export function offeredAuthModes(status: SetupStatus, current: string): string[] {
  // A field `env` owns is one this screen is *reporting*, not offering, and it
  // cannot report a mode it has filtered out. It also cannot tell which mode
  // that is: `FieldDto.value` is read from `config.toml` alone
  // (`effective_value` in `src/server/setup.rs`), so an `OPENCOMPANY_AUTH_MODE`
  // the host is actually running never reaches this list. Withholding on top of
  // that would show a locked picker whose every option is wrong. Nothing here
  // is selectable in that state, so nothing can be walked into.
  const field = status.fields.find((f) => f.key === "auth_mode");
  if (field !== undefined && !field.editable) return status.auth_modes;
  return status.auth_modes.filter((mode) => mode !== "wallet" || current === "wallet");
}

function SignInStep({
  status,
  value,
  onChange,
}: {
  status: SetupStatus;
  value: string;
  onChange: (v: string) => void;
}) {
  const field = status.fields.find((f) => f.key === "auth_mode");
  const locked = field !== undefined && !field.editable;

  return (
    <div>
      {/* Heading, one-sentence hint, then the control — the rhythm every other
          question screen keeps. It had none of its own while it lived inside
          Advanced, where the group header asked the question on its behalf. */}
      <h2 className="text-base font-medium leading-snug" data-testid="setup-question">
        How should people sign in?
      </h2>
      <p className="text-xs leading-snug text-muted-foreground">
        This applies to every company this host serves.
      </p>
      {locked && (
        <div className="mt-2.5">
          <LayerLock />
        </div>
      )}

      <div className="mt-2.5 space-y-2">
        {offeredAuthModes(status, value || field?.value || "").map((mode) => {
          const copy = AUTH_MODE_COPY[mode] ?? { label: mode, hint: "" };
          const active = (value || field?.value) === mode;
          return (
            <button
              key={mode}
              type="button"
              disabled={locked}
              onClick={() => onChange(mode)}
              data-testid={`auth-mode-${mode}`}
              aria-pressed={active}
              className={cn(
                "w-full rounded-lg border p-3 text-left transition-colors",
                !locked && "hover:bg-muted",
                active && "border-primary bg-muted",
                locked && "opacity-60",
              )}
            >
              <div className="text-sm font-medium">{copy.label}</div>
              <div className="mt-0.5 text-xs text-muted-foreground">{copy.hint}</div>
            </button>
          );
        })}
      </div>

      {/* What choosing email actually gets you on *this* host. Not a reason
          to hide the mode or grey the card out: a password signs people in
          with no transport anywhere in sight, so "no mail" means the magic
          link is off the table, not that email sign-in is broken. Hiding it
          would refuse a mode the operator may wire mail up for ten minutes
          from now. */}
      {status.auth_modes.includes("email") && !status.mail.wired && (
        <p className="mt-3 text-xs text-muted-foreground" data-testid="setup-mail-note">
          This host has no mail transport, so nobody gets a sign-in link here — you and
          anyone you invite sign in with a password instead. Configure mail on the host
          to offer links as well.
        </p>
      )}

      {!status.auth_modes.includes("none") && (
        <p className="mt-3 text-xs text-muted-foreground">
          &ldquo;No sign-in&rdquo; isn&apos;t offered because this host binds a routable address,
          where it would serve an unauthenticated admin console to anyone who can reach it.
        </p>
      )}
    </div>
  );
}

function FieldRow({
  field,
  value,
  onChange,
}: {
  field: SetupField;
  value: string;
  onChange: (v: string) => void;
}) {
  const locked = !field.editable;
  const copy = fieldCopy(field.key);
  return (
    <div>
      {/* Words first, key second.
          The label used to *be* the key, which turned this screen into a
          `.toml` file with input boxes. The key is still here — small and
          monospaced — because whoever opened Advanced is often the person who
          will next edit that file by hand. */}
      <Label htmlFor={field.key} className="text-base font-medium leading-snug">
        {copy.label}
      </Label>
      {copy.hint && (
        <p className="text-xs leading-snug text-muted-foreground">{copy.hint}</p>
      )}
      <Input
        id={field.key}
        data-testid={`field-${field.key}`}
        value={locked ? (field.value ?? "") : value}
        disabled={locked}
        type={field.secret ? "password" : "text"}
        placeholder={fieldPlaceholder(field)}
        className="mt-2.5"
        onChange={(e) => onChange(e.target.value)}
      />
      <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1">
        <code className="font-mono text-2xs text-muted-foreground">{field.key}</code>
        {/* Only where it is true *and* actionable: a locked field cannot be
            changed here at all, so telling its owner about a restart is noise
            about work they are not doing. */}
        {field.requires_restart && !locked && (
          <span className="text-2xs text-muted-foreground">· needs a restart</span>
        )}
      </div>
      {locked && <div className="mt-1.5">{<LayerLock />}</div>}
    </div>
  );
}

/**
 * Why a field can't be edited.
 *
 * Worth its own component because the reason is not obvious and the failure it
 * prevents is silent: `config.toml` sits *below* the environment in precedence,
 * so writing an env-owned field would produce a saved value that the next boot
 * ignores. Saying so beats disabling an input with no explanation.
 */
function LayerLock() {
  return (
    <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
      <Lock className="mt-0.5 size-3 shrink-0" />
      <span>
        Set by an environment variable on this host, which outranks{" "}
        <code className="font-mono">config.toml</code>. Change it where the host is deployed.
      </span>
    </p>
  );
}

// ---------------------------------------------------------------------------
// The question screens
// ---------------------------------------------------------------------------

function AccountStep({
  value,
  onChange,
  password,
  onPasswordChange,
  passwordProblem: problem,
  onEnter,
  required,
  mailed,
}: {
  value: string;
  onChange: (v: string) => void;
  password: string;
  onPasswordChange: (v: string) => void;
  passwordProblem?: string;
  onEnter: () => void;
  required: boolean;
  /** Whether this host mails: decides whether the login is asked for as a mailbox. */
  mailed: boolean;
}) {
  return (
    <div className="space-y-5">
      <div>
        {/* Same rhythm as every other question: the heading and its hint are one
            sentence, and the gap belongs before the field. */}
        <Label
          htmlFor="setup-email"
          className="text-base font-medium leading-snug"
          data-testid="setup-question"
        >
          {mailed ? "What's your email?" : "How will you sign in?"}
        </Label>
        <p className="text-xs leading-snug text-muted-foreground">
          {required
            ? mailed
              ? "This is how you sign back in, and the only address that can administer the company."
              : "An email address or a username — this host sends no mail, so it only has to be something you'll remember. It's the only login that can administer the company."
            : // Not the no-sign-in case: that host does not render this step at
              // all. What is left is a host that already serves a company, where
              // the roster it has can already administer it.
              "Optional on this host — it already serves a company, so this is only how you get back in."}
        </p>
        <Input
          id="setup-email"
          autoFocus
          type={mailed ? "email" : "text"}
          autoComplete="username"
          value={value}
          placeholder={mailed ? "you@example.com" : "you@example.com or admin"}
          data-testid="setup-field-email"
          className="mt-2.5"
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onEnter();
          }}
        />
      </div>

      {/* Asked here, with the login, because it is the other half of the same
          answer: without it the address is a standing invite the host may have
          no way to deliver, and setup finished by pointing at a mailbox. With
          it the apply creates the account and the finish line signs them in. */}
      {required && (
        <NewPasswordField
          id="setup-field-password"
          label="Password"
          value={password}
          onChange={onPasswordChange}
          onEnter={onEnter}
          problem={problem}
        />
      )}
    </div>
  );
}

/**
 * Step 0: which of the two setup paths this instance takes.
 *
 * Asks nothing of the host — the answer decides which step-1 screen mounts
 * next and nothing else.
 */
function SetupWayStep({
  value,
  onChange,
}: {
  value: SetupWay | null;
  onChange: (way: SetupWay) => void;
}) {
  return (
    <div>
      <h2 className="text-base font-medium leading-snug" data-testid="setup-question">
        How would you like to set this up?
      </h2>
      <p className="text-xs leading-snug text-muted-foreground">
        You can change any of it later from Connections.
      </p>

      <div className="mt-2.5 space-y-2">
        {(Object.keys(SETUP_WAY_COPY) as SetupWay[]).map((way) => {
          const copy = SETUP_WAY_COPY[way];
          const active = value === way;
          return (
            <button
              key={way}
              type="button"
              onClick={() => onChange(way)}
              data-testid={`setup-way-${way}`}
              aria-pressed={active}
              className={cn(
                "w-full rounded-lg border p-3 text-left transition-colors hover:bg-muted",
                active && "border-primary bg-muted",
              )}
            >
              <div className="text-sm font-medium">{copy.label}</div>
              <div className="mt-0.5 text-xs text-muted-foreground">{copy.hint}</div>
            </button>
          );
        })}
      </div>
    </div>
  );
}

/** What a connection test was asked with — the answers its verdict is true of. */
interface ProbeAnswers {
  provider: string;
  key: string;
  baseUrl: string;
}

/**
 * Run one connection test and settle the verdict — unless the answers moved
 * under it.
 *
 * The staleness rule is why this is shared rather than written per step. Both
 * step-1 screens leave their inputs live while a test is in flight, so an
 * answer can arrive after the operator has changed the thing it was asked
 * about, and a verdict is only ever true of the answers it was asked with. A
 * late `ok` overwriting a settled `skipped` submits a passing tick for a
 * credential nobody is using.
 */
async function runConnectionTest(
  client: OpenCompanyClient,
  asked: ProbeAnswers,
  live: () => ProbeAnswers,
  onTested: (t: TestState) => void,
): Promise<void> {
  const stale = () => {
    const now = live();
    return (
      asked.provider !== now.provider || asked.key !== now.key || asked.baseUrl !== now.baseUrl
    );
  };

  onTested({ kind: "testing" });
  try {
    const result = await testInference(client, {
      provider: asked.provider,
      key: asked.key || null,
      baseUrl: asked.baseUrl || null,
    });
    if (stale()) return;
    onTested(
      result.ok
        ? { kind: "ok", baseUrl: result.baseUrl, model: result.model }
        : { kind: "failed", error: result.error ?? "Could not reach the provider." },
    );
  } catch (err: unknown) {
    if (stale()) return;
    onTested({
      kind: "failed",
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

/**
 * Managed step 1: the Account page's "Connect to TinyHumans" dialog, asked as
 * a wizard step.
 *
 * Same ask, same link out, same one key. What it is *not* is the model step:
 * there is no provider to pick, because taking the managed way already picked
 * one, and there is no BYOK endpoint to name.
 *
 * ## Where the key goes
 *
 * Not into `config.toml`, and not through the wizard's own one-secret
 * inference write. It is the **company's** TinyHumans account key, and the
 * host fans it out from there the way `PUT …/credential` does for the Account
 * page: the Composio copy, the LLM copy, the `tinyhumans` row with the model
 * this step's probe reached, and the company default — each only where nothing
 * of its own is already set.
 *
 * The write is deferred to the finish rather than made here, because the
 * fan-out fills a company's slots and the company does not exist yet — the
 * apply is what creates it. So this step collects and proves; the host reports
 * back what the fan-out actually did, slot by slot, on the completion screen.
 *
 * No one-click grant button (`link/start`): that flow is company-scoped too,
 * and adding an entry point for it is explicitly out of scope here. A link out
 * to mint a key by hand is the whole of the alternative, exactly as the
 * Account dialog offers it.
 */
function ManagedLoginStep({
  status,
  client,
  value,
  onChange,
  tested,
  onTested,
}: {
  status: SetupStatus;
  client: OpenCompanyClient;
  value: string;
  onChange: (v: string) => void;
  tested: TestState;
  onTested: (t: TestState) => void;
}) {
  const key = value.trim();
  const keySource = tinyhumansKeySource(status);
  // Read from inside a call that started before it — see `runConnectionTest`.
  const live = useRef<ProbeAnswers>({ provider: MANAGED_PROVIDER, key, baseUrl: "" });
  live.current = { provider: MANAGED_PROVIDER, key, baseUrl: "" };

  const run = async () => {
    // Guards the Enter shortcut as well as the button: an empty box would
    // probe the host's own credential and report a pass for a key this
    // operator never gave.
    if (!key) return;
    await runConnectionTest(
      client,
      { provider: MANAGED_PROVIDER, key, baseUrl: "" },
      () => live.current,
      onTested,
    );
  };

  return (
    <div className="space-y-7">
      <div>
        <Label
          htmlFor="setup-key"
          className="text-base font-medium leading-snug"
          data-testid="setup-question"
        >
          Connect to TinyHumans
        </Label>
        <p className="text-xs leading-snug text-muted-foreground" data-testid="setup-model-prompt">
          Paste your account key. We&apos;ll check it reaches before going any further.
        </p>

        <div className="mt-2.5 flex items-center gap-2">
          <Input
            id="setup-key"
            autoFocus
            type="password"
            autoComplete="off"
            spellCheck={false}
            value={value}
            placeholder="th-…"
            data-testid="setup-field-key"
            onChange={(e) => onChange(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void run();
            }}
          />
          <Button
            type="button"
            variant={tested.kind === "ok" ? "outline" : "default"}
            disabled={tested.kind === "testing" || !key}
            onClick={() => void run()}
            data-testid="setup-test-connection"
            className="shrink-0"
          >
            {tested.kind === "testing" ? (
              <>
                <Loader2 className="size-4 animate-spin" />
                Testing…
              </>
            ) : tested.kind === "ok" ? (
              "Test again"
            ) : (
              "Test connection"
            )}
          </Button>
        </div>

        {/* What one key actually buys, said before it is asked for rather than
            discovered afterwards. Named in surfaces rather than in slots: the
            operator has not seen the Connections pages yet. */}
        <p className="mt-2 text-xs leading-snug text-muted-foreground" data-testid="setup-key-fills">
          One key: your team&apos;s model, and the account it connects tools like Gmail and
          Slack through. Anything you set up yourself later keeps its own key.
        </p>

        {/* A link out, not a grant. The key is minted on their own dashboard
            and pasted back, which works with no company in existence — the
            whole of when this step runs. The dashboard is the one belonging
            to the hub this host is on, as the host reports it. */}
        {keySource && (
          <p className="mt-2 text-xs leading-snug text-muted-foreground">
            Don&apos;t have one yet?{" "}
            <a
              href={keySource.url}
              target="_blank"
              rel="noreferrer"
              data-testid="setup-key-get-link"
              className={cn(buttonVariants({ variant: "outline", size: "sm" }), "ml-1")}
            >
              Get an API key
              <ExternalLink className="size-3.5" />
            </a>
          </p>
        )}
      </div>

      <div className="space-y-2">
        {tested.kind === "ok" && (
          <p className="text-sm leading-snug text-status-done-text" data-testid="setup-test-ok">
            Reached {tested.baseUrl}
            {tested.model ? ` using ${tested.model}` : ""} and got a reply.
          </p>
        )}
        {tested.kind === "failed" && (
          <Alert variant="destructive" data-testid="setup-test-failed">
            <AlertTriangle />
            <AlertTitle>That didn&apos;t connect</AlertTitle>
            <AlertDescription>{tested.error}</AlertDescription>
          </Alert>
        )}
      </div>
    </div>
  );
}

/** The connection verdict. See `tested` on the wizard for why each state exists. */
type TestState =
  | { kind: "untested" }
  | { kind: "testing" }
  | { kind: "ok"; baseUrl: string; model?: string | null }
  | { kind: "failed"; error: string }
  | { kind: "hosted" };

// ---------------------------------------------------------------------------
// Review, and the team as reviewed
// ---------------------------------------------------------------------------

/**
 * The team, before it exists.
 *
 * The screen that earns the ownership. People value what they had a hand in
 * shaping, and this is the honest place to catch a wrong guess — while it is
 * still four rows in a browser rather than six records on a host.
 *
 * It also says where the team came from, in a sentence. An operator shown a
 * curated roster with no indication assumes a model read their answers and
 * produced it, and judges the product on a team it never designed.
 */
function ReviewStep({
  designing,
  designError,
  roster,
  name,
  onRoster,
  onRetry,
  changed,
  restartKeys,
  status,
  hostModel,
  email,
  built,
}: {
  designing: boolean;
  designError: string | null;
  roster: SetupRoster | null;
  /** What the company will be called, as answered on the business step. */
  name: string;
  onRoster: (roster: SetupRoster) => void;
  onRetry: () => void;
  changed: Record<string, string | null>;
  restartKeys: string[];
  status: SetupStatus;
  /**
   * Whether the host answered for the model itself — reached, not merely
   * resolved, and never asked about on screen. An operator who saw the model
   * step and tested it there has already been told, and is not owed the line.
   */
  hostModel: boolean;
  email: string;
  /** Non-null once the apply is building, so the button reads as progress. */
  built: number | null;
}) {
  if (designing) {
    return (
      <div className="flex flex-col items-center gap-3 py-12" data-testid="setup-designing">
        <Loader2 className="size-6 animate-spin text-primary" />
        {/* Not "Designing your team…". The console cannot know which path the
            host will take, and on a laptop with no credential nothing designs
            anything — the curated team comes back in milliseconds under a word
            that claimed a model had read their answers. This is true either
            way; the review screen then says which actually happened. */}
        <p className="text-sm text-muted-foreground">Putting your team together…</p>
      </div>
    );
  }

  if (designError || !roster) {
    return (
      <Alert variant="destructive" data-testid="setup-design-error">
        <AlertTriangle />
        <AlertTitle>We couldn&apos;t design a team</AlertTitle>
        <AlertDescription className="space-y-2">
          <p>{designError ?? "The host returned nothing to review."}</p>
          <Button size="sm" variant="outline" onClick={onRetry}>
            Try again
          </Button>
        </AlertDescription>
      </Alert>
    );
  }

  /** Whether this roster is a template's own, seeded rather than rebuilt. */
  const shipped = roster.source === "preset";

  const drop = (index: number) =>
    onRoster({ ...roster, agents: roster.agents.filter((_, i) => i !== index) });

  const rename = (index: number, role: string) =>
    onRoster({
      ...roster,
      agents: roster.agents.map((a, i) => (i === index ? { ...a, role } : a)),
    });

  return (
    <div className="space-y-4" data-testid="setup-review">
      <div>
        <h2 className="text-base font-medium leading-snug">Your team</h2>
        <p className="text-xs leading-snug text-muted-foreground">
          {roster.source === "model"
            ? "Built from what you told us. Rename or drop anyone — you can add more later."
            : roster.source === "preset"
              ? // The template's own team, which is what the operator picked
                // from a list that told them its size. Said plainly, because
                // this screen used to show a *different* standard team under
                // the same heading and call it theirs.
                "The team this template ships with, exactly as it comes. You can rename or drop anyone once you're inside."
              : roster.reason === "not_designable"
              ? "A standard team for your industry — there wasn't enough in your answers to design one around. Go back and say more about the business, or rename and drop anyone here."
              : roster.reason === "output_budget_exhausted"
                ? "A standard team for your industry — the model ran out of output room before it finished tailoring one. Try again, or rename and drop anyone here. The team request is capped at 4,000 output tokens, so a model with a larger limit alone won't help."
              : roster.reason === "model_unreachable"
                ? "A standard team for your industry — we couldn't reach the model to tailor it right now. Check the connection, or rename and drop anyone here."
                : "A solid standard team for your industry — we couldn't reach a model to tailor it. Rename or drop anyone, and add a key later to redesign."}
        </p>
      </div>

      {/* People, not form rows.
          This was five bare text inputs stacked in a column, which is what a
          settings page looks like — and this is the one screen in the product
          where someone meets their company for the first time. The field is
          still there and still the first thing focus lands on; it just stops
          announcing itself as a form until you go to use it. */}
      <ul className="divide-y rounded-xl border" data-testid="setup-review-list">
        {roster.agents.map((agent, i) => (
          <li
            key={`${agent.role}-${i}`}
            className="group flex items-center gap-3 p-3"
            data-testid="setup-review-agent"
          >
            <span
              aria-hidden
              className={cn(
                "flex size-9 shrink-0 items-center justify-center rounded-full text-xs font-medium",
                TEAM_TONES[toneFor(agent.role)] ?? TEAM_TONES.sky,
              )}
            >
              {initials(agent.name || agent.role)}
            </span>
            <div className="min-w-0 flex-1">
              {/* A shipped roster is read here, not edited.
                  Not a restriction for its own sake: an edited roster can only
                  be sent back as a *designed* company, and the designed path is
                  bounded at six agents — so renaming one row of an
                  eight-agent template would silently drop two of them, and
                  the operator would find out by not finding them. Every one of
                  these is renameable and removable from the console the moment
                  setup finishes, where no such bound applies. */}
              {shipped ? (
                <p className="px-1 font-medium" data-testid="setup-review-role">
                  {agent.role}
                </p>
              ) : (
                <Input
                  value={agent.role}
                  aria-label={`Role for ${agent.role}`}
                  data-testid="setup-review-role"
                  onChange={(e) => rename(i, e.target.value)}
                  className="h-7 border-transparent bg-transparent px-1 font-medium shadow-none hover:border-input focus-visible:border-input"
                />
              )}
              <p className="truncate px-1 text-xs text-muted-foreground">{agent.description}</p>
            </div>
            {!shipped && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => drop(i)}
                aria-label={`Remove ${agent.role}`}
                data-testid="setup-review-remove"
                className="shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100"
              >
                Remove
              </Button>
            )}
          </li>
        ))}
      </ul>

      {roster.agents.length === 0 && (
        <Alert>
          <AlertTriangle />
          <AlertTitle>That&apos;s everyone gone</AlertTitle>
          <AlertDescription>
            A company needs at least one agent. Add one back, or start again.
          </AlertDescription>
        </Alert>
      )}

      {/* What the host checked, reported whichever way it came out.
          The checklist is the operator's own words, split by the host, and the
          verdict is set maths over that list — not the design pass's opinion of
          its own work. Reporting only the good case would make this decoration;
          the gap is the half worth showing, because it is the one they can do
          something about. */}
      {roster.source === "model" && (roster.jobs?.length ?? 0) > 0 && (
        <div
          className="rounded-lg border p-3 text-sm"
          data-testid="setup-coverage"
        >
          {(roster.uncovered?.length ?? 0) === 0 ? (
            <p className="text-muted-foreground">
              Every job you listed has an owner on this team.
            </p>
          ) : (
            <>
              <p className="font-medium">Nobody owns this yet</p>
              <ul className="mt-1.5 space-y-1 text-muted-foreground">
                {roster.uncovered?.map((job) => (
                  <li key={job} data-testid="setup-uncovered-job">
                    {job}
                  </li>
                ))}
              </ul>
              <p className="mt-2 text-sm leading-snug text-muted-foreground">
                You can still continue — add someone for it here, or later from
                the Company page.
              </p>
            </>
          )}
        </div>
      )}

      {/* Stated, not asked. Nobody five screens in can answer a governance
          question; they can recognise a sentence and change it in Advanced. */}
      <div className="rounded-lg border border-dashed p-3 text-sm text-muted-foreground">
        {/* What they can do on day one, said before they find out by trying.
            A roster reads as a set of capabilities: "Social Media Manager —
            owns posting and engagement" is taken to mean it can post. It
            cannot, yet, and nothing on this screen used to say so. Every
            designed agent starts with the workspace and nothing outward,
            because reaching a real account needs an account connected first —
            an act only a person can perform, and one there has been no
            opportunity to perform yet.

            This is the same failure as the twelve invented agents that used
            to render here: offering something the host cannot honour. The fix
            is the sentence, not a wider tool grant. */}
        <p data-testid="setup-reach">
          Your team starts with its own workspace — reading, writing, drafting.
          Posting, emailing and anything else that touches an outside account
          needs that account connected first, from Settings.
        </p>
        <p className="mt-1">
          Anything that leaves the company — sending, publishing, spending — waits
          for you until you say otherwise.
        </p>
        {/* Said, because the alternative is an operator who was never asked for
            a model wondering later where theirs came from. A skipped question
            still owes its answer somewhere. */}
        {hostModel && (
          <p className="mt-1" data-testid="setup-host-model">
            The model comes with this host, so there was no key to supply and none
            is stored against your company.
          </p>
        )}
        {email.trim() && (
          <p className="mt-1">
            You&apos;ll sign in as <span className="font-medium text-foreground">{email.trim()}</span>.
          </p>
        )}
        {name.trim() && (
          <p className="mt-1" data-testid="setup-review-name">
            The company will be called{" "}
            <span className="font-medium text-foreground">{name.trim()}</span>, permanently —
            go back to the business step to change it.
          </p>
        )}
      </div>

      {built !== null && (
        <p className="text-sm text-muted-foreground" data-testid="setup-building">
          Building {built} {built === 1 ? "agent" : "agents"}…
        </p>
      )}

      {(Object.keys(changed).length > 0 || restartKeys.length > 0) && (
        <details className="rounded-lg border p-3">
          <summary className="cursor-pointer text-sm font-medium">
            Settings this will write ({Object.keys(changed).length})
          </summary>
          <ul className="mt-2 space-y-1 text-xs text-muted-foreground">
            {Object.keys(changed).map((key) => (
              <li key={key}>
                <code className="font-mono">{key}</code>
                {restartKeys.includes(key) && " — takes effect after a restart"}
              </li>
            ))}
          </ul>
        </details>
      )}

      {status.companies.length > 0 && (
        <p className="text-xs text-muted-foreground">
          This host already serves a company, so no new one will be created.
        </p>
      )}
    </div>
  );
}

/**
 * Advanced, as a step of its own.
 *
 * It was a disclosure hanging under the footer, which made the page appear to
 * end twice and made these four subjects feel like a cupboard rather than part
 * of setting up. They are a step now, in the same sequence as everything else,
 * and skippable by pressing on — which is what "advanced" should mean: present
 * and passed over, not hidden and hunted for.
 *
 * Both groups on one screen, for the same reason the business questions share
 * one: they are short, related, and splitting them would be another Next press
 * for settings most people will never touch.
 */
function AdvancedStep({
  status,
  values,
  set,
}: {
  status: SetupStatus;
  values: Record<string, string>;
  set: (key: string, value: string) => void;
}) {
  return (
    <div className="space-y-7" data-testid="setup-advanced">
      <div>
        <h2 className="text-base font-medium leading-snug" data-testid="setup-question">
          Anything you want to change?
        </h2>
        <p className="text-xs leading-snug text-muted-foreground">
          Every one of these already has a working default. Press on if none of
          it matters to you — written to{" "}
          <code className="font-mono text-xs">{status.config_path}</code>.
        </p>
      </div>

      {ADVANCED_GROUPS.map((group) => (
        // Each subject is its own bounded card. Sections running together down
        // one scroll is what made this read as a dump — nothing told you where
        // "how this host runs" ended and "what it can reach" began.
        <section key={group.id} className="rounded-xl border">
          <div className="border-b px-4 py-3">
            <h3 className="text-base font-medium leading-snug">{group.title}</h3>
            <p className="text-xs leading-snug text-muted-foreground">{group.hint}</p>
          </div>
          <div className="space-y-5 px-4 py-4">
            {fieldsFor(status, group.fields).map((f) => (
              <FieldRow
                key={f.key}
                field={f}
                value={values[f.key] ?? ""}
                onChange={(v) => set(f.key, v)}
              />
            ))}
          </div>
        </section>
      ))}
    </div>
  );
}

/**
 * Everything we ask about the business, on one screen.
 *
 * These were three screens, one field each. That is the right shape when a
 * question needs the operator's whole attention, and the wrong one here: the
 * three are a single thought — *what you do, who you want, what you want off
 * your plate* — and splitting them made two of the three feel like padding
 * between the interesting one and the end.
 *
 * Read together they also answer each other. Seeing "what do you want to
 * automate" while the industry answer is still on screen is what makes someone
 * write "order dispatch" rather than "operations".
 */
function BusinessStep({
  draft,
  templates,
  template,
  name,
  onTemplate,
  onName,
  onChange,
  onEnter,
  modelless,
}: {
  draft: SetupDraft;
  templates: SetupStatus["templates"];
  template: string;
  /** What the company will be called. */
  name: string;
  onTemplate: (id: string) => void;
  onName: (name: string) => void;
  onChange: (update: (d: SetupDraft) => SetupDraft) => void;
  onEnter: () => void;
  /**
   * Whether this company is being built without a model, which is what the
   * other two questions need to be worth asking.
   *
   * They are the design brief: `automate` becomes a numbered job list the
   * roster is designed against and checked for coverage, and `teamHint` is a
   * request added on top of it. Neither happens without a model — the host
   * falls back to a curated team matched on keywords, where `automate` and
   * `teamHint` score one point each against `industry`'s three and nothing
   * else reads them.
   *
   * So they are not asked. Asking somebody to describe the work they want
   * taken off their plate, under copy promising their team is built around
   * it, and then staffing them from a keyword match, is a worse answer than
   * one fewer question.
   */
  modelless: boolean;
}) {
  const jobs = jobItems(draft.automate);

  return (
    <div className="space-y-7">
      <div>
        {/* Label and hint are one sentence, so they sit tight together; the
            breathing room belongs *before the field*, not inside the sentence.
            A uniform `space-y` gave all three the same gap and the question
            read as three unrelated lines. */}
        <Label htmlFor="setup-template" className="text-base font-medium leading-snug">
          What kind of company are you setting up?
        </Label>
        <p className="text-xs leading-snug text-muted-foreground">
          Pick one of the teams bundled with OpenCompany. You can tailor it below.
        </p>
        {templates.length > 0 ? (
          <select
            id="setup-template"
            autoFocus
            value={template}
            data-testid="setup-field-template"
            className="mt-2.5 h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
            onChange={(event) => onTemplate(event.target.value)}
          >
            <option value="" disabled>
              Choose a company template…
            </option>
            {templates.map((option) => (
              <option key={option.id} value={option.id}>
                {option.name} ({option.agent_count} agents)
              </option>
            ))}
          </select>
        ) : (
          <Input
            id="setup-industry"
            autoFocus
            value={draft.industry}
            placeholder="e.g. E-commerce — I sell homeware online"
            data-testid="setup-field-industry"
            className="mt-2.5"
            onChange={(e) => onChange((d) => ({ ...d, industry: e.target.value }))}
            onKeyDown={(e) => {
              if (e.key === "Enter") onEnter();
            }}
          />
        )}
        {template && (
          <Input
            id="setup-industry"
            value={draft.industry}
            placeholder="Optional details about what makes yours different"
            data-testid="setup-field-industry"
            className="mt-2"
            onChange={(e) => onChange((d) => ({ ...d, industry: e.target.value }))}
            onKeyDown={(e) => {
              if (e.key === "Enter") onEnter();
            }}
          />
        )}
      </div>

      <div>
        <Label htmlFor="setup-company-name" className="text-base font-medium leading-snug">
          What should we call it?
        </Label>
        <p className="text-xs leading-snug text-muted-foreground">
          This names the company and its id. You can change it now; you can&apos;t later.
        </p>
        <Input
          id="setup-company-name"
          data-testid="setup-company-name"
          value={name}
          placeholder="Your company's name"
          className="mt-2.5"
          onChange={(e) => onName(clampToSetupCompanyNameLimit(e.target.value))}
          onKeyDown={(e) => {
            if (e.key === "Enter") onEnter();
          }}
        />
      </div>

      {/* Both questions exist to brief a model. Without one they are asked and
          then not acted on — see `modelless`. */}
      {!modelless && (
      <div>
        <Label htmlFor="setup-automate" className="text-base font-medium leading-snug">
          What are you trying to automate?
        </Label>
        <p className="text-xs leading-snug text-muted-foreground">
          List whatever comes to mind. This is what your team gets built around.
        </p>
        <Textarea
          id="setup-automate"
          className="mt-2.5 min-h-0"
          rows={2}
          value={draft.automate}
          placeholder="e.g. Meta ads, order dispatch, daily sales reports"
          data-testid="setup-field-automate"
          onChange={(e) => onChange((d) => ({ ...d, automate: e.target.value }))}
        />
        {/* Their own words, split the way the host splits them — not a guess at
            what they meant. This is the checklist the roster is judged against,
            so showing it here is what makes a bad split fixable by the person
            who typed it rather than a silent input to a prompt. */}
        {jobs.length > 1 && (
          <p className="mt-2 text-sm leading-snug text-muted-foreground" data-testid="setup-jobs">
            {jobs.length} jobs — each one needs an owner on your team.
          </p>
        )}
      </div>
      )}

      {!modelless && (
      <div>
        <Label htmlFor="setup-teamHint" className="text-base font-medium leading-snug">
          Anyone in particular you need on the team?
          <span className="ml-1.5 text-sm font-normal text-muted-foreground">Optional</span>
        </Label>
        <p className="text-xs leading-snug text-muted-foreground">
          We&apos;ll suggest a team either way — this just adds to it.
        </p>
        <Textarea
          id="setup-teamHint"
          className="mt-2.5 min-h-0"
          rows={2}
          value={draft.teamHint}
          placeholder="e.g. someone chasing the customers who go quiet"
          data-testid="setup-field-teamHint"
          onChange={(e) => onChange((d) => ({ ...d, teamHint: e.target.value }))}
        />
      </div>
      )}
    </div>
  );
}
