import { useCallback, useEffect, useRef, useState } from "react";

import type { OpenCompanyClient } from "../api/client";
import type { ApprovalSummary, CompanyStatus } from "../api/types";
import { Approvals } from "../components/Approvals";
import { Chat, type ChatMessage } from "../components/Chat";
import { FeedbackDialog } from "../components/FeedbackDialog";
import { StatusBar } from "../components/StatusBar";

interface Props {
  client: OpenCompanyClient;
  company: string;
  initialStatus: CompanyStatus;
  onBack?: () => void;
}

const POLL_MS = 5000;

/** The operating surface for a single company: status, chat, approvals. */
export function Console({ client, company, initialStatus, onBack }: Props) {
  const [status, setStatus] = useState<CompanyStatus>(initialStatus);
  const [approvals, setApprovals] = useState<ApprovalSummary[]>([]);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [now, setNow] = useState(() => Date.now());
  const [showFeedback, setShowFeedback] = useState(false);
  const mounted = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const [s, a] = await Promise.all([client.status(company), client.approvals(company)]);
      if (!mounted.current) return;
      setStatus(s);
      setApprovals(a);
      setNow(Date.now());
    } catch {
      /* transient; keep the last good view */
    }
  }, [client, company]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    const timer = setInterval(() => void refresh(), POLL_MS);
    return () => {
      mounted.current = false;
      clearInterval(timer);
    };
  }, [refresh]);

  return (
    <div className="app">
      <StatusBar status={status} onBack={onBack} onFeedback={() => setShowFeedback(true)} />
      <Chat
        client={client}
        company={company}
        messages={messages}
        setMessages={setMessages}
        onReply={() => void refresh()}
      />
      <Approvals
        client={client}
        company={company}
        approvals={approvals}
        now={now}
        onResolved={(line) => {
          setMessages((m) => [...m, { id: `sys${Date.now()}`, from: "system", text: line }]);
          void refresh();
        }}
      />
      {showFeedback && (
        <FeedbackDialog client={client} company={company} onClose={() => setShowFeedback(false)} />
      )}
    </div>
  );
}
