import type { CompanyStatus } from "../api/types";
import { lifecycle } from "../lib/language";

interface Props {
  companies: CompanyStatus[];
  onPick: (id: string) => void;
}

/** Multi-company hosts: choose which company to operate. */
export function CompanyPicker({ companies, onPick }: Props) {
  return (
    <div className="app">
      <header className="statusbar">
        <h1>Your companies</h1>
      </header>
      <div className="picker">
        {companies.map((c) => {
          const state = lifecycle(c.lifecycle);
          return (
            <button className="btn" key={c.id} onClick={() => onPick(c.id)}>
              <span className="name">{c.name}</span>
              {c.pending_approvals > 0 && (
                <span className="pill idle">
                  {c.pending_approvals} to approve
                </span>
              )}
              <span className={`pill ${state.tone}`}>
                <span className="dot" />
                {state.label}
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
