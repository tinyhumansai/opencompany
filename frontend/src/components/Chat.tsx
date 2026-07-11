import { useEffect, useRef, useState } from "react";

import type { OpenCompanyClient } from "../api/client";
import { ApiError } from "../api/types";

export interface ChatMessage {
  id: string;
  from: "you" | "company" | "system";
  text: string;
}

interface Props {
  client: OpenCompanyClient;
  company: string;
  messages: ChatMessage[];
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>;
  /** Called after a reply lands, so the parent can refresh approvals/status. */
  onReply?: () => void;
}

let seq = 0;
const nextId = () => `m${seq++}`;

/** The one-voice conversation with the company. */
export function Chat({ client, company, messages, setMessages, onReply }: Props) {
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scroller.current?.scrollTo({ top: scroller.current.scrollHeight, behavior: "smooth" });
  }, [messages]);

  async function send() {
    const text = draft.trim();
    if (!text || sending) return;
    setDraft("");
    setMessages((m) => [...m, { id: nextId(), from: "you", text }]);
    setSending(true);
    try {
      const reply = await client.chat(text, company);
      const replies = reply.responses.length
        ? reply.responses.map((r) => ({ id: nextId(), from: "company" as const, text: r.text }))
        : [{ id: nextId(), from: "system" as const, text: "(no reply)" }];
      setMessages((m) => [...m, ...replies]);
      onReply?.();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      setMessages((m) => [...m, { id: nextId(), from: "system", text: `Couldn't send — ${msg}` }]);
    } finally {
      setSending(false);
    }
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  }

  return (
    <div className="chat">
      <div className="messages" ref={scroller}>
        {messages.length === 0 && (
          <div className="bubble system">
            Say hello, ask for an update, or hand off a task. Your company handles the rest.
          </div>
        )}
        {messages.map((m) => (
          <div key={m.id} className={`bubble ${m.from}`}>
            {m.from === "company" && <div className="byline">{"Your company"}</div>}
            {m.text}
          </div>
        ))}
        {sending && <div className="bubble system">…working on it</div>}
      </div>
      <div className="composer">
        <textarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder="Message your company…"
          rows={1}
        />
        <button className="btn primary" onClick={() => void send()} disabled={sending || !draft.trim()}>
          Send
        </button>
      </div>
    </div>
  );
}
