// The hosts this console is connected to, and which one is on screen.
//
// ## The rule this context exists to keep
//
// Host selection is a **filter over N live things**, not a selector that makes
// one live. Choosing a host changes which console is on screen and nothing
// else: no connection is torn down, no stream is closed, no storage is
// re-scoped.
//
// That is the difference between this and block/buzz's workspace rail, which
// looks the same and is not. Switching there is a stateful *apply* — it
// re-scopes the retention database, re-resolves identity and restarts the
// managed agents — because its app state holds one `relay_url_override`. The
// switcher is the visible part; the singleton behind it is why buzz cannot hold
// two workspaces at once.
//
// So: selection lives in `App`'s local state, never in the registry, and no
// code path here mutates a connection. This context only *carries* that state
// down to the switcher, which now lives in the sidebar header — two layers
// below `App`, inside a console that a host being unreachable must not be able
// to take down (issue #1142).

import { createContext, useContext, useEffect, useState, type ReactNode } from "react";

import type { LocalInstance } from "@/api/transport/desktop";
import type { HostEdit } from "@/connections/registry";
import type { Connection, ConnectionId, Connector, SshTarget } from "@/connections/types";

export interface HostsValue {
  connections: Connection[];
  /** The connection whose console is on screen. */
  selected: ConnectionId | null;
  /** Puts another host's console on screen. A filter — see the note above. */
  onSelect: (id: ConnectionId) => void;
  /**
   * Registers a host reachable at `baseUrl`, and opens it.
   *
   * The connector says which of the four this is, and is not derivable from
   * the address: a cloud tenant and a gateway someone runs are both
   * `https://…`, and only the first is worth waiting for when it does not
   * answer. See `docs/spec/runtime/connectors.md`.
   */
  onAdd: (baseUrl: string, connector?: Connector) => void;
  /**
   * The hosts this machine runs, running or not.
   *
   * Empty in a browser, which runs none, and on a shell predating the roster —
   * where the "on this computer" half simply does not draw.
   */
  localInstances: LocalInstance[];
  /** Creates a host on this machine over a data root of its own, and starts it. */
  onAddLocal?: (label: string) => Promise<void>;
  /**
   * Renames the local host whose console is on screen, if it is one.
   *
   * Exists so finishing setup can put the *company's* name on the host that
   * now holds it. A second company on this machine means a second data root,
   * which the operator meets as "add a host" and has to name before they have
   * been asked a single question about the company — so the name they type is
   * a placeholder for a thing they were not thinking about, and the roster ends
   * up listing hosts nobody can tell apart. Naming it after the company turns
   * that into a detail they never have to hold.
   *
   * A no-op on a remote host and in the browser: neither is this machine's to
   * rename. Absent entirely on a shell with no local roster.
   */
  onNameLocalHost?: (label: string) => Promise<void>;
  onStartLocal?: (id: string) => Promise<void>;
  onStopLocal?: (id: string) => Promise<void>;
  /** Permanently deletes a non-default local host and its data. */
  onDeleteLocal?: (id: string) => Promise<void>;
  /**
   * Opens a tunnel to a host on another machine and registers it.
   *
   * Absent in a browser, which cannot start a process — and absent on a shell
   * built before tunnels existed, so the tab is offered only where the button
   * behind it can be honoured.
   *
   * Rejects with what `ssh` said, which the form shows: a refused key names
   * a specific thing to go and fix.
   */
  onAddSsh?: (target: SshTarget) => Promise<void>;
  /**
   * Renames a host, or points it at a different address.
   *
   * The other half of "add": a host that moved, or one whose name came from
   * its URL and reads as nothing, was until now only fixable by forgetting it
   * and adding it again — which mints a new connection id and orphans every
   * `scopedKey` written under the old one. Editing keeps the id.
   */
  onEditHost: (id: ConnectionId, change: HostEdit) => void;
  /**
   * Forgets a host, and whatever this client remembered about it.
   *
   * Local to this client: the host itself is untouched, and a tunnel opened
   * for it is closed. See `removeConnection`.
   */
  onRemoveHost: (id: ConnectionId) => void;
  /** Whether this is a hub deployment, which offers the switcher at any count. */
  hub: boolean;
}

/**
 * The roster plus the one piece of UI state that must not live in the switcher.
 *
 * "Add a host" opens a screen (`views/setup/AddHostPage.tsx`), and creating a
 * host on this computer *selects* it — which remounts the console the switcher
 * is drawn inside, taking that screen with it and closing the flow halfway
 * through. The rail did not have this problem because it stood outside the
 * console; keeping the open flag here, and the screen mounted beside the
 * console rather than within it, is how the switcher keeps that property from
 * its new home.
 */
export interface HostsContextValue extends HostsValue {
  /** Whether the "Add a host" screen is up. */
  addingHost: boolean;
  setAddingHost: (open: boolean) => void;
  /**
   * Whether the "Manage hosts" page is on screen.
   *
   * Here for the same reason `addingHost` is, and more sharply: the page's
   * whole job is editing and removing rows, and removing the row that is on
   * screen selects another host — which remounts the console. A flag owned by
   * anything inside it would take the page away mid-edit.
   */
  managingHosts: boolean;
  setManagingHosts: (open: boolean) => void;
}

const HostsContext = createContext<HostsContextValue | null>(null);

/**
 * The hosts, for anything below `App` that needs them.
 *
 * Throws rather than defaulting: a switcher rendered outside the provider would
 * silently show an empty roster, which reads as "you have no hosts" rather than
 * as the wiring mistake it is.
 */
export function useHosts(): HostsContextValue {
  const value = useContext(HostsContext);
  if (!value) throw new Error("useHosts must be used within a HostsProvider.");
  return value;
}

/**
 * The hosts, for anything that works with or without them.
 *
 * The sibling of {@link useHosts}, and the difference is whether the caller is
 * *about* the roster. A switcher outside the provider is a wiring mistake and
 * should say so; a view that merely takes an extra step when it happens to be
 * inside a console — the setup wizard naming its host after the company — is
 * not, and it renders on its own in tests and wherever a console has not been
 * assembled yet.
 */
export function useOptionalHosts(): HostsContextValue | null {
  return useContext(HostsContext);
}

/** How many hosts the number row can reach. `⌘1`–`⌘9`, in list order. */
export const HOST_SHORTCUT_LIMIT = 9;

/** Whether this keyboard spells the shortcut with ⌘ rather than Ctrl. */
export function isAppleKeyboard(): boolean {
  return /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
}

/** How the shortcut for the host at `index` reads on this keyboard. */
export function hostShortcutLabel(index: number): string | null {
  if (index >= HOST_SHORTCUT_LIMIT) return null;
  return isAppleKeyboard() ? `⌘${index + 1}` : `Ctrl+${index + 1}`;
}

export function HostsProvider({ value, children }: { value: HostsValue; children: ReactNode }) {
  const { connections, onSelect } = value;
  const [addingHost, setAddingHost] = useState(false);
  const [managingHosts, setManagingHosts] = useState(false);

  // `⌘1`–`⌘9` selects the host in that position. Installed here rather than on
  // the switcher so it works in every phase — including the ones where the
  // sidebar is not mounted because the selected host is unreachable.
  //
  // **Only swallowed when a host is actually there.** With two hosts connected,
  // `⌘3` is left alone and the browser keeps its own tab shortcut; taking a key
  // to do nothing is worse than not taking it. `event.key` rather than
  // `event.code`, so a layout that puts the digits elsewhere still matches what
  // the menu prints.
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (!(event.metaKey || event.ctrlKey) || event.altKey || event.shiftKey) return;
      const position = Number(event.key);
      if (!Number.isInteger(position) || position < 1 || position > HOST_SHORTCUT_LIMIT) return;
      const host = connections[position - 1];
      if (!host) return;
      event.preventDefault();
      onSelect(host.id);
    }

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [connections, onSelect]);

  return (
    <HostsContext.Provider
      value={{ ...value, addingHost, setAddingHost, managingHosts, setManagingHosts }}
    >
      {children}
    </HostsContext.Provider>
  );
}
