// The versioned-artifacts API (#187) behind the Task Detail **Artifacts** tab
// (#306). Mirrors `tasks.ts`: plain functions over `OpenCompanyClient`, the
// company scope resolved by `client.scopeFor`, and the host's wire shapes
// re-transcribed here so `tsc` pins the contract.
//
// Two of the host's five artifact routes are deliberately **not** wired:
//
//   - `POST …/artifacts` — opening an artifact is the producer's job. A
//     completed dispatch records its output itself; the console inventing a
//     first version would fabricate agent authorship.
//   - `DELETE …/artifacts/{id}` — #184 requires that no control on the Task
//     Detail screen can alter or delete a past event, and an artifact version
//     is exactly that.

import type { OpenCompanyClient } from "./client";

/** What an artifact holds; picks the renderer. Binary kinds carry a reference, never bytes. */
export type ArtifactKind = "text" | "markdown" | "image" | "file";

/**
 * Who wrote a version. The whole point of the port: an operator's pre-approval
 * edit stays distinguishable from the agent's own re-run.
 */
export type ArtifactAuthor = "agent" | "operator";

/** One immutable revision. Versions are append-only and 1-based. */
export interface ArtifactVersion {
  /** 1-based revision number, contiguous within the artifact. */
  version: number;
  /** The revision's content. */
  body: string;
  /** Whether an agent or a human wrote it. */
  author: ArtifactAuthor;
  /** The agent id, or the operator's label. Display-only. */
  authorId: string;
  /** Epoch-millis the revision was recorded. */
  createdAtMillis: number;
  /** The timeline step that produced it (#185), when known. */
  stepSeq?: number;
  /** Why this revision exists, e.g. `"operator edit before approval"`. */
  note?: string;
}

/** What happened to one diff line. */
export type DiffOp = "equal" | "insert" | "delete";

/** A single line of a diff. */
export interface DiffLine {
  op: DiffOp;
  /** The line's text, without a trailing newline. */
  text: string;
}

/** A line-level diff between two revisions, plus the churn summary. */
export interface ArtifactDiff {
  fromVersion: number;
  toVersion: number;
  /** Newer-side order with deletions interleaved. */
  lines: DiffLine[];
  /** Lines only in the newer side. */
  added: number;
  /** Lines only in the older side. */
  removed: number;
  /**
   * Fraction of the older side's lines that changed, 0.0–1.0. The number worth
   * watching: sustained high churn means the agent's instructions need work.
   */
  churn: number;
}

/**
 * An artifact as the console renders it.
 *
 * The host flattens the record into the response (`#[serde(flatten)]`), so the
 * record's fields sit at the top level rather than under an `artifact` key —
 * do not nest this. `humanEditDiff` is derived and omitted entirely when no
 * human has edited, which is the common, healthy case.
 */
export interface ArtifactView {
  /** Stable id within the company. */
  id: string;
  /** The task that produced it. */
  taskId: string;
  /** A short display title. */
  title: string;
  kind: ArtifactKind;
  /** Every revision, oldest first. */
  versions: ArtifactVersion[];
  /** Epoch-millis the artifact was first created. */
  createdAtMillis: number;
  /** Epoch-millis of the newest revision. */
  updatedAtMillis: number;
  /**
   * The last-agent-version → last-operator-version diff, present only once a
   * human has edited. Inlined by the host so the tab renders the edit story
   * without a second call.
   */
  humanEditDiff?: ArtifactDiff;
}

/** The append body. `author` is deliberately omitted — see {@link appendArtifactVersion}. */
export interface AppendVersionInput {
  body: string;
  note?: string;
}

/** One task's artifacts, most-recently-updated first. */
export function listTaskArtifacts(
  client: OpenCompanyClient,
  company: string | null,
  taskId: string,
): Promise<ArtifactView[]> {
  return client.get<ArtifactView[]>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(taskId)}/artifacts`,
  );
}

/** One artifact with its full version history. 404s when the id names nothing. */
export function getArtifact(
  client: OpenCompanyClient,
  company: string | null,
  artifactId: string,
): Promise<ArtifactView> {
  return client.get<ArtifactView>(
    `${client.scopeFor(company)}/artifacts/${encodeURIComponent(artifactId)}`,
  );
}

/**
 * Append a revision — how an operator's pre-approval edit is captured. The
 * prior version is left untouched, so the diff between them stays computable
 * forever; there is deliberately no route that rewrites a stored version.
 *
 * `author` is **not** sent. The host defaults an unattributed append to
 * `operator` and stamps the note `"operator edit before approval"`, which is
 * precisely what an edit from this console is. Sending it explicitly would let
 * a console bug attribute a human's words to the agent and quietly destroy the
 * one datum the whole port exists to answer.
 *
 * Returns the refreshed artifact, so a caller can render the new version — and
 * the now-present `humanEditDiff` — without a follow-up read.
 */
export function appendArtifactVersion(
  client: OpenCompanyClient,
  company: string | null,
  artifactId: string,
  body: AppendVersionInput,
): Promise<ArtifactView> {
  return client.post<ArtifactView>(
    `${client.scopeFor(company)}/artifacts/${encodeURIComponent(artifactId)}/versions`,
    body,
  );
}

/**
 * Diff two specific revisions of an artifact.
 *
 * Both bounds are always sent: the host treats "neither" as a request for the
 * human-edit diff (already inlined on {@link ArtifactView}) and "one" as a 400.
 * This overload is for comparing a pair the operator picked — two agent drafts
 * after a re-run, say — which nothing else answers.
 */
export function diffArtifact(
  client: OpenCompanyClient,
  company: string | null,
  artifactId: string,
  from: number,
  to: number,
): Promise<ArtifactDiff> {
  const qs = `?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`;
  return client.get<ArtifactDiff>(
    `${client.scopeFor(company)}/artifacts/${encodeURIComponent(artifactId)}/diff${qs}`,
  );
}
