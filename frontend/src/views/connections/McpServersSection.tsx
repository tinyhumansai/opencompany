import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { openInNewTab, openOutward } from "@/lib/external-links";
import {
  AlertTriangle,
  FileJson,
  Info,
  Plus,
  Search,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  discoverMcpTools,
  listMcpServers,
  removeMcpServer,
  startMcpOAuth,
  testMcpServer,
  updateMcpServer,
} from "@/api/mcp";
import {
  connectMcpRegistryServer,
  disconnectMcpRegistryServer,
  getMcpRegistryEntry,
  installMcpRegistryEntry,
  uninstallMcpRegistryServer,
  updateMcpRegistryEnv,
  type McpCatalogueEntry,
} from "@/api/mcp-registry";
import {
  ApiError,
  type McpHealth,
  type McpServer,
  type McpSource,
  type McpStatus,
  type RosterAgent,
} from "@/api/types";
import { type McpBridgeState, mcpBridgeState } from "@/lib/mcp-bridge";
import {
  mcpDisplayName,
  missingEnvKeys,
  mcpRowControls,
  REGISTRY_UNWIRED_NOTICE,
  registryOutage,
} from "@/lib/mcp-registry";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { useHashParam } from "@/hooks/use-hash-param";
import { McpDiscover } from "@/views/connections/McpRegistryBrowser";
import { McpAddServerDialog } from "@/views/connections/McpAddServerDialog";
import {
  McpServerCard,
  McpServerGrid,
  McpServerRow,
  McpServerTable,
  type McpRowActions,
  type PrimaryAction,
} from "@/views/connections/McpServerTable";
import {
  McpLayoutSwitch,
  McpModeSwitch,
  useMcpLayouts,
  type McpMode,
} from "@/views/connections/mcp-view-controls";
import {
  McpConnectDialog,
  type EnvFields,
  type SignInFlight,
  type ToolsState,
} from "@/views/mcp/McpConnectDialog";
import { McpJsonEditor } from "@/views/mcp/McpJsonEditor";
import { McpServerPage } from "@/views/mcp/McpServerPage";
import { type SignInWatch, watchSignIn } from "@/lib/mcp-sign-in-watch";

/** The toast a finished browser sign-in raises. */
export function signedInMessage(
  server: Pick<McpServer, "name" | "probedTitle">,
  health: Pick<McpHealth, "toolCount">,
): string {
  const n = health.toolCount;
  return `Connected to ${mcpDisplayName(server)} · ${n} tool${n === 1 ? "" : "s"}`;
}

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
 * credential can arrive as `needs_config` with no hint at all. The hint still
 * has the last word on a registry row: `oauth_required` names a credential the
 * env form neither collects nor stores, and no route here can start that
 * sign-in, so such a row is offered no credential control at all. List A's
 * mapping below is untouched — it is still a function of the hint and nothing
 * else.
 */
export function credentialAffordance(
  authHint: string | undefined,
  row?: { source: McpSource; status?: McpStatus },
): "sign_in" | "add_token" | "rotate_env" | "none" {
  if (row?.source === "registry") {
    if (authHint === "oauth_required") return "none";
    return row.status === "needs_config" ? "rotate_env" : "none";
  }
  switch (authHint) {
    case "oauth_required":
      return "sign_in";
    case "static_token_required":
    // A plain credential prompt wants the same field; it simply never had a
    // sign-in button to withdraw.
    case "credential_required":
    // A refused credential is replaced through that same field.
    case "token_rejected":
      return "add_token";
    default:
      return "none";
  }
}

type McpLoad = "loading" | "ready" | "unavailable" | "error";

/**
 * How the page around this section frames it.
 *
 * - `inline` — a section among others. The page has plenty else to show, so a
 *   host with no MCP surface renders nothing here at all. The default, and
 *   since the Connections split no page embeds it.
 * - `standalone` — the whole page is this section (`#/connections/mcp`). The
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
  /** The roster, for the per-teammate lens on a server's permissions. */
  agents?: RosterAgent[];
}

/**
 * The company's MCP tool servers (Yours) and the public directory (Discover),
 * each searchable on its own and shown as a list or as cards.
 *
 * This is the console's **only** MCP surface, and it has exactly one caller:
 * [`McpServersView`](../McpServersView.tsx), the `#/connections/mcp` page.
 */
export function McpServersSection({
  client,
  company,
  canManage,
  chrome = "inline",
  agents = [],
}: Props) {
  const [load, setLoad] = useState<McpLoad>("loading");
  // Whether the agent-side MCP bridge is compiled into this host (issue #567).
  // Starts `unknown` so nothing is claimed before the capability read lands.
  const [bridge, setBridge] = useState<McpBridgeState>("unknown");
  // Whether a `needs_approval` mode parks anything on this host. `undefined`
  // until the capability read answers, and left that way when it cannot: the
  // notice claims nothing on a host that has not said.
  const [approvalsPark, setApprovalsPark] = useState<boolean | undefined>(
    undefined,
  );
  const [servers, setServers] = useState<McpServer[]>([]);
  // The name of the row currently mutating. Every mutating handler serialises on
  // it with an `if (busy) return`, so while one is in flight the controls on ALL
  // rows disable, not just the busy one.
  const [busy, setBusy] = useState<string | null>(null);
  const [tools, setTools] = useState<Record<string, ToolsState>>({});
  // Live health from an on-demand re-check, overriding the persisted badge.
  const [tested, setTested] = useState<Record<string, McpHealth>>({});
  const watches = useRef<Record<string, SignInWatch>>({});
  const [signIns, setSignIns] = useState<Record<string, SignInFlight>>({});
  // Opens the detail panel on the permissions section. Kept as its own key so
  // links already written against it keep landing where they meant to.
  const [permissionsFor, setPermissionsFor] = useHashParam("permissions");
  // The server whose detail page is open. A name rather than the row itself, so
  // an open page re-derives from `servers` after a refresh.
  const [openedName, setOpenedName] = useHashParam("server");
  const opened = openedName ?? permissionsFor;
  const closeDetail = () => {
    setOpenedName(null);
    setPermissionsFor(null);
  };
  const [viewParam, setViewParam] = useHashParam("view");
  const [tabParam, setTabParam] = useHashParam("tab");
  const [yoursQuery, setYoursQuery] = useState("");
  const [discoverQuery, setDiscoverQuery] = useState("");
  const [layouts, chooseLayout] = useMcpLayouts();
  // The server whose connect dialog is open.
  const [connectFor, setConnectFor] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [installing, setInstalling] = useState<string | null>(null);

  /**
   * The server whose credential field is open, and its draft value.
   *
   * Per-row rather than a shared field: the add dialog's Token creates a *new*
   * server, so pointing an operator at it to fix an existing one would have them
   * add a second copy.
   */
  const [credentialFor, setCredentialFor] = useState<string | null>(null);
  const [credentialDraft, setCredentialDraft] = useState("");
  /**
   * The registry row whose credential-rotation form is open, and the fields it
   * is showing (issue #1270).
   *
   * Separate state from `credentialFor` because the two collect different things
   * for different stores: List A's is one bearer token written to this company's
   * secret store, a directory install's is a set of *named* env values written to
   * the host's registry store. Which of the two a row offers is
   * `credentialAffordance`'s decision and nothing else's.
   */
  const [envFor, setEnvFor] = useState<string | null>(null);
  const [envFields, setEnvFields] = useState<EnvFields>({ kind: "loading" });
  const [envDraft, setEnvDraft] = useState<Record<string, string>>({});
  const [envError, setEnvError] = useState<string | null>(null);
  // Which company's answers are still wanted, bumped whenever the scope changes.
  // `refresh` reads it before asking and again on arrival, and drops the answer
  // if it moved: without this, switching company while the list request is in
  // flight lets the older response resolve last and write one company's servers
  // into another company's view.
  const scope = useRef(0);
  // Removal is irreversible and takes the server's stored credential with it, so
  // it is asked rather than done on the press.
  const [pendingRemoval, setPendingRemoval] = useState<McpServer | null>(null);
  // Bumped per server whenever a probe rewrote its stored tool inventory, so an
  // open permissions panel re-reads instead of rendering the pre-probe list.
  const [probedAt, setProbedAt] = useState<Record<string, number>>({});

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
      setLoad(
        err instanceof ApiError && err.status === 404 ? "unavailable" : "error",
      );
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
        if (alive) {
          setBridge(mcpBridgeState(status));
          setApprovalsPark(
            typeof status.approvalsPark === "boolean"
              ? status.approvalsPark
              : undefined,
          );
        }
      })
      // A host with no `…/capabilities` surface 404s. Unknown, not absent.
      .catch(() => {
        if (alive) {
          setBridge("unknown");
          setApprovalsPark(undefined);
        }
      });
    return () => {
      alive = false;
    };
  }, [client, company]);

  useEffect(() => {
    const live = watches.current;
    const recheck = () => {
      if (document.visibilityState === "hidden") return;
      for (const watch of Object.values(live)) watch.checkNow();
    };
    window.addEventListener("focus", recheck);
    document.addEventListener("visibilitychange", recheck);
    return () => {
      window.removeEventListener("focus", recheck);
      document.removeEventListener("visibilitychange", recheck);
      for (const [name, watch] of Object.entries(live)) {
        watch.stop();
        delete live[name];
      }
    };
  }, []);

  async function test(server: McpServer) {
    if (busy) return;
    setBusy(server.name);
    try {
      const health = await testMcpServer(client, company, server.name);
      setTested((t) => ({ ...t, [server.name]: health }));
      setProbedAt((p) => ({ ...p, [server.name]: Date.now() }));
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message(
          "Live testing isn't enabled in this build (the agent harness is off).",
        );
      } else {
        toast.error(
          err instanceof ApiError ? err.message : "Couldn't test the server.",
        );
      }
    } finally {
      setBusy(null);
    }
  }

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
      // request. Omitting the field leaves the host's stored scheme untouched.
      await updateMcpServer(client, company, server.name, { token });
      setCredentialFor(null);
      setCredentialDraft("");
      await refresh();
      await test(server);
    } catch (err) {
      toast.error(
        err instanceof ApiError ? err.message : "Couldn't save the token.",
      );
    } finally {
      setBusy(null);
    }
  }

  /** Stop watching for a sign-in the operator has given up on. */
  function cancelSignIn(name: string) {
    watches.current[name]?.stop();
    delete watches.current[name];
    setSignIns(({ [name]: _dropped, ...rest }) => rest);
  }

  function patchFlight(name: string, patch: Partial<SignInFlight>) {
    setSignIns((s) => {
      const flight = s[name];
      return flight ? { ...s, [name]: { ...flight, ...patch } } : s;
    });
  }

  /**
   * Browser OAuth sign-in: open the authorization page in a new tab, then watch
   * the server's health until it reports `ok`. The host stores the token on its
   * callback route, which tells this tab nothing, so the watch is the only
   * signal.
   */
  async function signIn(server: McpServer) {
    const name = server.name;
    setConnectFor(name);
    const live = watches.current[name];
    if (busy || (live && !live.stopped && !live.timedOut)) return;
    live?.stop();
    delete watches.current[name];
    setBusy(name);
    try {
      const { authorizeUrl } = await startMcpOAuth(client, company, name);
      let opened = openOutward(authorizeUrl);
      if (!opened) {
        opened = openInNewTab(authorizeUrl);
      }
      setSignIns((s) => ({
        ...s,
        [name]: {
          authorizeUrl,
          blocked: !opened,
          startedAtMillis: Date.now(),
          checkedAtMillis: null,
          timedOut: false,
        },
      }));
      const watch: SignInWatch = watchSignIn({
        probe: () => testMcpServer(client, company, name),
        onProbe: (health) => {
          if (watches.current[name] !== watch) return;
          setTested((t) => ({ ...t, [name]: health }));
          patchFlight(name, { checkedAtMillis: Date.now() });
        },
        onConnected: (health) => {
          if (watches.current[name] !== watch) return;
          delete watches.current[name];
          setSignIns(({ [name]: _dropped, ...rest }) => rest);
          toast.success(signedInMessage(server, health));
          void refresh();
        },
        onTimeout: () => {
          if (watches.current[name] !== watch) return;
          patchFlight(name, { timedOut: true });
        },
      });
      watches.current[name] = watch;
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message(
          "OAuth sign-in isn't enabled in this build (the agent harness is off).",
        );
      } else {
        toast.error(
          err instanceof ApiError ? err.message : "Couldn't start sign-in.",
        );
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
      toast.error(
        err instanceof ApiError ? err.message : "Couldn't update the server.",
      );
    } finally {
      setBusy(null);
    }
  }

  /**
   * Remove a server through whichever route owns it (issue #1270).
   *
   * The dispatch is [`mcpRowControls`](@/lib/mcp-registry)'s, not a condition
   * here, because the two routes key on different things and neither accepts the
   * other's key: List A deletes by `name`, a directory install by its
   * `serverId`. A registry row's `name` is a slug the host mints for the merged
   * view — sending it to `DELETE …/mcp/servers/{name}` addresses a declaration
   * that does not exist, and on an unlucky slug collision would address someone
   * else's.
   */
  async function remove(server: McpServer) {
    if (busy) return;
    const removal = mcpRowControls(
      server,
      tested[server.name] ?? server.health,
    ).removal;
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
        toast.error(
          err instanceof ApiError ? err.message : "Couldn't remove the server.",
        );
      }
    } finally {
      setBusy(null);
    }
  }

  /**
   * Dial or drop a directory install's session (issue #1270).
   *
   * A disconnect keeps the install and its stored credentials — it closes the
   * session, it does not uninstall — so the two are separate controls rather
   * than one destructive toggle. A refused connection is **not** an error: the
   * host says so, and a server that answers "needs a credential" has told the
   * operator exactly what to do next.
   */
  async function lifecycle(
    server: McpServer,
    direction: "connect" | "disconnect",
  ) {
    if (busy || !server.serverId) return;
    setBusy(server.name);
    try {
      const res =
        direction === "connect"
          ? await connectMcpRegistryServer(client, company, server.serverId)
          : await disconnectMcpRegistryServer(client, company, server.serverId);
      const after = res.test;
      if (after) setTested((t) => ({ ...t, [server.name]: after }));
      else setTested(({ [server.name]: _dropped, ...rest }) => rest);
      await refresh();
      if (direction === "connect") setConnectFor(server.name);
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
   * Install a directory entry, with no credential.
   *
   * Every tool it exposes starts un-granted either way, and a credential the
   * entry needs is collected on the row the install lands as — which is the one
   * control that writes to the store that install is actually dialled from.
   */
  async function install(entry: McpCatalogueEntry): Promise<boolean> {
    if (installing) return false;
    setInstalling(entry.qualifiedName);
    try {
      const res = await installMcpRegistryEntry(client, company, {
        qualifiedName: entry.qualifiedName,
      });
      const after = res.test;
      if (after) setTested((t) => ({ ...t, [res.server.name]: after }));
      await refresh();
      setConnectFor(res.server.name);
      return true;
    } catch (err) {
      const outage = registryOutage(err);
      toast.error(
        outage.kind === "unwired" ? REGISTRY_UNWIRED_NOTICE : outage.message,
      );
      return false;
    } finally {
      setInstalling(null);
    }
  }

  /**
   * Open a directory install's credential rotation, reading its field names from
   * the catalogue entry it was installed from.
   *
   * The row cannot supply them: the merged read reports only that *a* credential
   * is stored. See {@link EnvFields}.
   */
  async function openEnvRotation(server: McpServer) {
    setEnvDraft({});
    setEnvError(null);
    setEnvFor(server.name);
    if (!server.qualifiedName) {
      setEnvFields({
        kind: "failed",
        message:
          "This install doesn't name a directory entry, so its credential fields are unknown.",
      });
      return;
    }
    setEnvFields({ kind: "loading" });
    try {
      const detail = await getMcpRegistryEntry(
        client,
        company,
        server.qualifiedName,
      );
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
   * Write-only in both directions, exactly like List A's token: the values go to
   * `PUT …/mcp/registry/{serverId}/env` and come back only as the row's
   * `authConfigured` flag. The host merges the supplied keys over the stored ones
   * and reconnects, so the post-write connection state is the answer to "was that
   * the right credential" and is recorded as this row's live health.
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
      const res = await updateMcpRegistryEnv(
        client,
        company,
        server.serverId,
        envDraft,
      );
      const after = res.test;
      if (after) setTested((t) => ({ ...t, [server.name]: after }));
      if (after && after.status !== "ok") {
        setEnvError(
          after.message.trim() ||
            `${server.name} still isn't connected with those credentials.`,
        );
        await refresh();
        return;
      }
      setEnvFor(null);
      setEnvDraft({});
      await refresh();
    } catch (err) {
      setEnvError(
        err instanceof ApiError
          ? err.message
          : "Couldn't save those credentials.",
      );
    } finally {
      setBusy(null);
    }
  }

  async function discover(server: McpServer) {
    setTools((t) => ({ ...t, [server.name]: { kind: "loading" } }));
    try {
      const list = await discoverMcpTools(client, company, server.name);
      setTools((t) => ({
        ...t,
        [server.name]: { kind: "ready", tools: list },
      }));
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        setTools((t) => ({ ...t, [server.name]: { kind: "unwired" } }));
      } else {
        setTools((t) => ({
          ...t,
          [server.name]: {
            kind: "error",
            message:
              err instanceof ApiError ? err.message : "Discovery failed.",
          },
        }));
      }
    }
  }

  const actions: McpRowActions = {
    onOpen: (name) => setOpenedName(name),
    onSignIn: (server) => void signIn(server),
    onAddToken: (server) => {
      setCredentialDraft("");
      setCredentialFor(server.name);
      setConnectFor(server.name);
    },
    onRotateEnv: (server) => {
      setConnectFor(server.name);
      void openEnvRotation(server);
    },
    onLifecycle: (server, direction) => void lifecycle(server, direction),
    onTest: (server) => void test(server),
    onTools: (server) => {
      setConnectFor(server.name);
      void discover(server);
    },
    onPermissions: (name) => setPermissionsFor(name),
    onToggle: (server, enabled) => void toggle(server, enabled),
    onRemove: (server) => setPendingRemoval(server),
  };

  // Re-derived from the list every render rather than captured on click, so the
  // open dialog reflects the last refresh — a toggle, a completed sign-in or a
  // removal all reach it without a second copy of the row to keep in step.
  const removalDialog = (
    <AlertDialog
      open={pendingRemoval !== null}
      onOpenChange={(open) => !open && setPendingRemoval(null)}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Remove {pendingRemoval?.name}?</AlertDialogTitle>
          <AlertDialogDescription>
            Its agents stop seeing this server&apos;s tools on their next turn,
            and the stored credential goes with it — a token is never shown
            again, so adding the server back means pasting a new one.
          </AlertDialogDescription>
          <AlertDialogDescription>
            Its per-tool permissions are removed too.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={busy !== null}>Keep it</AlertDialogCancel>
          <AlertDialogAction
            disabled={busy !== null}
            onClick={() => {
              const server = pendingRemoval;
              setPendingRemoval(null);
              if (server) void remove(server);
            }}
          >
            Remove
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );

  const openedServer = useMemo(
    () => servers.find((s) => s.name === opened) ?? null,
    [servers, opened],
  );

  const mode: McpMode =
    viewParam === "discover"
      ? "discover"
      : viewParam === "yours"
        ? "yours"
        : load === "ready" && servers.length === 0
          ? "discover"
          : "yours";
  const layout = layouts[mode];

  const term = yoursQuery.trim().toLowerCase();
  const matches = useMemo(
    () =>
      term === ""
        ? servers
        : servers.filter((s) =>
            [s.name, s.description ?? "", s.probedTitle ?? "", s.endpoint].some(
              (field) => field.toLowerCase().includes(term),
            ),
          ),
    [servers, term],
  );

  function primaryFor(server: McpServer, health: McpHealth | undefined): PrimaryAction {
    const credential = credentialAffordance(health?.authHint, {
      source: server.source,
      status: health?.status,
    });
    if (credential === "sign_in") return { kind: "sign_in" };
    if (credential === "add_token") return { kind: "add_token" };
    if (credential === "rotate_env") return { kind: "rotate_env" };
    return mcpRowControls(server, health).lifecycle === "connect"
      ? { kind: "connect" }
      : null;
  }

  function runPrimary(server: McpServer, primary: PrimaryAction) {
    if (primary === null) return;
    if (primary.kind === "sign_in") actions.onSignIn(server);
    else if (primary.kind === "add_token") actions.onAddToken(server);
    else if (primary.kind === "rotate_env") actions.onRotateEnv(server);
    else actions.onLifecycle(server, "connect");
  }

  function closeConnect() {
    const name = connectFor;
    setConnectFor(null);
    if (name === null) return;
    if (credentialFor === name) setCredentialFor(null);
    if (envFor === name) setEnvFor(null);
    setTools(({ [name]: _dropped, ...rest }) => rest);
  }

  const connecting = useMemo(
    () => servers.find((s) => s.name === connectFor) ?? null,
    [servers, connectFor],
  );
  const connectingHealth = connecting
    ? (tested[connecting.name] ?? connecting.health)
    : undefined;

  const connectDialog = (
    <McpConnectDialog
      server={connecting}
      health={connectingHealth}
      bridge={bridge}
      canManage={canManage}
      busy={busy}
      primary={connecting ? primaryFor(connecting, connectingHealth) : null}
      flight={connecting ? signIns[connecting.name] : undefined}
      token={{
        open: connecting !== null && credentialFor === connecting.name,
        draft: credentialDraft,
        onDraft: setCredentialDraft,
        onSave: () => connecting && void saveCredential(connecting),
        onCancel: () => setCredentialFor(null),
      }}
      env={{
        open: connecting !== null && envFor === connecting.name,
        fields: envFields,
        draft: envDraft,
        error: envError,
        onDraft: setEnvDraft,
        onSave: (keys) => connecting && void saveEnvRotation(connecting, keys),
        onCancel: () => setEnvFor(null),
      }}
      tools={connecting ? (tools[connecting.name] ?? { kind: "idle" }) : { kind: "idle" }}
      onPrimary={() =>
        connecting && runPrimary(connecting, primaryFor(connecting, connectingHealth))
      }
      onCancelSignIn={() => connecting && cancelSignIn(connecting.name)}
      onCheckSignIn={() => connecting && watches.current[connecting.name]?.checkNow()}
      onRetrySignIn={() => connecting && void signIn(connecting)}
      onOpenServer={() => {
        const name = connectFor;
        closeConnect();
        if (name) setOpenedName(name);
      }}
      onClose={closeConnect}
    />
  );

  const jsonDialog = (
    <Dialog
      open={tabParam === "json"}
      onOpenChange={(open) => !open && setTabParam(null)}
    >
      <DialogContent className="sm:max-w-3xl" data-testid="mcp-json-dialog">
        <DialogHeader>
          <DialogTitle>mcp.json</DialogTitle>
          <DialogDescription>
            Every server this company declares, as one document.
          </DialogDescription>
        </DialogHeader>
        <McpJsonEditor
          client={client}
          company={company}
          canManage={canManage}
          onSaved={() => void refresh()}
        />
      </DialogContent>
    </Dialog>
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

  if (openedServer !== null) {
    const health = tested[openedServer.name] ?? openedServer.health;
    const primary = primaryFor(openedServer, health);
    return (
      <>
        <McpServerPage
          client={client}
          company={company}
          server={openedServer}
          health={health}
          canManage={canManage}
          bridge={bridge}
          approvalsPark={approvalsPark}
          agents={agents}
          reloadKey={probedAt[openedServer.name] ?? 0}
          focusPermissions={openedName === null && permissionsFor !== null}
          primary={primary}
          busy={busy}
          onPrimary={() => runPrimary(openedServer, primary)}
          onDisconnect={
            mcpRowControls(openedServer, health).removal.kind === "none"
              ? null
              : () => setPendingRemoval(openedServer)
          }
          onBack={closeDetail}
          onAccessSaved={() => void refresh()}
        />
        {removalDialog}
        {connectDialog}
      </>
    );
  }

  const ready = load === "ready";
  const query = mode === "yours" ? yoursQuery : discoverQuery;
  const setQuery = mode === "yours" ? setYoursQuery : setDiscoverQuery;

  return (
    <section className="space-y-4">
      <h2 className="sr-only">
        {mode === "yours" ? "Your servers" : "Discover servers"}
      </h2>
      <div className="flex flex-wrap items-center gap-2">
        <McpModeSwitch mode={mode} onChange={setViewParam} />
        <div className="relative order-last min-w-0 basis-full sm:order-none sm:basis-auto sm:flex-1">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            key={mode}
            value={query}
            disabled={mode === "yours" && !ready}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={
              mode === "yours" ? "Search your servers" : "Search the directory"
            }
            aria-label={
              mode === "yours" ? "Search your servers" : "Search the directory"
            }
            data-testid={mode === "yours" ? "mcp-search" : "mcp-discover-search"}
            className="h-8 pl-8"
          />
        </div>
        <span className="flex-1 sm:hidden" />
        <McpLayoutSwitch
          layout={layout}
          onChange={(next) => chooseLayout(mode, next)}
        />
        {ready && (
          <Button
            size="sm"
            variant="outline"
            data-testid="mcp-json-open"
            title={canManage ? "Edit mcp.json" : "View mcp.json"}
            onClick={() => setTabParam("json")}
          >
            <FileJson className="size-4" />
            <span className="hidden sm:inline">mcp.json</span>
          </Button>
        )}
        {canManage && ready && (
          <Button size="sm" data-testid="mcp-add-open" onClick={() => setAdding(true)}>
            <Plus className="size-4" />
            Add custom server
          </Button>
        )}
      </div>

      {bridge === "absent" && (
        <Alert data-testid="mcp-bridge-absent">
          <AlertTriangle className="size-4" />
          <AlertTitle>No agent can use tool servers in this deployment</AlertTitle>
          <AlertDescription>
            The MCP bridge isn&apos;t compiled into this build, so servers added
            here are stored and can be probed, but no agent ever receives their
            tools. Rebuild with the <code className="font-mono">mcp</code>{" "}
            feature and they start reaching agents on the next turn.
          </AlertDescription>
        </Alert>
      )}

      {load === "error" ? (
        <Alert variant="destructive" data-testid="mcp-load-error">
          <AlertTriangle className="size-4" />
          <AlertTitle>Couldn&apos;t load this company&apos;s MCP servers</AlertTitle>
          <AlertDescription>
            The host didn&apos;t answer with its server list, so what is
            installed is unknown. Reload to try again.
          </AlertDescription>
        </Alert>
      ) : load === "loading" ? (
        <Skeleton className="h-24 rounded-xl" />
      ) : mode === "discover" ? (
        <McpDiscover
          client={client}
          company={company}
          query={discoverQuery}
          layout={layout}
          servers={servers}
          installing={installing}
          canManage={canManage}
          onInstall={install}
        />
      ) : servers.length === 0 ? (
        <Card>
          <CardContent className="space-y-2">
            <p className="text-sm font-medium">No tool servers yet</p>
            <p className="text-sm text-muted-foreground">
              An MCP server gives your agents tools they do not have natively — a
              Notion workspace, a Linear board, an internal database.
            </p>
            {canManage && (
              <div className="flex flex-wrap items-center gap-2 pt-1">
                <Button
                  size="sm"
                  data-testid="mcp-browse-directory"
                  onClick={() => setViewParam("discover")}
                >
                  <Search className="size-4" />
                  Browse the directory
                </Button>
                <Button size="sm" variant="outline" onClick={() => setAdding(true)}>
                  <Plus className="size-4" />
                  Add custom server
                </Button>
              </div>
            )}
          </CardContent>
        </Card>
      ) : matches.length === 0 ? (
        <Card data-testid="mcp-search-nothing">
          <CardContent className="space-y-2">
            <p className="text-sm text-muted-foreground">
              None of your servers match{" "}
              <strong className="font-medium text-foreground">
                {yoursQuery.trim()}
              </strong>
              .
            </p>
            <Button
              size="sm"
              variant="outline"
              data-testid="mcp-search-directory"
              onClick={() => {
                setDiscoverQuery(yoursQuery);
                setViewParam("discover");
              }}
            >
              <Search className="size-4" />
              Search the directory for it
            </Button>
          </CardContent>
        </Card>
      ) : (
        <>
          {layout === "cards" ? (
            <McpServerGrid>
              {matches.map((server) => {
                const health = tested[server.name] ?? server.health;
                return (
                  <McpServerCard
                    key={server.name}
                    server={server}
                    health={health}
                    bridge={bridge}
                    canManage={canManage}
                    busy={busy}
                    primary={primaryFor(server, health)}
                    signingIn={signIns[server.name] !== undefined}
                    actions={actions}
                  />
                );
              })}
            </McpServerGrid>
          ) : (
            <McpServerTable>
              {matches.map((server) => {
                const health = tested[server.name] ?? server.health;
                return (
                  <McpServerRow
                    key={server.name}
                    server={server}
                    health={health}
                    bridge={bridge}
                    canManage={canManage}
                    busy={busy}
                    primary={primaryFor(server, health)}
                    signingIn={signIns[server.name] !== undefined}
                    actions={actions}
                  />
                );
              })}
            </McpServerTable>
          )}
          <p className="text-xs text-muted-foreground" data-testid="mcp-tally">
            {servers.length} server{servers.length === 1 ? "" : "s"} ·{" "}
            {servers.filter((s) => s.enabled).length} on.{" "}
            {bridge === "absent"
              ? "None of them reaches an agent in this build."
              : "Agents pick up a change on their next turn."}
          </p>
        </>
      )}

      {removalDialog}
      {connectDialog}
      {jsonDialog}

      <McpAddServerDialog
        client={client}
        company={company}
        open={adding}
        bridge={bridge}
        onOpenChange={setAdding}
        onAdded={() => void refresh()}
        onConnect={(name, health) => {
          if (health) setTested((t) => ({ ...t, [name]: health }));
          setConnectFor(name);
        }}
        onOpenServer={(name) => setOpenedName(name)}
      />
    </section>
  );
}
