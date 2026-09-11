// The provider catalogue, console side: data, and nothing else.
//
// This is a MIRROR of `src/company/inference/catalogue.rs`, not a second
// opinion. Five Rust tests (`the_console_mirror_lists_the_same_…` in
// `src/company/inference/catalogue.rs`) parse this file and fail when the two
// disagree, because the alternative — two
// hand-maintained copies and nothing watching — is exactly the state this module
// replaces. Provider knowledge lived in six places here with no shared source
// and two had already drifted before anyone looked.
//
// ## Editing rules, which the cross-language test depends on
//
// The three exported arrays are parsed by a deliberately simple reader on the
// Rust side. Keep them literal:
//
//   - one field per line, in the order `slug, label, endpoint, auth,
//     keyPlaceholder` (and the analogous order for the other two arrays);
//   - double-quoted string values;
//   - NO comments inside an array entry. Put the prose above the array, where
//     it is easier to read anyway.
//
// A richer parser would let the file drift into a shape the test silently stops
// checking. A strict one fails loudly, which is the entire point.
//
// ## Why `tone` is not here
//
// openhuman's mirror carries a Tailwind class string per provider, raw hex
// included. `scripts/ci/assert-design-tokens.sh` rejects raw hex on purpose, so
// per-provider colour is a design-token decision made where the rows are
// rendered — not a column of this table. The table stays presentation-free.

/** How a provider expects its credential presented. */
export type AuthStyle = "bearer" | "anthropic" | "none";

/** The `anthropic-version` header value that rides with `anthropic` auth. */
export const ANTHROPIC_VERSION = "2023-06-01";

/** One hosted provider a company can be pointed at. */
export interface CloudProvider {
  /** Stable routing key — what a routing entry names and a credential slot is keyed on. */
  slug: string;
  /** Display label. Never used for routing. */
  label: string;
  /** The OpenAI-compatible base URL, as a preset. */
  endpoint: string;
  /** How the credential is presented. */
  auth: AuthStyle;
  /** What a key for this provider looks like, for the input placeholder. */
  keyPlaceholder?: string;
}

/**
 * The 26 hosted providers the add dialog offers.
 *
 * The endpoints are presets rather than a pattern, and the paths are the reason:
 * `/openai/v1`, `/inference/v1`, `/v1beta/openai`, `/v1/openai`, `/v3/openai`,
 * `/api/paas/v4`, `/api/gateway`, `/step_plan/v1`. Deriving one as
 * `https://{host}/v1` is wrong for about a third of this list, which is why the
 * cloud category never asks the operator to type an endpoint.
 *
 * Two rows carry a fix rather than just a value. `anthropic` is the **only**
 * non-bearer entry, so a renderer that assumes one auth style breaks exactly the
 * provider people try first. `minimax` points at the OpenAI-compatible `/v1`
 * surface; its previous `/anthropic` base with Anthropic auth 404'd both chat and
 * the model listing. Neither is a candidate for tidying.
 */
export const CLOUD_PROVIDERS: readonly CloudProvider[] = [
  {
    slug: "openai",
    label: "OpenAI",
    endpoint: "https://api.openai.com/v1",
    auth: "bearer",
    keyPlaceholder: "sk-...",
  },
  {
    slug: "anthropic",
    label: "Anthropic",
    endpoint: "https://api.anthropic.com/v1",
    auth: "anthropic",
    keyPlaceholder: "sk-ant-...",
  },
  {
    slug: "openrouter",
    label: "OpenRouter",
    endpoint: "https://openrouter.ai/api/v1",
    auth: "bearer",
    keyPlaceholder: "sk-or-...",
  },
  {
    slug: "orcarouter",
    label: "OrcaRouter",
    endpoint: "https://api.orcarouter.ai/v1",
    auth: "bearer",
    keyPlaceholder: "sk-orca-...",
  },
  {
    slug: "gmi",
    label: "GMI",
    endpoint: "https://api.gmi-serving.com/v1",
    auth: "bearer",
    keyPlaceholder: "eyJ....",
  },
  {
    slug: "fireworks",
    label: "Fireworks",
    endpoint: "https://api.fireworks.ai/inference/v1",
    auth: "bearer",
    keyPlaceholder: "fw-...",
  },
  {
    slug: "moonshot",
    label: "Kimi (Moonshot)",
    endpoint: "https://api.moonshot.ai/v1",
    auth: "bearer",
    keyPlaceholder: "sk-...",
  },
  {
    slug: "groq",
    label: "Groq",
    endpoint: "https://api.groq.com/openai/v1",
    auth: "bearer",
    keyPlaceholder: "gsk_...",
  },
  {
    slug: "mistral",
    label: "Mistral",
    endpoint: "https://api.mistral.ai/v1",
    auth: "bearer",
  },
  {
    slug: "deepseek",
    label: "DeepSeek",
    endpoint: "https://api.deepseek.com/v1",
    auth: "bearer",
    keyPlaceholder: "sk-...",
  },
  {
    slug: "together",
    label: "Together AI",
    endpoint: "https://api.together.xyz/v1",
    auth: "bearer",
  },
  {
    slug: "google",
    label: "Google Gemini",
    endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
    auth: "bearer",
  },
  {
    slug: "cerebras",
    label: "Cerebras",
    endpoint: "https://api.cerebras.ai/v1",
    auth: "bearer",
  },
  {
    slug: "xai",
    label: "xAI",
    endpoint: "https://api.x.ai/v1",
    auth: "bearer",
  },
  {
    slug: "huggingface",
    label: "Hugging Face",
    endpoint: "https://router.huggingface.co/v1",
    auth: "bearer",
    keyPlaceholder: "hf_...",
  },
  {
    slug: "nvidia",
    label: "NVIDIA",
    endpoint: "https://integrate.api.nvidia.com/v1",
    auth: "bearer",
  },
  {
    slug: "zai",
    label: "Z.AI",
    endpoint: "https://api.z.ai/api/paas/v4",
    auth: "bearer",
  },
  {
    slug: "minimax",
    label: "MiniMax",
    endpoint: "https://api.minimax.io/v1",
    auth: "bearer",
  },
  {
    slug: "stepfun",
    label: "StepFun",
    endpoint: "https://api.stepfun.ai/step_plan/v1",
    auth: "bearer",
  },
  {
    slug: "kilocode",
    label: "Kilo Code",
    endpoint: "https://api.kilo.ai/api/gateway",
    auth: "bearer",
  },
  {
    slug: "deepinfra",
    label: "DeepInfra",
    endpoint: "https://api.deepinfra.com/v1/openai",
    auth: "bearer",
  },
  {
    slug: "novita",
    label: "Novita",
    endpoint: "https://api.novita.ai/v3/openai",
    auth: "bearer",
  },
  {
    slug: "venice",
    label: "Venice",
    endpoint: "https://api.venice.ai/api/v1",
    auth: "bearer",
  },
  {
    slug: "vercel-ai-gateway",
    label: "Vercel AI Gateway",
    endpoint: "https://ai-gateway.vercel.sh/v1",
    auth: "bearer",
  },
  {
    slug: "sumopod",
    label: "SumoPod",
    endpoint: "https://ai.sumopod.com/v1",
    auth: "bearer",
    keyPlaceholder: "sk-...",
  },
  {
    slug: "modelscope",
    label: "ModelScope",
    endpoint: "https://api-inference.modelscope.cn/v1",
    auth: "bearer",
    keyPlaceholder: "ms-...",
  },
];

/** One model runtime reachable over an endpoint rather than a vendor account. */
export interface LocalRuntime {
  slug: string;
  label: string;
  /** A starting endpoint where one is conventional; the operator still confirms it. */
  defaultEndpoint?: string;
  /** Whether this runtime also wants a credential. */
  needsKey: boolean;
}

/**
 * The three local runtimes.
 *
 * On a server-side product these mean the **host's** localhost, not the
 * operator's laptop — openhuman is a desktop app, where they are the same
 * machine. The category still ports, and it is the reason loopback is an
 * explicit allowance in the connect flow's SSRF guard rather than an oversight.
 *
 * `omlx` is the only one that wants a key as well as an endpoint.
 */
export const LOCAL_RUNTIMES: readonly LocalRuntime[] = [
  {
    slug: "ollama",
    label: "Ollama",
    defaultEndpoint: "http://localhost:11434",
    needsKey: false,
  },
  {
    slug: "lmstudio",
    label: "LM Studio",
    needsKey: false,
  },
  {
    slug: "omlx",
    label: "OMLX",
    needsKey: true,
  },
];

/** A credential another command-line tool already holds. */
export interface CliLogin {
  /** What the dialog's option is called. */
  optionSlug: string;
  /** What the resulting credential is stored under — not always the same. */
  storedSlug: string;
  label: string;
  /** Whether connecting this option runs the probe. */
  probes: boolean;
}

/**
 * The two CLI logins.
 *
 * Codex is the trap: its login **is** an OpenAI credential, so it stores under
 * the `openai` slug and shows up as the OpenAI row. Keying its already-connected
 * check on the literal `codex` never matches, so the dialog would offer Codex
 * forever. It gets no row of its own on purpose — a second row would imply a
 * second connection the operator could remove separately.
 *
 * `claude-code` hands over no key, so there is nothing for a probe to present.
 *
 * On a server-side host this category has no options. The dialog renders the
 * group saying so rather than hiding it: the shape is then already right if a
 * delegated credential ever becomes available, and an empty labelled group is
 * more honest than a missing one.
 */
export const CLI_LOGINS: readonly CliLogin[] = [
  {
    optionSlug: "claude-code",
    storedSlug: "claude-code",
    label: "Claude Code",
    probes: false,
  },
  {
    optionSlug: "codex",
    storedSlug: "openai",
    label: "Codex",
    probes: true,
  },
];

/**
 * Authority hosts that serve Azure AI Foundry / Azure OpenAI inference.
 *
 * Azure separates the base model id a deployment was made from
 * (`gpt-5.6-terra-2026-07-09`) from the deployment name (`gpt-5.6-terra`) that
 * actually routes the request, and the request body's `model` field keys on the
 * deployment name while `/models` publishes base model ids. So an Azure endpoint
 * forces free-text model entry: a dropdown sourced from the catalog makes the
 * only correct value unreachable.
 *
 * `inference.ai.azure.com` and `models.ai.azure.com` are deliberately absent.
 * Those are the Foundry serverless endpoints, which key `model` on the model
 * name — classifying them would relabel a correct model id as a "deployment
 * name" and mislead the operator in the one place this rule exists to clarify.
 * Do not add them for symmetry.
 */
export const AZURE_ENDPOINT_HOSTS: readonly string[] = [
  "openai.azure.com",
  "services.ai.azure.com",
  "cognitiveservices.azure.com",
  "openai.azure.us",
  "openai.azure.cn",
];

/**
 * Copy for the add-provider dialog, ported verbatim.
 *
 * The categories are not three slices of one decision, they are three different
 * questions: a cloud provider wants an API key, a local runtime wants an
 * endpoint on this machine, a CLI login wants nothing because another tool
 * already holds the credential. One flat list makes the operator infer that from
 * a group heading; a select per category says it outright.
 */
export const COPY = {
  groupCloud: "Cloud",
  groupLocal: "Local runtimes",
  groupCli: "CLI logins",
  placeholderCloud: "Choose a cloud provider…",
  placeholderLocal: "Choose a local runtime…",
  placeholderCli: "Choose a CLI login…",
  helperCloud: "Hosted models. You supply an API key.",
  helperLocal: "Models running on this machine. You supply the endpoint.",
  helperCli: "Reuses a login another command line tool already holds.",
  detailLocal: "Runs on this machine",
  detailCli: "Uses a login another CLI already holds",
} as const;

/**
 * Lowercased authority host of an endpoint URL — scheme, userinfo, port and path
 * dropped. Empty string when no host can be parsed.
 *
 * Mirrors the Rust `endpoint_host`, including its tolerance for a missing scheme
 * and for bracketed IPv6 literals, so both sides classify a stored endpoint
 * identically. A value here may be something an operator typed.
 */
export function endpointHost(endpoint: string | null | undefined): string {
  const trimmed = (endpoint ?? "").trim().toLowerCase();
  if (!trimmed) return "";
  const schemeIdx = trimmed.indexOf("://");
  const afterScheme = schemeIdx === -1 ? trimmed : trimmed.slice(schemeIdx + 3);
  const authority = afterScheme.split(/[/?#]/)[0] ?? "";
  const atIdx = authority.lastIndexOf("@");
  const hostPort = atIdx === -1 ? authority : authority.slice(atIdx + 1);
  if (hostPort.startsWith("[")) {
    const close = hostPort.indexOf("]");
    return close === -1 ? hostPort.slice(1) : hostPort.slice(1, close);
  }
  const colonIdx = hostPort.lastIndexOf(":");
  return (colonIdx === -1 ? hostPort : hostPort.slice(0, colonIdx)).trim();
}

/** The catalogue row for `slug`, if it is a built-in cloud provider. */
export function cloudProvider(slug: string): CloudProvider | undefined {
  return CLOUD_PROVIDERS.find((p) => p.slug === slug);
}

/** The catalogue row for `slug`, if it is a built-in local runtime. */
export function localRuntime(slug: string): LocalRuntime | undefined {
  return LOCAL_RUNTIMES.find((r) => r.slug === slug);
}

/** The catalogue row for `optionSlug`, if it is a CLI login option. */
export function cliLogin(optionSlug: string): CliLogin | undefined {
  return CLI_LOGINS.find((c) => c.optionSlug === optionSlug);
}

/**
 * Whether an endpoint points at an Azure Foundry / Azure OpenAI resource, i.e. a
 * provider whose `model` field must carry a deployment name.
 *
 * Matched on the endpoint **host** rather than the slug, because Azure is
 * reachable only through the custom-provider flow: the operator names the slug
 * (`azure`, `my-azure`, …) and the host is the one stable signal.
 */
export function isAzureEndpoint(endpoint: string | null | undefined): boolean {
  const host = endpointHost(endpoint);
  if (!host) return false;
  return AZURE_ENDPOINT_HOSTS.some((known) => host === known || host.endsWith(`.${known}`));
}

/**
 * Whether `slug` names anything the catalogue ships — cloud, local, or the slug
 * a CLI login stores under.
 *
 * One list with one meaning: a custom provider may not take a name the catalogue
 * already owns. openhuman keeps two lists under this name meaning two different
 * things, which is a trap worth not porting.
 */
export function isReservedSlug(slug: string): boolean {
  const trimmed = slug.trim();
  return (
    cloudProvider(trimmed) !== undefined ||
    localRuntime(trimmed) !== undefined ||
    CLI_LOGINS.some((c) => c.storedSlug === trimmed)
  );
}

/**
 * Which of the three questions a provider answers.
 *
 * A category is a fact about the provider rather than a routing decision, so it
 * lives with the table. It is load-bearing in `routing.ts`: the rule for
 * scrubbing a removed provider out of the routing map differs per category,
 * because only the cloud refs carry a slug.
 *
 * An unknown kind is `cloud` — a custom provider is an OpenAI-compatible
 * endpoint someone pays for, addressed by the slug they named, which is exactly
 * how a cloud row behaves. Mirrors the Rust `category_of`.
 */
export function categoryOf(kind: string): "cloud" | "local" | "cli" {
  const trimmed = kind.trim();
  if (localRuntime(trimmed) !== undefined) return "local";
  // Codex stores under `openai`, and `openai` is a cloud row in its own right,
  // so the shared slug stays cloud. Reading it as a CLI login would give the
  // OpenAI row the slug-less scrub rule and orphan every route naming it.
  if (
    CLI_LOGINS.some(
      (c) => c.optionSlug === trimmed || (c.storedSlug === trimmed && c.storedSlug !== "openai"),
    )
  ) {
    return "cli";
  }
  return "cloud";
}

/**
 * The detail line a row shows under its label.
 *
 * Cloud rows show the endpoint's host rather than the full URL — the path is
 * noise at a glance, and the host is the part an operator recognises.
 */
export function rowDetail(
  category: "cloud" | "local" | "cli",
  endpoint?: string | null,
): string {
  if (category === "local") return COPY.detailLocal;
  if (category === "cli") return COPY.detailCli;
  return endpointHost(endpoint);
}
