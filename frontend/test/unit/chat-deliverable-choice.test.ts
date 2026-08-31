// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { OpenCompanyClient } from "@/api/client";
import type { MessageIntent } from "@/api/tasks";
import { MessageComposer } from "@/views/chat/MessageComposer";
import type { ChannelKind } from "@/views/chat/model";
import { offersDeliverableChoice } from "@/views/chat/model";

/**
 * Issue #845: which composers offer "Do it once" / "Build me the workflow".
 *
 * #580 shipped the control on channel composers only. Nothing downstream was
 * ever scoped to channels — `client.chat` carries `deliverable` off the payload
 * whatever thread it came from, and the chat route reads it the same way — so
 * the asymmetry lived entirely in the caller. A DM asking for a workflow had no
 * way to say so: it went as a `once` card, was dispatched to a desk agent
 * holding no workflow-authoring tool, and came back as a refusal. Reported
 * verbatim on staging as "The only workflow tools I have are read-only".
 */
describe("offersDeliverableChoice", () => {
  it("offers the choice on a DM — the gap this closes", () => {
    expect(offersDeliverableChoice("dm")).toBe(true);
  });

  it("still offers it on a channel, unchanged from #580", () => {
    expect(offersDeliverableChoice("channel")).toBe(true);
  });

  /**
   * The rule is total over `ChannelKind` rather than an inline
   * `kind === "channel"`, so a new kind is a decision someone makes in the
   * function instead of a control that silently fails to appear. This pins that
   * every kind the type admits has an answer.
   */
  it("answers for every channel kind", () => {
    const kinds: ChannelKind[] = ["channel", "dm"];
    for (const kind of kinds) {
      expect(typeof offersDeliverableChoice(kind)).toBe("boolean");
    }
  });
});

/**
 * A client whose transport records the body of every request it is handed.
 *
 * The rule under test is what the console *omits*, and that is only observable
 * on the wire — a fake `client.chat` would be asserting against a second copy of
 * the rule rather than against the one that ships.
 */
function recordingClient() {
  const bodies: Array<Record<string, unknown>> = [];
  const transport = {
    request: async (req: { method: string; url: string; body?: string }) => {
      bodies.push(req.body === undefined ? {} : JSON.parse(req.body));
      return {
        status: 200,
        statusText: "OK",
        url: req.url,
        text: JSON.stringify({ responses: [] }),
        header: () => null,
      };
    },
    subscribe: () => () => {},
  };
  const client = new OpenCompanyClient(
    { baseUrl: "", company: "acme", operatorToken: "t0ken", sessionHeader: null },
    transport as never,
  );
  return { client, bodies };
}

async function chatBody(intent?: MessageIntent) {
  const { client, bodies } = recordingClient();
  await client.chat("morning all", "acme", null, null, intent);
  return bodies[0];
}

/**
 * Issue #1152: the wire rule for the composer's third position.
 *
 * The operator could already override the classifier upward — "Build me the
 * workflow" mints a card it declined — and had no way to override it downward.
 * `"chat"` is that control, and these pin the only thing the console decides
 * about it: which values reach the body.
 *
 * The omission half is the compatibility guarantee. Both "Do it once" and no
 * selection send nothing, so an unmarked message posts a body byte-identical to
 * one from a console that predates every one of these controls. The host can
 * therefore apply its normal triage without having to distinguish client ages.
 */
describe("client.chat — what the composer's choice puts on the wire", () => {
  it("sends the new chat intent explicitly, under the existing key", async () => {
    expect(await chatBody("chat")).toEqual({ text: "morning all", deliverable: "chat" });
  });

  it("omits the key entirely for `once` — the default is silence, not a value", async () => {
    expect(await chatBody("once")).not.toHaveProperty("deliverable");
  });

  it("omits the key entirely when no choice was expressed", async () => {
    expect(await chatBody(undefined)).toEqual({ text: "morning all" });
  });

  it("still sends `workflow`, unchanged from #580", async () => {
    expect(await chatBody("workflow")).toEqual({ text: "morning all", deliverable: "workflow" });
  });

  /**
   * One key, not two. A `{deliverable, intent}` pair would let a body claim
   * "build me the workflow" and "just chatting" about the same message — the
   * split brain #1035 closed, pointed the other way.
   */
  it("never puts a second intent field beside the deliverable", async () => {
    expect(await chatBody("chat")).not.toHaveProperty("intent");
  });
});

let container: HTMLDivElement;
let root: Root;
let sent: ReturnType<typeof vi.fn>;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  sent = vi.fn();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function renderComposer() {
  await act(async () => {
    root.render(
      createElement(MessageComposer, {
        placeholder: "Message engineering",
        deliverableChoice: true,
        onSend: sent,
      }),
    );
  });
}

async function send(text: string) {
  const input = container.querySelector("textarea") as HTMLTextAreaElement;
  const setValue = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
  await act(async () => {
    setValue?.call(input, text);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => {
    (container.querySelector('[aria-label="Send"]') as HTMLButtonElement).click();
  });
}

describe("composer intent selection (issue #984)", () => {
  it("starts unselected, sends no override, and resets to unselected after a choice", async () => {
    await renderComposer();

    for (const intent of ["chat", "once", "workflow"]) {
      expect(
        container.querySelector(`[data-testid="composer-deliverable-${intent}"]`)?.getAttribute(
          "aria-pressed",
        ),
      ).toBe("false");
    }

    await send("ordinary message");
    // The trailing `undefined`s are the attachments (issue #1682) and mentions
    // args: the composer widens `onSend` to `(text, intent?, attachments?,
    // mentions?)`, and a message with neither passes them as absent.
    expect(sent).toHaveBeenLastCalledWith("ordinary message", undefined, undefined, undefined);

    await act(async () => {
      (container.querySelector('[data-testid="composer-deliverable-workflow"]') as HTMLButtonElement).click();
    });
    await send("make it reusable");
    expect(sent).toHaveBeenLastCalledWith("make it reusable", "workflow", undefined, undefined);

    for (const intent of ["chat", "once", "workflow"]) {
      expect(
        container.querySelector(`[data-testid="composer-deliverable-${intent}"]`)?.getAttribute(
          "aria-pressed",
        ),
      ).toBe("false");
    }
  });
});
