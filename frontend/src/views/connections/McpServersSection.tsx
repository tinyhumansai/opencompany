import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  Info,
  KeyRound,
  Loader2,
  LogIn,
  Plug,
  Plus,
  Power,
  PowerOff,
  RefreshCw,
  Server,
  Trash2,
  Unplug,
  Wrench,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  addMcpServer,
  discoverMcpTools,
  listMcpServers,
  type McpAuthKind,
  removeMcpServer,
  startMcpOAuth,
  testMcpServer,
  updateMcpServer,
} from "@/api/mcp";
import {
  connectMcpRegistryServer,
  disconnectMcpRegistryServer,
  getMcpRegistryEntry,
  uninstallMcpRegistryServer,
  updateMcpRegistryEnv,
} from "@/api/mcp-registry";
import {
  ApiError,
  type McpHealth,
  type McpServer,
  type McpSource,
  type McpStatus,
  type McpToolInfo,
} from "@/api/types";
import {
  type McpBridgeState,
  mcpAddedMessage,
  mcpBridgeState,
  mcpHealthBadge,
} from "@/lib/mcp-bridge";
import {
  missingEnvKeys,
  mcpRowControls,
  mcpSourceBadge,
  REGISTRY_UNWIRED_NOTICE,
  registryOutage,
} from "@/lib/mcp-registry";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { McpIconButton } from "@/views/mcp/McpIconButton";
import { McpRegistryBrowser } from "@/views/connections/McpRegistryBrowser";
import { ProviderDetail } from "@/views/connections/ProviderDetail";

/**
 * What a server's health entitles its row to offer (issues #1260, #1270).
 *
 * A rule rather than inline conditions, because the two OAuth states differ by
 * exactly one thing an operator cannot see: whether the server advertises
 * dynamic client registration. `oauth_required` means the server asked for
 * OAuth; only the host knows whether this console can complete one, and it says
 * so by sending `static_token_required` instead. Reading them as the same state
 * is what put a Sign in button on a Slack row that could never sign in.
 *
 * Issue #1270 added a third control and a reason the hint alone cannot decide
 * between them. A **directory install** keeps its credentials as named env
 * values in the host's registry store, and its row's `name` is a display slug
 * addressing no List A declaration — so both List A controls are wrong for it
 * twice over: `POST …/oauth/start` and `PUT …/mcp/servers/{name}` answer *no
 * MCP server named …*, and the value they collect would be written to a store
 * this server is not dialled from. Such a row gets `rotate_env`, which is
 * `PUT …/mcp/registry/{serverId}/env`.
 *
 * `row.status` is consulted only for that case, and it has to be: the host's
 * registry projection emits a stable `authHint` only when the upstream
 * connection reported one, so a directory install refused for want of a
 * credential can arrive as `needs_config` with no hint at all. List A's
 * mapping below is untouched — it is still a function of the hint and nothing
 * else.
 */
export function credentialAffordance(
  authHint: string | undefined,
  row?: { source: McpSource; status?: McpStatus },
): "sign_in" | "add_token" | "rotate_env" | "none" {
  if (row?.source === "registry") {
    return row.status === "needs_config" ? "rotate_env" : "none";
  }
  switch (authHint) {
    case "oauth_required":
      return "sign_in";
    case "static_token_required":
    // A plain credential prompt wants the same field; it simply never had a
    // sign-in button to withdraw.
    case "credential_required":
      return "add_token";
    default:
      return "none";
  }
}

type McpLoad = "loading" | "ready" | "unavailable" | "error";
/**
 * The fields a directory install's credential rotation is asking for.
 *
 * They come from the *directory*, not from the row: `GET …/mcp/servers` reports
 * only whether a credential is stored (`authConfigured`), never which keys hold
 * it, so the names have to be re-read from the catalogue entry the install came
 * from. That is also why this can fail while the rotation route itself is
 * perfectly healthy — a directory outage costs the field names, so the form
 * says so instead of guessing at them.
 */
type EnvFields =
  | { kind: "loading" }
  | { kind: "failed"; message: string }
  | { kind: "ready"; keys: string[] };
type ToolsState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "unwired" }
  | { kind: "error"; message: string }
  | { kind: "ready"; tools: McpToolInfo[] };

/**
 * How the page around this section frames it.
 *
 * - `inline` — a section among others (Connections). The page has plenty else
 *   to show, so a host with no MCP surface renders nothing here at all.
 * - `standalone` — the whole page is this section (Settings, MCP Servers). The
 *   page supplies the heading, and a host with no MCP surface says so, because
 *   the alternative is a page that is simply blank.
 */
export type McpSectionChrome = "inline" | "standalone";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** Whether this viewer may add, edit or remove servers (issue #403). */
  canManage: boolean;
  chrome?: McpSectionChrome;
}

/**
 * Manage the company's MCP tool servers (issue #50). Lists the effective set,
 * adds runtime servers with a **write-only** token field, toggles/removes them,
 * and live-discovers each server's tools. A manifest server can be disabled but
 * not deleted.
 *
 * Since issue #1270 the effective set is four provenances, not two: manifest
 * and default declarations, operator-typed runtime entries, and installs from
 * the upstream MCP directories that [`McpRegistryBrowser`](./McpRegistryBrowser.tsx)
 * below the form adds. They are **one list** with a provenance badge, not two
 * sections — but a row's provenance decides more than its badge, because a
 * directory install has no List A declaration behind its name and so cannot use
 * List A's routes at all. That dispatch is
 * [`mcpRowControls`](@/lib/mcp-registry)'s.
 *
 * This is the console's **only** MCP surface, reached from two places: the
 * Connections page renders it `inline`, Settings, MCP Servers renders it
 * `standalone`. Settings used to carry a second implementation of this screen
 * against an API no host has ever served (`{ servers }` wrappers, `server_id`
 * keys, `/connect` and `/disconnect` routes), which crashed on open — a second
 * surface is how the two came to disagree, so there is one (issue #414).
 */
export function McpServersSection({ client, company, canManage, chrome = "inline" }: Props) {
  const [load, setLoad] = useState<McpLoad>("loading");
  // Whether the agent-side MCP bridge is compiled into this host (issue #567).
  // Starts `unknown` so nothing is claimed before the capability read lands.
  const [bridge, setBridge] = useState<McpBridgeState>("unknown");
  const [servers, setServers] = useState<McpServer[]>([]);
  // The name of the row (or `"add"`) currently mutating. Every mutating handler
  // serialises on it with an `if (busy) return`, so while one is in flight the
  // controls on ALL rows disable, not just the busy one (issue #1475): the guard
  // used to be invisible on the other rows, which accepted clicks and silently
  // did nothing. The active control still shows its own spinner via
  // `busy === server.name`, so the operator can see which row holds the lock.
  const [busy, setBusy] = useState<string | null>(null);
  const [tools, setTools] = useState<Record<string, ToolsState>>({});
  // Live health from an on-demand Test, overriding the persisted badge per row.
  const [tested, setTested] = useState<Record<string, McpHealth>>({});
  // In-flight OAuth sign-in poll timers, keyed by server name. A row with a live
  // timer is still "signing in" even after its `busy` flag clears, so a repeat
  // click can't spawn a second overlapping poll. Cleared on unmount so stale
  // callbacks don't fire against a gone component.
  const pollTimers = useRef<Record<string, number>>({});
  // The name of the server whose detail panel is open, or `null` (issue #821).
  // A name rather than the row itself, so an open panel re-derives from
  // `servers` after a refresh instead of showing the row as it was when clicked.
  const [opened, setOpened] = useState<string | null>(null);
  /**
   * The server whose inline credential field is open, and its draft value
   * (issue #1260).
   *
   * Per-row rather than a shared field: the add form's Token creates a *new*
   * server, so pointing an operator at it to fix an existing one would have
   * them add a second copy. The host has accepted a credential rotation on
   * `PUT …/mcp/servers/{name}` all along — this is the control that was missing.
   */
  const [credentialFor, setCredentialFor] = useState<string | null>(null);
  const [credentialDraft, setCredentialDraft] = useState("");
  /**
   * The registry row whose credential-rotation form is open, and the fields it
   * is showing (issue #1270).
   *
   * Separate state from `credentialFor` because the two collect different
   * things for different stores: List A's is one bearer token written to this
   * company's secret store, a directory install's is a set of *named* env
   * values written to the host's registry store. Which of the two a row offers
   * is `credentialAffordance`'s decision and nothing else's.
   *
   * The field names are not on the row — see `openEnvRotation`.
   */
  const [envFor, setEnvFor] = useState<string | null>(null);
  const [envFields, setEnvFields] = useState<EnvFields>({ kind: "loading" });
  const [envDraft, setEnvDraft] = useState<Record<string, string>>({});
  const [envError, setEnvError] = useState<string | null>(null);
  // Set by the unmount cleanup below. A sign-in poll that is mid-`await` when
  // this component goes away has already removed its own timer entry, so the
  // cleanup has nothing left to cancel — it checks this instead of re-arming.
  const unmounted = useRef(false);
  // Which company's answers are still wanted, bumped whenever the scope
  // changes. `refresh` reads it before asking and again on arrival, and drops
  // the answer if it moved: without this, switching company while the list
  // request is in flight lets the older response resolve last and write one
  // company's servers into another company's view.
  //
  // A generation counter rather than the effect-local `live` flag used
  // elsewhere in this file's siblings, because `refresh` is also called
  // imperatively after every add, toggle, remove and completed sign-in. A flag
  // owned by the mount effect cannot speak for those calls; a counter that only
  // moves on a scope change lets them all through while still fencing the ones
  // that belong to a company the operator has left.
  const scope = useRef(0);

  // Add-server form.
  const [name, setName] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [token, setToken] = useState("");
  const [authKind, setAuthKind] = useState<McpAuthKind>("bearer");
  const [authFieldName, setAuthFieldName] = useState("");
  // The add flow's failure is a PERSISTENT inline alert (not a transient toast):
  // a silent-fail auth error is exactly the bug this cell fixes.
  const [addError, setAddError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    const mine = scope.current;
    try {
      const list = await listMcpServers(client, company);
      if (scope.current !== mine) return;
      setServers(list);
      setLoad("ready");
    } catch (err) {
      if (scope.current !== mine) return;
      // A 404 is a host with no MCP surface: a fact about the build, not a
      // failure. Anything else (offline, 5xx, a body that wasn't the list the
      // route promises) means we do not know what this company has, and saying
      // "no MCP here" would be a claim we cannot make (issue #414).
      setLoad(err instanceof ApiError && err.status === 404 ? "unavailable" : "error");
    }
  }, [client, company]);

  useEffect(() => {
    scope.current += 1;
    setLoad("loading");
    void refresh();
  }, [refresh]);

  // Whether this build can actually run what this screen manages (issue #567).
  // Read separately from the server list, and deliberately not fatal to it: the
  // list is the screen's job, the build state is a caption on it, so a host that
  // cannot answer the capability read still gets a working MCP tab — it just
  // gets no claim about the bridge.
  useEffect(() => {
    let alive = true;
    client
      .capabilityStatus(company)
      .then((status) => {
        if (alive) setBridge(mcpBridgeState(status));
      })
      // A host with no `…/capabilities` surface 404s. Unknown, not absent.
      .catch(() => {
        if (alive) setBridge("unknown");
      });
    return () => {
      alive = false;
    };
  }, [client, company]);

  // Cancel any in-flight sign-in polls when the view unmounts so their timers
  // don't fire against a torn-down component, and tell a poll that is currently
  // between its own `delete` and its next arm that there is nothing to come
  // back to.
  useEffect(() => {
    const timers = pollTimers.current;
    return () => {
      unmounted.current = true;
      for (const id of Object.values(timers)) window.clearTimeout(id);
    };
  }, []);

  async function add() {
    if (busy) return;
    setAddError(null);
    if (!name.trim() || !endpoint.trim()) {
      setAddError("A server needs a name and an https endpoint.");
      return;
    }
    if (authKind !== "bearer" && token.trim() && !authFieldName.trim()) {
      setAddError(
        authKind === "header"
          ? "A custom-header credential needs a header name."
          : "A query-parameter credential needs a parameter name.",
      );
      return;
    }
    setBusy("add");
    try {
      const res = await addMcpServer(client, company, {
        name: name.trim(),
        endpoint: endpoint.trim(),
        token: token.trim() || undefined,
        authKind,
        headerName: authKind === "header" ? authFieldName.trim() || undefined : undefined,
        paramName: authKind === "query_param" ? authFieldName.trim() || undefined : undefined,
      });
      // A probe that lands "needs config" or "error" is NOT a rollback — the
      // server is added — but surface it inline so the operator acts on it.
      // Exception: an OAuth-required result is not an error to shout about — the
      // amber "needs config" badge carries a Sign in button, so a red alert here
      // would be redundant and misleading.
      if (
        res.test &&
        res.test.status !== "ok" &&
        res.test.authHint !== "oauth_required" &&
        res.test.authHint !== "static_token_required"
      ) {
        setAddError(res.test.message);
      } else if (res.warning) {
        setAddError(res.warning);
      } else {
        // The success path has to agree with the banner (issue #567): a toast
        // promising pickup, fired at the moment the operator acts, undoes a
        // statement sitting a few pixels above it.
        toast.success(mcpAddedMessage(name.trim(), bridge));
      }
      setName("");
      setEndpoint("");
      setToken("");
      setAuthFieldName("");
      await refresh();
    } catch (err) {
      // Persistent, not a toast: the operator must see why the add failed.
      setAddError(err instanceof ApiError ? err.message : "Couldn't add the server.");
    } finally {
      setBusy(null);
    }
  }

  async function test(server: McpServer) {
    if (busy) return;
    setBusy(server.name);
    try {
      const health = await testMcpServer(client, company, server.name);
      setTested((t) => ({ ...t, [server.name]: health }));
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message("Live testing isn't enabled in this build (the agent harness is off).");
      } else {
        toast.error(err instanceof ApiError ? err.message : "Couldn't test the server.");
      }
    } finally {
      setBusy(null);
    }
  }

  // Browser OAuth sign-in (issue #90): open the authorization URL in a new tab,
  // then poll the server's health until it flips to `ok` (the host stores the
  // token on its callback route) so the amber badge turns green on its own.
  /**
   * Rotate one server's credential from its own row (issue #1260).
   *
   * Re-tests on success rather than trusting the write: the whole point of the
   * flow is that the operator does not know whether the token is the right one
   * until the server answers, and a silent save would leave the amber badge
   * sitting there with no way to tell "wrong token" from "not saved".
   */
  async function saveCredential(server: McpServer) {
    if (busy) return;
    const token = credentialDraft.trim();
    if (!token) return;
    setBusy(server.name);
    try {
      // Rotate the credential VALUE only — never the auth scheme (issue #1464).
      // The read model carries no `authKind`, so the console cannot know whether
      // this server was added as a bearer token, an `X-Api-Key:` header or a
      // `?api_key=` query parameter. Sending `authKind: "bearer"` here silently
      // rewrote a header/query server to bearer, after which it rejected every
      // request — and the immediate re-test below surfaced that as a bad-token
      // error, sending the operator to re-paste a key that was never the
      // problem. Omitting the field leaves the host's stored scheme untouched.
      await updateMcpServer(client, company, server.name, { token });
      setCredentialFor(null);
      setCredentialDraft("");
      await refresh();
      await test(server);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't save the token.");
    } finally {
      setBusy(null);
    }
  }

  async function signIn(server: McpServer) {
    // Guard both the shared `busy` flag and a per-server poll already in flight:
    // the poll outlives `busy`, so without the second check a repeat click would
    // spawn a second overlapping sign-in (duplicate token exchange + toasts).
    if (busy || pollTimers.current[server.name] !== undefined) return;
    setBusy(server.name);
    try {
      const { authorizeUrl } = await startMcpOAuth(client, company, server.name);
      window.open(authorizeUrl, "_blank", "noopener,noreferrer");
      toast.message(`Complete sign-in for ${server.name} in the new tab.`);
      // Poll for completion for up to ~2 minutes; stop as soon as it's healthy.
      const deadline = Date.now() + 120_000;
      const poll = async () => {
        // The entry goes before the probe, so from here to the arm at the
        // bottom this poll is invisible to the unmount cleanup — which is why
        // every step below re-checks. Without it, an unmount inside the probe
        // leaves the cleanup nothing to cancel, the arm attaches to a
        // torn-down component, and the chain keeps probing and toasting until
        // the two-minute deadline.
        delete pollTimers.current[server.name];
        if (unmounted.current) return;
        if (Date.now() > deadline) {
          toast.message(
            `Sign-in for ${server.name} timed out. Try again if it didn't complete.`,
          );
          return;
        }
        try {
          const health = await testMcpServer(client, company, server.name);
          if (unmounted.current) return;
          setTested((t) => ({ ...t, [server.name]: health }));
          if (health.status === "ok") {
            toast.success(`Signed in to ${server.name}.`);
            await refresh();
            return;
          }
        } catch {
          // Ignore transient probe errors while the operator finishes sign-in.
        }
        if (unmounted.current) return;
        pollTimers.current[server.name] = window.setTimeout(() => void poll(), 2_000);
      };
      pollTimers.current[server.name] = window.setTimeout(() => void poll(), 2_000);
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message("OAuth sign-in isn't enabled in this build (the agent harness is off).");
      } else {
        toast.error(err instanceof ApiError ? err.message : "Couldn't start sign-in.");
      }
    } finally {
      setBusy(null);
    }
  }

  async function toggle(server: McpServer, enabled: boolean) {
    if (busy) return;
    setBusy(server.name);
    try {
      await updateMcpServer(client, company, server.name, { enabled });
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't update the server.");
    } finally {
      setBusy(null);
    }
  }

  /**
   * Remove a server through whichever route owns it (issue #1270).
   *
   * The dispatch is [`mcpRowControls`](@/lib/mcp-registry)'s, not a condition
   * here, because the two routes key on different things and neither accepts
   * the other's key: List A deletes by `name`, a directory install by its
   * `serverId`. A registry row's `name` is a slug the host mints for the merged
   * view — sending it to `DELETE …/mcp/servers/{name}` addresses a declaration
   * that does not exist, and on an unlucky slug collision would address someone
   * else's.
   *
   * The `index` arm covers a *reconciled* runtime row on purpose: the host
   * removes the index row and uninstalls the directory half in the same call,
   * so one delete is the whole removal.
   */
  async function remove(server: McpServer) {
    if (busy) return;
    const removal = mcpRowControls(server, tested[server.name] ?? server.health).removal;
    if (removal.kind === "none") return;
    setBusy(server.name);
    try {
      if (removal.kind === "install") {
        await uninstallMcpRegistryServer(client, company, removal.serverId);
      } else {
        await removeMcpServer(client, company, removal.name);
      }
      toast.success(`Removed ${server.name}.`);
      await refresh();
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message(REGISTRY_UNWIRED_NOTICE);
      } else {
        toast.error(err instanceof ApiError ? err.message : "Couldn't remove the server.");
      }
    } finally {
      setBusy(null);
    }
  }

  /**
   * Dial or drop a directory install's session (issue #1270).
   *
   * The registry's answer to the Switch. A disconnect keeps the install and its
   * stored credentials — it closes the session, it does not uninstall — so the
   * two are separate controls rather than one destructive toggle.
   *
   * The response carries the connection state right after the change, which is
   * recorded as this row's live health so the badge moves without a second
   * round trip. A refused connection is **not** an error: the host says so, and
   * a server that answers "needs a credential" has told the operator exactly
   * what to do next.
   */
  async function lifecycle(server: McpServer, direction: "connect" | "disconnect") {
    if (busy || !server.serverId) return;
    setBusy(server.name);
    try {
      const res =
        direction === "connect"
          ? await connectMcpRegistryServer(client, company, server.serverId)
          : await disconnectMcpRegistryServer(client, company, server.serverId);
      const after = res.test;
      if (after) setTested((t) => ({ ...t, [server.name]: after }));
      // No state came back: drop any stale override so the row falls back to
      // the health the refresh below is about to bring.
      else setTested(({ [server.name]: _dropped, ...rest }) => rest);
      await refresh();
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message(REGISTRY_UNWIRED_NOTICE);
      } else {
        toast.error(
          err instanceof ApiError
            ? err.message
            : `Couldn't ${direction} ${server.name}.`,
        );
      }
    } finally {
      setBusy(null);
    }
  }

  /**
   * Open a directory install's credential rotation, reading its field names
   * from the catalogue entry it was installed from.
   *
   * The row cannot supply them: the merged read reports only that *a*
   * credential is stored. See {@link EnvFields}.
   */
  async function openEnvRotation(server: McpServer) {
    setEnvDraft({});
    setEnvError(null);
    setEnvFor(server.name);
    if (!server.qualifiedName) {
      setEnvFields({
        kind: "failed",
        message: "This install doesn't name a directory entry, so its credential fields are unknown.",
      });
      return;
    }
    setEnvFields({ kind: "loading" });
    try {
      const detail = await getMcpRegistryEntry(client, company, server.qualifiedName);
      setEnvFields({ kind: "ready", keys: detail.requiredEnvKeys });
    } catch (err) {
      const outage = registryOutage(err);
      setEnvFields({
        kind: "failed",
        message:
          outage.kind === "unwired"
            ? REGISTRY_UNWIRED_NOTICE
            : `${outage.message} Its credential fields are named in the directory, so they can't be read right now.`,
      });
    }
  }

  /**
   * Rotate a directory install's credentials.
   *
   * Write-only in both directions, exactly like List A's token: the values go
   * to `PUT …/mcp/registry/{serverId}/env` and come back only as the row's
   * `authConfigured` flag. The host merges the supplied keys over the stored
   * ones and reconnects, so the post-write connection state is the answer to
   * "was that the right credential" and is recorded as this row's live health.
   */
  async function saveEnvRotation(server: McpServer, keys: string[]) {
    if (busy || !server.serverId) return;
    const missing = missingEnvKeys(keys, envDraft);
    if (missing.length > 0) {
      setEnvError(`Fill in ${missing.join(", ")} before saving.`);
      return;
    }
    setBusy(server.name);
    setEnvError(null);
    try {
      const res = await updateMcpRegistryEnv(client, company, server.serverId, envDraft);
      const after = res.test;
      if (after) setTested((t) => ({ ...t, [server.name]: after }));
      setEnvFor(null);
      setEnvDraft({});
      await refresh();
    } catch (err) {
      setEnvError(err instanceof ApiError ? err.message : "Couldn't save those credentials.");
    } finally {
      setBusy(null);
    }
  }

  async function discover(server: McpServer) {
    // Toggle closed if already shown.
    if (tools[server.name]?.kind === "ready") {
      setTools((t) => ({ ...t, [server.name]: { kind: "idle" } }));
      return;
    }
    setTools((t) => ({ ...t, [server.name]: { kind: "loading" } }));
    try {
      const list = await discoverMcpTools(client, company, server.name);
      setTools((t) => ({ ...t, [server.name]: { kind: "ready", tools: list } }));
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        setTools((t) => ({ ...t, [server.name]: { kind: "unwired" } }));
      } else {
        setTools((t) => ({
          ...t,
          [server.name]: {
            kind: "error",
            message: err instanceof ApiError ? err.message : "Discovery failed.",
          },
        }));
      }
    }
  }

  // Re-derived from the list every render rather than captured on click, so the
  // open panel reflects the last refresh — a toggle, a completed sign-in or a
  // removal all reach it without a second copy of the row to keep in step.
  const openedServer = useMemo(
    () => servers.find((s) => s.name === opened) ?? null,
    [servers, opened],
  );

  if (load === "unavailable") {
    if (chrome === "inline") return null;
    return (
      <Alert data-testid="mcp-unavailable">
        <Info className="size-4" />
        <AlertTitle>MCP servers aren&apos;t wired on this host</AlertTitle>
        <AlertDescription>
          This host serves no MCP routes, so there is nothing to manage here yet.
        </AlertDescription>
      </Alert>
    );
  }

  return (
    <section className="space-y-3">
      {/* `h2` in both chromes, and it lands one level under the page's `h1`
          either way (issue #1392). Inline on Connections it is a peer of
          the company key, Composio and providers, which all head their
          sections at the same level —
          `test/unit/connections-section-heading-level.test.ts` holds them
          together, because promoting this one alone would read to a screen
          reader as though every section after it were a subsection of MCP
          Servers. Standalone on `#/settings/mcp` the page's own `h1` is "MCP
          Servers", so this names what it actually heads there instead of
          repeating it. */}
      <div className="flex items-center gap-2">
        {chrome === "inline" && <Server className="size-4 text-muted-foreground" />}
        <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          {chrome === "inline" ? "MCP Servers" : "Installed servers"}
        </h2>
      </div>
      <p className="text-sm text-muted-foreground">
        Remote MCP tool servers your teammates can call. Add an HTTP endpoint and (optionally) a
        token — the token is stored securely and never shown again.
      </p>

      {/* Issue #567: this screen's routes ship in every build, the agent-side
          bridge does not. Said before the list rather than per row, because it
          is a fact about the deployment and not about any one server — and said
          only on an explicit `false`, never on a host that stayed silent. */}
      {bridge === "absent" && (
        <Alert data-testid="mcp-bridge-absent">
          <AlertTriangle className="size-4" />
          <AlertTitle>No teammate can use tool servers in this deployment</AlertTitle>
          <AlertDescription>
            The MCP bridge isn&apos;t compiled into this build, so servers added here are stored and
            can be probed, but no teammate ever receives their tools. The configuration survives —
            rebuild this deployment with the <code className="font-mono">mcp</code> feature and the
            servers below start reaching teammates on the next turn.
          </AlertDescription>
        </Alert>
      )}

      {load === "error" ? (
        // Not an empty list: an empty list is a company with no tool servers,
        // and this host did not tell us that (issue #414).
        <Alert variant="destructive" data-testid="mcp-load-error">
          <AlertTriangle className="size-4" />
          <AlertTitle>Couldn&apos;t load this company&apos;s MCP servers</AlertTitle>
          <AlertDescription>
            The host didn&apos;t answer with its server list, so what is installed is unknown.
            Reload to try again.
          </AlertDescription>
        </Alert>
      ) : load === "loading" ? (
        <Skeleton className="h-24 rounded-xl" />
      ) : (
        <Card>
          <CardContent className="space-y-3">
            {servers.length === 0 ? (
              <p className="text-sm text-muted-foreground">No MCP servers yet.</p>
            ) : (
              <ul className="divide-y divide-border">
                {servers.map((server) => {
                  const health = tested[server.name] ?? server.health;
                  // Which half of the API this row may talk to at all — not
                  // merely which badge it prints (issue #1270). A directory
                  // install has no List A declaration behind its name, so the
                  // Switch, Test and Tools that every row used to carry answer
                  // `no MCP server named …` on it, and the registry's own
                  // connect/disconnect stand in their place.
                  const controls = mcpRowControls(server, health);
                  // Hoisted out of the JSX so the direction stays narrowed:
                  // there is one place that decides connect-or-disconnect, and
                  // the button below reads it rather than re-testing it.
                  const dial = controls.lifecycle === "none" ? null : controls.lifecycle;
                  const badge = mcpSourceBadge(server.source);
                  const credential = credentialAffordance(health?.authHint, {
                    source: server.source,
                    status: health?.status,
                  });
                  return (
                    <li
                      key={server.name}
                      data-testid="mcp-server-row"
                      className="space-y-2 py-3 first:pt-0 last:pb-0"
                    >
                      {/* Claude-style row: an identity on the left, the
                          controls as icons on the right (issue: the MCP page
                          redesign). Labelled buttons pushed a row six controls
                          deep onto two lines, and the second line was always
                          the one carrying the endpoint — the row's own
                          information lost to its chrome. Every icon keeps its
                          sentence in a tooltip and in its accessible name; see
                          `McpIconButton`. */}
                      <div className="flex items-start gap-3">
                        <span className="mt-0.5 flex size-8 shrink-0 items-center justify-center overflow-hidden rounded-md border border-border bg-muted/40">
                          {server.iconUrl ? (
                            // The directory's own icon when the row came from
                            // one; a decorative image, so it is named by the
                            // row beside it rather than by alt text repeating
                            // the server name a screen reader just read.
                            <img src={server.iconUrl} alt="" className="size-full object-cover" />
                          ) : (
                            <Server className="size-4 text-muted-foreground" />
                          )}
                        </span>
                        <div className="min-w-0 flex-1 space-y-1">
                          <div className="flex flex-wrap items-center gap-2">
                            {/* The row's handle on its own detail view (issue #821).
                                A button on the name rather than a trailing "Open":
                                the row's right edge is already several controls
                                deep, and the name is what an operator points at
                                when they want to know what a server is.

                                The chevron is not decoration. Hover styling alone
                                makes a name that opens something indistinguishable
                                from one that does not until the pointer is already
                                on it — which on a touch screen is never, and for
                                anyone scanning the page is a control that does not
                                exist. */}
                            <button
                              type="button"
                              data-testid="mcp-server-open"
                              className="inline-flex cursor-pointer items-center gap-0.5 rounded-sm font-medium underline-offset-4 hover:underline focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
                              onClick={() => setOpened(server.name)}
                              aria-label={`Open ${server.name}`}
                            >
                              {server.name}
                              <ChevronRight className="size-3.5 text-muted-foreground" />
                            </button>
                            <Badge variant={badge.variant} data-testid="mcp-source-badge">
                              {badge.label}
                            </Badge>
                            <McpHealthBadge
                              health={health}
                              authConfigured={server.authConfigured}
                              bridge={bridge}
                            />
                            {/* The enable switch became an icon with the rest of
                                the controls, and an icon in an off state reads
                                as "press me to turn it off" as readily as the
                                reverse. A disabled server therefore says so in
                                words, where the other statuses are. */}
                            {controls.toggle && !server.enabled && (
                              <Badge variant="outline" data-testid="mcp-disabled-badge">
                                disabled
                              </Badge>
                            )}
                          </div>
                          <p className="truncate text-xs text-muted-foreground">{server.endpoint}</p>
                        </div>
                        <span className="flex shrink-0 items-center gap-0.5">
                          {/* Issue #1260: `oauth_required` means the server asked for
                              OAuth; it does NOT mean this console can complete one. A
                              server advertising no dynamic client registration — Slack's
                              MCP endpoint, for one — has no client for us to mint, so
                              Sign in cannot succeed and the host says so with a distinct
                              hint. Offering the button anyway spends a click to reach an
                              error naming something the operator cannot act on, which is
                              the same trade `hub_providers` already refuses to make for
                              the Google and GitHub buttons. */}
                          {credential === "add_token" &&
                            canManage &&
                            credentialFor !== server.name && (
                              <McpIconButton
                                label={
                                  server.authConfigured
                                    ? `Replace ${server.name}'s API token`
                                    : `Add an API token for ${server.name}`
                                }
                                icon={KeyRound}
                                tone="primary"
                                testId="mcp-add-token"
                                disabled={busy !== null}
                                onClick={() => {
                                  setCredentialDraft("");
                                  setCredentialFor(server.name);
                                }}
                              />
                            )}
                          {credential === "sign_in" && canManage && (
                            <McpIconButton
                              label={`Sign in to ${server.name}`}
                              icon={LogIn}
                              tone="primary"
                              testId="mcp-sign-in"
                              busy={busy === server.name}
                              disabled={busy !== null}
                              onClick={() => void signIn(server)}
                            />
                          )}
                          {/* Issue #1270: a directory install's credentials are
                              named env values in the host's registry store, so
                              the control it gets is a rotation of those, not
                              List A's single bearer-token field. */}
                          {credential === "rotate_env" &&
                            canManage &&
                            envFor !== server.name && (
                              <McpIconButton
                                label={`Set ${server.name}'s credentials`}
                                icon={KeyRound}
                                tone="primary"
                                testId="mcp-rotate-env"
                                disabled={busy !== null}
                                onClick={() => void openEnvRotation(server)}
                              />
                            )}
                          {dial !== null && canManage && (
                            <McpIconButton
                              label={
                                dial === "connect"
                                  ? `Connect ${server.name}`
                                  : `Disconnect ${server.name}`
                              }
                              icon={dial === "connect" ? Plug : Unplug}
                              testId="mcp-lifecycle"
                              busy={busy === server.name}
                              disabled={busy !== null}
                              onClick={() => void lifecycle(server, dial)}
                            />
                          )}
                          {/* Test and Tools resolve the row's `name` against
                              List A's declarations, which a directory install
                              has none of — offering them there would spend a
                              click to reach `no MCP server named …`. Connect
                              above is that row's equivalent: its answer carries
                              the connection state and the tool count. */}
                          {controls.probe && (
                            <>
                              <McpIconButton
                                label={`Re-check ${server.name}`}
                                icon={RefreshCw}
                                testId="mcp-test"
                                busy={busy === server.name}
                                disabled={busy !== null}
                                onClick={() => void test(server)}
                              />
                              <McpIconButton
                                label={
                                  tools[server.name]?.kind === "ready"
                                    ? `Hide ${server.name}'s tools`
                                    : `List ${server.name}'s tools`
                                }
                                icon={Wrench}
                                testId="mcp-tools"
                                disabled={busy !== null}
                                onClick={() => void discover(server)}
                              />
                            </>
                          )}
                          {controls.toggle && canManage && (
                            <McpIconButton
                              label={
                                server.enabled
                                  ? `Disable ${server.name}`
                                  : `Enable ${server.name}`
                              }
                              icon={server.enabled ? Power : PowerOff}
                              testId="mcp-toggle"
                              busy={busy === server.name}
                              disabled={busy !== null}
                              onClick={() => void toggle(server, !server.enabled)}
                            />
                          )}
                          {controls.removal.kind !== "none" && canManage && (
                            <McpIconButton
                              label={`Remove ${server.name}`}
                              icon={Trash2}
                              tone="destructive"
                              testId="mcp-remove"
                              disabled={busy !== null}
                              onClick={() => void remove(server)}
                            />
                          )}
                        </span>
                      </div>
                      {/* Reachability (issue #568): who can actually call this server. An
                          enabled server no agent reaches is almost always a misconfiguration,
                          so that empty case is flagged loudly rather than shown as a blank list.
                          A disabled server is empty by construction — the harness hands out no
                          tool for it whatever the grants say — so the loud state is scoped to
                          enabled servers; flagging an off server would cry wolf on intent.

                          Suppressed entirely when the bridge is absent (issue #1467): the
                          reachability walk knows nothing about whether the `mcp` feature is
                          compiled in, so with no bridge the red "no tool grant covers …, widen a
                          grant" advice misdiagnoses the cause — grants cannot fix a missing
                          bridge — and the positive "Reachable by: …" line contradicts the banner
                          that says no teammate receives these tools. The banner already carries
                          the real message here. */}
                      {bridge !== "absent" &&
                        server.reachableBy !== undefined &&
                        server.enabled &&
                        (server.reachableBy.length === 0 ? (
                          <p
                            data-testid="mcp-reachability-none"
                            className="flex items-start gap-1.5 rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1 text-xs font-medium text-destructive"
                          >
                            <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
                            <span>
                              No teammate can reach this server — no tool grant covers{" "}
                              <code className="font-mono">mcp:{server.name}</code>. Widen a company or
                              per-teammate tool grant, or this server is unused.
                            </span>
                          </p>
                        ) : (
                          <p data-testid="mcp-reachability" className="text-xs text-muted-foreground">
                            Reachable by:{" "}
                            <span className="font-medium text-foreground">
                              {/* Names, not ids (issue #931): an operator-added teammate's
                                  id is a minted internal string and tells the reader
                                  nothing about who can reach the server. */}
                              {server.reachableBy.map((agent) => agent.name).join(", ")}
                            </span>
                          </p>
                        ))}
                      {health && health.status !== "ok" && health.message && (
                        <p className="text-xs text-muted-foreground">{health.message}</p>
                      )}
                      {credentialFor === server.name && canManage && (
                        <div className="flex items-end gap-2" data-testid="mcp-token-inline">
                          <div className="flex-1 space-y-1">
                            <Label htmlFor={`mcp-token-${server.name}`} className="text-xs">
                              API token for {server.name}
                              {/* The value is write-only and unrecoverable, so
                                  say when saving it overwrites an existing one
                                  (issue #1464). */}
                              {server.authConfigured ? " — replaces the stored credential" : ""}
                            </Label>
                            <Input
                              id={`mcp-token-${server.name}`}
                              type="password"
                              autoComplete="new-password"
                              placeholder="write-only"
                              value={credentialDraft}
                              onChange={(e) => setCredentialDraft(e.target.value)}
                              onKeyDown={(e) => {
                                if (e.key === "Enter") void saveCredential(server);
                                if (e.key === "Escape") setCredentialFor(null);
                              }}
                            />
                          </div>
                          <Button
                            size="sm"
                            data-testid="mcp-token-save"
                            disabled={busy !== null || !credentialDraft.trim()}
                            onClick={() => void saveCredential(server)}
                          >
                            {busy === server.name ? (
                              <Loader2 className="size-4 animate-spin" />
                            ) : (
                              "Save"
                            )}
                          </Button>
                          <Button
                            size="sm"
                            variant="ghost"
                            disabled={busy !== null}
                            onClick={() => setCredentialFor(null)}
                          >
                            Cancel
                          </Button>
                        </div>
                      )}
                      {envFor === server.name && canManage && (
                        <div className="space-y-2 rounded-md bg-muted/40 p-2" data-testid="mcp-env-inline">
                          <p className="text-xs text-muted-foreground">
                            Saving merges these values with the stored credentials and reconnects this server.
                          </p>
                          {envFields.kind === "loading" ? (
                            <p className="flex items-center gap-1 text-xs text-muted-foreground">
                              <Loader2 className="size-3 animate-spin" /> Reading this
                              server&apos;s credential fields…
                            </p>
                          ) : envFields.kind === "failed" ? (
                            <p className="text-xs text-destructive" data-testid="mcp-env-unavailable">
                              {envFields.message}
                            </p>
                          ) : envFields.keys.length === 0 ? (
                            <p className="text-xs text-muted-foreground">
                              This server asks for no credentials.
                            </p>
                          ) : (
                            envFields.keys.map((key) => (
                              <div key={key} className="space-y-1">
                                <Label
                                  htmlFor={`mcp-env-${server.name}-${key}`}
                                  className="font-mono text-xs"
                                >
                                  {key}
                                </Label>
                                <Input
                                  id={`mcp-env-${server.name}-${key}`}
                                  type="password"
                                  autoComplete="new-password"
                                  placeholder="write-only"
                                  value={envDraft[key] ?? ""}
                                  onChange={(e) =>
                                    setEnvDraft({ ...envDraft, [key]: e.target.value })
                                  }
                                />
                              </div>
                            ))
                          )}
                          {envError && <p className="text-xs text-destructive">{envError}</p>}
                          <div className="flex items-center gap-2">
                            {envFields.kind === "ready" && envFields.keys.length > 0 && (
                              <Button
                                size="sm"
                                data-testid="mcp-env-save"
                                disabled={busy !== null}
                                onClick={() => void saveEnvRotation(server, envFields.keys)}
                              >
                                {busy === server.name ? (
                                  <Loader2 className="size-4 animate-spin" />
                                ) : (
                                  "Save"
                                )}
                              </Button>
                            )}
                            <Button
                              size="sm"
                              variant="ghost"
                              disabled={busy !== null}
                              onClick={() => setEnvFor(null)}
                            >
                              {envFields.kind === "ready" && envFields.keys.length > 0
                                ? "Cancel"
                                : "Close"}
                            </Button>
                          </div>
                        </div>
                      )}
                      <McpToolsList state={tools[server.name] ?? { kind: "idle" }} />
                    </li>
                  );
                })}
              </ul>
            )}

            {/* Adding a server hands the agents a new set of tools, so the
                whole form goes for a member rather than leaving fields that
                cannot be submitted (issue #403). Test and Tools above stay:
                they probe a server an admin already added, and the host
                deliberately leaves those open. */}
            {canManage && (
              <div className="space-y-2 border-t border-border pt-3">
                {addError && (
                  <Alert variant="destructive">
                    <AlertTriangle className="size-4" />
                    <AlertTitle>Couldn&apos;t add the server</AlertTitle>
                    <AlertDescription>{addError}</AlertDescription>
                  </Alert>
                )}
                <div className="grid gap-2 sm:grid-cols-2 sm:items-end">
                  <div className="space-y-1">
                    <Label htmlFor="mcp-name" className="text-xs">
                      Name
                    </Label>
                    <Input
                      id="mcp-name"
                      data-testid="mcp-add-name"
                      value={name}
                      placeholder="notion"
                      onChange={(e) => setName(e.target.value)}
                    />
                  </div>
                  <div className="space-y-1">
                    <Label htmlFor="mcp-endpoint" className="text-xs">
                      Endpoint
                    </Label>
                    <Input
                      id="mcp-endpoint"
                      name="mcp-endpoint-url"
                      data-testid="mcp-add-endpoint"
                      value={endpoint}
                      placeholder="https://host/mcp"
                      autoComplete="url"
                      onChange={(e) => setEndpoint(e.target.value)}
                    />
                  </div>
                </div>
                <div className="grid gap-2 sm:grid-cols-[auto_1fr_1fr_auto] sm:items-end">
                  <div className="space-y-1">
                    <Label htmlFor="mcp-auth-kind" className="text-xs">
                      Auth
                    </Label>
                    <select
                      id="mcp-auth-kind"
                      value={authKind}
                      onChange={(e) => setAuthKind(e.target.value as McpAuthKind)}
                      className="h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm shadow-xs"
                    >
                      <option value="bearer">Bearer token</option>
                      <option value="header">Custom header</option>
                      <option value="query_param">Query parameter</option>
                    </select>
                  </div>
                  {authKind !== "bearer" && (
                    <div className="space-y-1">
                      <Label htmlFor="mcp-auth-field" className="text-xs">
                        {authKind === "header" ? "Header name" : "Parameter name"}
                      </Label>
                      <Input
                        id="mcp-auth-field"
                        value={authFieldName}
                        placeholder={authKind === "header" ? "X-Api-Key" : "apiKey"}
                        autoComplete="off"
                        onChange={(e) => setAuthFieldName(e.target.value)}
                      />
                    </div>
                  )}
                  <div className="space-y-1">
                    <Label htmlFor="mcp-token" className="text-xs">
                      {authKind === "bearer" ? "Token (optional)" : "Credential value"}
                    </Label>
                    <Input
                      id="mcp-token"
                      name="mcp-token-secret"
                      type="password"
                      value={token}
                      placeholder="write-only"
                      autoComplete="new-password"
                      onChange={(e) => setToken(e.target.value)}
                    />
                  </div>
                  <Button
                    data-testid="mcp-add-submit"
                    disabled={busy !== null}
                    onClick={() => void add()}
                  >
                    {busy === "add" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <Plus className="size-4" />
                    )}
                    Add
                  </Button>
                </div>
                <p className="text-xs text-muted-foreground">
                  Adding saves the server now. {bridge === "absent"
                    ? "It stays unavailable to teammates until this deployment is rebuilt with MCP support."
                    : "Teammates pick up its tools on their next turn."}
                </p>
              </div>
            )}

            {/* Issue #1270: the tab could not discover anything — an operator
                had to arrive already knowing a URL to paste. The directory
                browser sits inside the same card, under the same manage gate as
                the form above it (an install hands every teammate a new set of
                tools), and what it installs lands in the list above with a
                `registry` badge rather than in a section of its own.

                Its failures are its own: it renders a notice inside itself and
                never touches `load`, so a directory that is down — or a host
                built without the MCP feature, which 404s these routes — costs
                the catalogue and not the company's server list. */}
            {canManage && (
              <McpRegistryBrowser
                client={client}
                company={company}
                onInstalled={() => void refresh()}
              />
            )}
          </CardContent>
        </Card>
      )}

      {/* A server as an object you open, in the same panel a Composio provider
          opens into (issue #821). Rendered from here rather than from the page
          above, because the live health an operator just pressed Test for lives
          in this component's state — and because this section is also the whole
          of Settings, MCP Servers, which gets the detail view for free.

          `openedServer` is re-derived from `servers` every render, so a removed
          server closes its own panel rather than leaving a page describing
          something that is gone. */}
      <ProviderDetail
        client={client}
        company={company}
        subject={
          openedServer === null
            ? null
            : {
                kind: "mcp",
                server: openedServer,
                // The live Test result when there has been one this session,
                // else the server's own persisted probe — the same precedence
                // the row's badge uses, so the panel and the row it opened from
                // cannot report different health.
                health: tested[openedServer.name] ?? openedServer.health,
              }
        }
        canManage={canManage}
        busy={busy !== null}
        onClose={() => setOpened(null)}
      />
    </section>
  );
}

/**
 * The per-server health badge: green `ok · N tools`, amber `needs config`, red
 * `error`. Falls back to a plain "auth set" hint when the server has never been
 * probed (no `health`).
 *
 * Bridge state is folded in the way `HostingView` folds `inBuild` (issue #1405).
 * On a build with no MCP bridge the probe still answers for real — the server is
 * reachable — but nothing it reports is delivered to a teammate, and the banner
 * above the list says exactly that. A green `ok` under that banner tells the
 * operator twelve tools are working when no teammate can call one, so on
 * `bridge === "absent"` every affirmative reading drops out of the success
 * colour into the neutral register: reachable and configured, but not
 * delivering. `needs config` / `error` are genuine problems either way and keep
 * their amber/red.
 */
function McpHealthBadge({
  health,
  authConfigured,
  bridge,
}: {
  health?: McpHealth;
  authConfigured: boolean;
  bridge: McpBridgeState;
}) {
  // The decision — which register, and the text — is the pure `mcpHealthBadge`
  // decider (issue #1405), so the bridge fold is unit-tested and this component
  // only maps a tone to its token colour and icon.
  const badge = mcpHealthBadge(health, authConfigured, bridge);
  if (!badge) return null;
  const tone =
    badge.tone === "delivering"
      ? { className: "text-status-done-text", Icon: Check }
      : badge.tone === "configured"
        ? { className: "text-muted-foreground", Icon: Info }
        : badge.tone === "warn"
          ? { className: "text-status-blocked-text", Icon: AlertTriangle }
          : { className: "text-destructive", Icon: AlertTriangle };
  return (
    <span className={`inline-flex items-center gap-1 text-xs ${tone.className}`}>
      <tone.Icon className="size-3" /> {badge.label}
    </span>
  );
}

/** Renders the live-discovered tool list for one server. */
function McpToolsList({ state }: { state: ToolsState }) {
  if (state.kind === "idle") return null;
  if (state.kind === "loading") {
    return (
      <p className="flex items-center gap-1 text-xs text-muted-foreground">
        <Loader2 className="size-3 animate-spin" /> Discovering tools…
      </p>
    );
  }
  if (state.kind === "unwired") {
    return (
      <p className="text-xs text-muted-foreground">
        Live tool discovery isn&apos;t enabled in this build (the agent harness is off).
      </p>
    );
  }
  if (state.kind === "error") {
    return <p className="text-xs text-destructive">{state.message}</p>;
  }
  if (state.tools.length === 0) {
    return <p className="text-xs text-muted-foreground">This server exposed no tools.</p>;
  }
  return (
    <ul className="space-y-1 rounded-md bg-muted/40 p-2">
      {state.tools.map((tool) => (
        <li key={tool.name} className="text-xs">
          <span className="font-mono font-medium">{tool.name}</span>
          {tool.description ? (
            <span className="text-muted-foreground"> — {tool.description}</span>
          ) : null}
        </li>
      ))}
    </ul>
  );
}
