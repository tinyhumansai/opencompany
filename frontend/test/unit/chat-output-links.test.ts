// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ChatHistoryMessageDto, ChatOutput } from "@/api/types";
import { fromHistory } from "@/lib/chat";
import type { TeamMember } from "@/lib/team";
import { OutputLinkRow } from "@/views/room/MessageRow";
import { ThreadPanel } from "@/views/room/ThreadPanel";
import type { Channel } from "@/views/room/model";

const CHANNEL: Channel = {
  id: "general",
  name: "general",
  voice: "General",
  kind: "channel",
  purpose: "",
};

const MEMBERS: TeamMember[] = [
  {
    id: "writer",
    name: "Writer",
    role: "Writer",
    description: "",
    tone: "violet",
    avatar: "badger",
    inboxEnabled: true,
    effectiveTools: [],
    desks: [],
  },
];

let container: HTMLDivElement;
let root: Root;

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

function rehydratedOutputs(outputs: ChatOutput[]): ChatOutput[] {
  const entry: ChatHistoryMessageDto = {
    id: "42",
    channel: "writer",
    author: "writer",
    text: "Done.",
    atMillis: 1_700_000_000_000,
    mine: false,
    outputs,
  };
  return fromHistory([entry])[0]?.outputs ?? [];
}

function render(outputs: ChatOutput[]) {
  act(() => root.render(createElement(OutputLinkRow, { outputs: rehydratedOutputs(outputs) })));
}

describe("chat reply output links", () => {
  it("rehydrates one workspace output as a button link", () => {
    render([
      {
        kind: "workspace-node",
        targetId: "node-1",
        title: "launch-note.md",
      },
    ]);

    const link = container.querySelector("a");
    expect(link?.textContent).toContain("launch-note.md");
    expect(link?.getAttribute("href")).toBe("#/company/workspace/node-1");
    expect(container.querySelector("button")).toBeNull();
  });

  it("collapses several outputs behind a +N more control", () => {
    render([
      { kind: "workspace-node", targetId: "node-1", title: "first.md" },
      {
        kind: "artifact",
        targetId: "artifact-2",
        title: "Second draft",
        taskId: "task-7",
        version: 3,
      },
      { kind: "workspace-node", targetId: "node-3", title: "third.md" },
    ]);

    expect(container.querySelectorAll("a")).toHaveLength(1);
    const more = container.querySelector("button");
    expect(more?.textContent).toBe("+2 more");
    act(() => more?.click());
    expect(container.querySelectorAll("a")).toHaveLength(3);
    expect(container.querySelectorAll("a")[1]?.getAttribute("href")).toBe(
      "#/tasks/task-7?artifact=artifact-2&v=3",
    );
  });

  it("renders no row when the reply produced nothing", () => {
    render([]);
    expect(container.querySelector("[data-chat-output-links]")).toBeNull();
  });

  it("renders a rehydrated output on a reply that lives only in a thread", () => {
    const outputs = rehydratedOutputs([
      {
        kind: "workspace-node",
        targetId: "thread-node",
        title: "thread-note.md",
      },
    ]);

    act(() =>
      root.render(
        createElement(ThreadPanel, {
          channel: CHANNEL,
          members: MEMBERS,
          parent: { id: "parent", from: "you", text: "Write the note", at: 1 },
          replies: [
            {
              id: "reply",
              parentId: "parent",
              from: "company",
              channel: "writer",
              text: "Done.",
              at: 2,
              outputs,
            },
          ],
          sending: false,
          onSend: vi.fn(),
          onClose: vi.fn(),
        }),
      ),
    );

    const link = container.querySelector('[data-chat-output-links] a');
    expect(link?.textContent).toContain("thread-note.md");
    expect(link?.getAttribute("href")).toBe("#/company/workspace/thread-node");
  });
});
