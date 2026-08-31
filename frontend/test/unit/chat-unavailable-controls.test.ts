// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { ChannelRail } from "@/views/chat/ChannelRail";
import { MessageComposer } from "@/views/chat/MessageComposer";

let container: HTMLDivElement;
let root: Root;

function render(element: ReturnType<typeof createElement>) {
  act(() => root.render(element));
}

function action(label: string) {
  return container.querySelector(`[aria-label="${label}"]`);
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("chat only renders controls it can perform (issue #1336)", () => {
  it("offers the new-message picker in the channel rail when there is someone to message", () => {
    render(
      createElement(ChannelRail, {
        sections: [],
        activeId: null,
        unread: {},
        onSelect: () => {},
        directMessages: [{ id: "dm:2", name: "Ade", kind: "dm", purpose: "" }],
        onStartDirectMessage: () => {},
      }),
    );

    const button = action("New message");
    expect(button).not.toBeNull();
    expect((button as HTMLButtonElement).disabled).toBe(false);
  });

  it("holds the new-message action back when nobody can be messaged", () => {
    render(
      createElement(ChannelRail, {
        sections: [],
        activeId: null,
        unread: {},
        onSelect: () => {},
        directMessages: [],
        onStartDirectMessage: () => {},
      }),
    );

    const button = action("New message");
    expect(button).not.toBeNull();
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps working composer controls and holds unavailable ones back", () => {
    render(
      createElement(MessageComposer, {
        placeholder: "Message",
        onSend: () => {},
      }),
    );

    expect(action("Mention someone")).not.toBeNull();
    expect(action("Formatting")).not.toBeNull();
    expect(action("Attach a file")).toBeNull();
    expect(action("Add an emoji")).toBeNull();

    act(() => (action("Formatting") as HTMLButtonElement).click());
    expect(action("Bold")).not.toBeNull();
    expect(action("Bulleted list")).toBeNull();
    expect(action("Link")).toBeNull();
  });
});
