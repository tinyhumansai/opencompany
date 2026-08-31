// The console's ambient `(client, company)` — who to ask, and about which
// company — for the handful of leaf components that need to fetch something
// and are too deep, and too numerous, to thread props to.
//
// **Not a general escape from props.** Nearly every view in this console takes
// its client and company as props, deliberately: it makes the data flow legible
// and keeps a component honest about what it reaches for. This exists for one
// shape that defeats that rule — a leaf drawn dozens of times per screen, from
// a dozen different parents, that needs an authenticated fetch. Today that is
// exactly one component: `TeammateAvatar`, which has to fetch an uploaded face
// through the client because an `<img>` cannot carry a credential.

import { createContext, useContext, useMemo, type ReactNode } from "react";

import type { OpenCompanyClient } from "@/api/client";

interface ConsoleScope {
  /**
   * The client for the host on screen, or `undefined` outside a provider.
   *
   * Optional rather than required so a consumer degrades instead of throwing:
   * a component rendered outside the shell — a styleguide page, a test — draws
   * what it can without a client rather than taking the tree down.
   */
  client?: OpenCompanyClient;
  company: string | null;
}

const ConsoleContext = createContext<ConsoleScope>({ company: null });

export function ConsoleProvider({
  client,
  company,
  children,
}: ConsoleScope & { children: ReactNode }) {
  // Memoised on the two values rather than rebuilt each render: the value is an
  // object, so an unmemoised one re-renders every consumer on every render of
  // the shell — which for an avatar means re-running its resolve effect.
  const value = useMemo(() => ({ client, company }), [client, company]);
  return <ConsoleContext.Provider value={value}>{children}</ConsoleContext.Provider>;
}

/** The host and company the surrounding console is pointed at. */
export function useConsole(): ConsoleScope {
  return useContext(ConsoleContext);
}
