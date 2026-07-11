import type { CompanyStatus } from "../api/types";
import { lifecycle } from "../lib/language";

interface Props {
  status: CompanyStatus;
  onBack?: () => void;
  onFeedback: () => void;
}

/** The company header: who you're talking to and whether it's live. */
export function StatusBar({ status, onBack, onFeedback }: Props) {
  const state = lifecycle(status.lifecycle);
  return (
    <header className="statusbar">
      {onBack && (
        <button className="btn small" onClick={onBack} title="Switch company">
          ←
        </button>
      )}
      <div>
        <h1>{status.name}</h1>
        <div className="sub">You're talking to your company.</div>
      </div>
      <div className="spacer" />
      <span className={`pill ${state.tone}`}>
        <span className="dot" />
        {state.label}
      </span>
      <button className="btn small" onClick={onFeedback} title="Flag something">
        Flag something
      </button>
    </header>
  );
}
