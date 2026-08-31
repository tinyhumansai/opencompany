// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { AttachmentDto } from "@/api/types";
import type { ChatMessage } from "@/lib/chat";
import type { TeamMember } from "@/lib/team";
import { ThreadPanel } from "@/views/chat/ThreadPanel";
import type { Channel } from "@/views/chat/model";

/**
 * Issue #1682 follow-up: a thread line must render attachments the way a main
 * timeline row does. The wire accepts a reply with both `parent` and
 * `attachments`, history preserves it, and the server only renders it inside
 * `ThreadPanel` — so if the panel's own `Line` never draws `MessageAttachments`
 * the file is completely invisible and cannot be downloaded.
 */

const CHANNEL: Channel = {
  id: "engineering",
  name: "engineering",
  voice: "Engineering",
  kind: "channel",
  purpose: "",
};

const MEMBERS: TeamMember[] = [
  {
    id: "member-1",
    name: "Ada",
    role: "Engineer",
    description: "",
    tone: "amber",
    avatar: "badger",
    inboxEnabled: true,
    effectiveTools: [],
    desks: [],
  },
];

const pdf: AttachmentDto = { nodeId: "n1", name: "report.pdf", mime: "application/pdf", size: 8192 };
const png: AttachmentDto = { nodeId: "n2", name: "chart.png", mime: "image/png", size: 4096 };

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  URL.createObjectURL = vi.fn(() => "blob:mock");
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

async function render(over: Partial<Parameters<typeof ThreadPanel>[0]> = {}) {
  await act(async () => {
    root.render(
      createElement(ThreadPanel, {
        channel: CHANNEL,
        members: MEMBERS,
        parent: { id: "p", from: "you", text: "see attached", at: 0, attachments: [pdf] },
        replies: [],
        sending: false,
        onSend: vi.fn(),
        onClose: vi.fn(),
        resolveAttachmentUrl: vi.fn(async () => "blob:the-file"),
        ...over,
      }),
    );
  });
  // Flush the preview-fetch effect's microtasks.
  await act(async () => {
    await Promise.resolve();
  });
}

describe("ThreadPanel attachment rendering (issue #1682)", () => {
  it("draws a chip for the parent message's attachment", async () => {
    await render();
    expect(container.textContent).toContain("report.pdf");
    expect(container.querySelector('[title="Download report.pdf"]')).not.toBeNull();
  });

  it("draws a chip for a reply's attachment too", async () => {
    await render({
      replies: [
        {
          id: "r",
          parentId: "p",
          from: "company",
          text: "looks good",
          at: 1,
          attachments: [png],
        } as ChatMessage,
      ],
    });
    expect(container.textContent).toContain("report.pdf");
    expect(container.textContent).toContain("chart.png");
    expect(container.querySelector('[title="Download chart.png"]')).not.toBeNull();
  });

  it("resolves preview/download through the threaded resolver", async () => {
    const resolveUrl = vi.fn(async () => "blob:the-file");
    // An image attachment triggers the in-view preview fetch; a PDF chip only
    // fetches on an explicit download click.
    await render({
      parent: { id: "p", from: "you", text: "see attached", at: 0, attachments: [png] },
      resolveAttachmentUrl: resolveUrl,
    });
    expect(resolveUrl).toHaveBeenCalledWith("n2", expect.any(AbortSignal));
  });
});
