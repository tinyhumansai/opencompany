/**
 * The workspace editor's unsaved-text buffer (issue #1372).
 *
 * The editor autosaves on a debounce, so between a keystroke and the host's
 * acknowledgement the operator's words live in exactly one place: this tab.
 * Two things have to be true for the whole of that window, and neither is true
 * of a bare `pending` ref:
 *
 * 1. **The window has two halves.** A debounce is waiting, *and then* a request
 *    is in flight. A buffer that only remembers the first half reports "nothing
 *    unsaved" for the entire round trip — which is the half the operator cannot
 *    retype from muscle memory, and the half a page reload actually kills (the
 *    browser cancels the in-flight request on unload). So the buffer counts
 *    writes in flight as well as holding the pending job, and
 *    {@link SaveBuffer.holdsUnsavedWork} answers for both.
 *
 * 2. **An old write is not allowed to speak for a newer edit.** `writeFile` is
 *    async and the operator keeps typing during it. Without sequencing, the
 *    older call's resolution lands last and reports "Saved" over text the host
 *    has never seen — and its *rejection* lands the same way, replacing an
 *    honest "Unsaved" with an "error" for a request that has already been
 *    superseded. So every claim takes a ticket, every edit invalidates
 *    outstanding tickets, and a settled ticket that is no longer the newest
 *    word applies **nothing at all**: no state, no toast, no stamp patch.
 *
 * The buffer is deliberately free of React and of the workspace API: it holds
 * the ordering rules and nothing else, so they can be driven directly in a unit
 * test at the speed of a function call. The caller supplies the writer and the
 * effects as a {@link SaveSink} per flush, which is what lets the component keep
 * passing its current `client` and `company` without the buffer being rebuilt.
 */

/** A note's full text, addressed to the id it belongs to. */
export type SaveJob = { id: string; content: string };

/**
 * The side of a flush the buffer does not own: how to write, and what to do
 * with the answer once the buffer has ruled it current.
 *
 * `onSaved` and `onFailed` are called **only** for a write that is still the
 * newest word. A superseded write is silent by design — the newer job carries
 * the operator's whole note, so its outcome is the one worth reporting, and a
 * stale failure toast beside a newer successful save is noise that reads as a
 * contradiction.
 */
export type SaveSink<Ack> = {
  write: (job: SaveJob) => Promise<Ack>;
  /** The write has started. Fires once per flush that had something to send. */
  onSaving: () => void;
  onSaved: (job: SaveJob, ack: Ack) => void;
  onFailed: (job: SaveJob, error: unknown) => void;
};

export interface SaveBuffer {
  /** Record a keystroke's text, superseding anything outstanding. */
  stage(job: SaveJob): void;
  /** The job waiting on the debounce, without taking it. */
  peek(): SaveJob | null;
  /** Drop the pending job — the note it belongs to is gone or closed. */
  clear(): void;
  /**
   * `true` while the operator has text the host has not acknowledged: a job
   * waiting on the debounce, a write in flight, or both.
   */
  holdsUnsavedWork(): boolean;
  /** Send the pending job, if any, and apply the result only if it is current. */
  flush<Ack>(sink: SaveSink<Ack>): Promise<void>;
}

export function createSaveBuffer(): SaveBuffer {
  let pending: SaveJob | null = null;
  let inFlight = 0;
  // Monotonic. Bumped by every edit *and* every claim, so a ticket stops being
  // the newest word the moment either one happens — an outstanding write is
  // stale whether it was overtaken by typing or by a later write.
  let newest = 0;

  function claim(): { job: SaveJob; ticket: number } | null {
    if (!pending) return null;
    const job = pending;
    pending = null;
    newest += 1;
    inFlight += 1;
    return { job, ticket: newest };
  }

  /** Retire a ticket. `true` when its result is still the newest word. */
  function settle(ticket: number): boolean {
    inFlight = Math.max(0, inFlight - 1);
    return ticket === newest;
  }

  return {
    stage(job) {
      pending = job;
      newest += 1;
    },
    peek() {
      return pending;
    },
    clear() {
      pending = null;
      // A removed or closed note also retires writes already in flight.
      newest += 1;
    },
    holdsUnsavedWork() {
      return pending !== null || inFlight > 0;
    },
    async flush(sink) {
      const claimed = claim();
      if (!claimed) return;
      const { job, ticket } = claimed;
      sink.onSaving();
      try {
        const ack = await sink.write(job);
        if (!settle(ticket)) return;
        sink.onSaved(job, ack);
      } catch (error) {
        if (!settle(ticket)) return;
        // Keep the buffer: the operator's text is never dropped because a save
        // failed, and the next edit retries it. Only when nothing newer arrived
        // — a fresher job here would already be the operator's real work.
        if (!pending) pending = job;
        sink.onFailed(job, error);
      }
    },
  };
}

/**
 * The `beforeunload` handler for a buffer: ask the browser to interrupt the
 * unload while, and only while, the tab is holding words the host has not got.
 *
 * `preventDefault` is the whole modern API — the browser writes its own
 * wording — and an operator who is merely reading is never asked.
 */
export function createUnloadGuard(buffer: SaveBuffer) {
  return (event: BeforeUnloadEvent) => {
    if (!buffer.holdsUnsavedWork()) return;
    event.preventDefault();
  };
}
