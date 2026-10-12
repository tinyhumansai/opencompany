/**
 * @vitest-environment jsdom
 *
 * The workspace editor's unsaved-text buffer (issue #1372).
 *
 * #1372 made the editor *say* when it is holding text the host has not got.
 * The first pass said it from a single `pending` ref, and a ref is not the
 * whole truth about unsaved work. Two windows escaped it, and both are ways the
 * editor still lies about the same thing:
 *
 * 1. **The round trip.** `flush` cleared the pending ref *before* awaiting the
 *    write, so for the whole duration of the request the buffer reported
 *    "nothing unsaved" — and the `beforeunload` guard let the page go. The
 *    browser cancels an in-flight `PUT` on unload, so the text died in exactly
 *    the window the guard existed to cover.
 *
 * 2. **The overtaken write.** The operator keeps typing during the request. The
 *    older call resolved last and set "Saved" over a newer edit the host has
 *    never seen — the indicator's one job, done backwards. Its *rejection* was
 *    the mirror image: an "error" for a superseded request, replacing an honest
 *    "Unsaved".
 *
 * So the property under test is ordering, not labelling: what counts as unsaved
 * (both halves of the window), and which write is allowed to speak (only the
 * newest). `workspace-save-state.test.ts` covers the wording.
 */
import { describe, expect, it, vi } from "vitest";

import {
  createSaveBuffer,
  createUnloadGuard,
  type SaveJob,
  type SaveSink,
} from "@/lib/workspace-save-buffer";

/**
 * A write the test opens and closes by hand, so it can type mid-flight.
 *
 * `started` resolves when the buffer actually calls the writer, which is the
 * only moment from which "in flight" means anything.
 */
function heldWrite<Ack>() {
  let ok!: (ack: Ack) => void;
  let fail!: (e: unknown) => void;
  let calling!: () => void;
  const started = new Promise<void>((seen) => {
    calling = seen;
  });
  const write = () =>
    new Promise<Ack>((resolve, reject) => {
      ok = resolve;
      fail = reject;
      calling();
    });
  return {
    write,
    started,
    resolve: (ack: Ack) => ok(ack),
    reject: (e: unknown) => fail(e),
  };
}

type Ack = { updatedAt: number };

/** A sink that records which callbacks the buffer let through. */
function spySink(write: SaveSink<Ack>["write"]) {
  return {
    write,
    onSaving: vi.fn(),
    onSaved: vi.fn<(job: SaveJob, ack: Ack) => void>(),
    onFailed: vi.fn<(job: SaveJob, error: unknown) => void>(),
  };
}

/** Let every already-settled microtask drain. */
const drain = () => new Promise<void>((done) => setTimeout(done, 0));

describe("unsaved work while a write is in flight", () => {
  it("still counts as unsaved once the pending job has been claimed", async () => {
    const held = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    buffer.stage({ id: "note", content: "half a sentence" });

    const flushed = buffer.flush(spySink(held.write));
    await held.started;

    // The debounce is spent and the pending job is gone — this is precisely the
    // window the old `pending.current` check reported as clean.
    expect(buffer.peek()).toBeNull();
    expect(buffer.holdsUnsavedWork()).toBe(true);

    held.resolve({ updatedAt: 1 });
    await flushed;
    expect(buffer.holdsUnsavedWork()).toBe(false);
  });

  it("prevents the unload for a write in flight with nothing pending", async () => {
    const held = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    const guard = createUnloadGuard(buffer);
    buffer.stage({ id: "note", content: "half a sentence" });

    const flushed = buffer.flush(spySink(held.write));
    await held.started;

    const midFlight = new Event("beforeunload", { cancelable: true });
    const prevented = vi.spyOn(midFlight, "preventDefault");
    guard(midFlight as BeforeUnloadEvent);
    // The regression: the browser cancels this request if the page goes, so the
    // operator must be asked.
    expect(prevented).toHaveBeenCalled();

    held.resolve({ updatedAt: 1 });
    await flushed;

    const settled = new Event("beforeunload", { cancelable: true });
    const untouched = vi.spyOn(settled, "preventDefault");
    guard(settled as BeforeUnloadEvent);
    // …and never asked once the host has the words. A guard that always fires
    // is a guard operators learn to click through.
    expect(untouched).not.toHaveBeenCalled();
  });

  it("leaves a reader alone", () => {
    const buffer = createSaveBuffer();
    const event = new Event("beforeunload", { cancelable: true });
    const prevented = vi.spyOn(event, "preventDefault");
    createUnloadGuard(buffer)(event as BeforeUnloadEvent);
    expect(prevented).not.toHaveBeenCalled();
  });
});

describe("a write overtaken by newer typing", () => {
  it("does not report the newer edit as saved", async () => {
    const held = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    const sink = spySink(held.write);

    buffer.stage({ id: "note", content: "first" });
    const flushed = buffer.flush(sink);
    await held.started;

    // The operator keeps typing while the request is out.
    buffer.stage({ id: "note", content: "first and second" });

    held.resolve({ updatedAt: 1 });
    await flushed;
    await drain();

    // The host acknowledged "first". It has never seen "first and second", so
    // nothing may announce a save — and the stamp patch must not land either,
    // because it would write the stale content back over the open file.
    expect(sink.onSaved).not.toHaveBeenCalled();
    expect(sink.onFailed).not.toHaveBeenCalled();
    // The newer text is still buffered and still guarded.
    expect(buffer.peek()).toEqual({ id: "note", content: "first and second" });
    expect(buffer.holdsUnsavedWork()).toBe(true);
  });

  it("does not clobber the newer state when it fails", async () => {
    const held = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    const sink = spySink(held.write);

    buffer.stage({ id: "note", content: "first" });
    const flushed = buffer.flush(sink);
    await held.started;

    buffer.stage({ id: "note", content: "first and second" });

    held.reject(new Error("host said no"));
    await flushed;
    await drain();

    // The superseded request's failure is not the operator's situation: the
    // newer job carries the whole note and has not been attempted yet. Saying
    // "Not saved — retrying on edit" here would replace an honest "Unsaved"
    // with a verdict about text nobody is waiting on.
    expect(sink.onFailed).not.toHaveBeenCalled();
    expect(sink.onSaved).not.toHaveBeenCalled();
    // And the newer text survives the failure untouched — not rolled back to
    // the job that failed.
    expect(buffer.peek()).toEqual({ id: "note", content: "first and second" });
  });

  it("still reports a failure that nothing has superseded", async () => {
    const held = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    const sink = spySink(held.write);

    buffer.stage({ id: "note", content: "first" });
    const flushed = buffer.flush(sink);
    await held.started;

    const boom = new Error("host said no");
    held.reject(boom);
    await flushed;

    expect(sink.onFailed).toHaveBeenCalledWith({ id: "note", content: "first" }, boom);
    // The text is kept so the next edit retries it, which is what the error
    // wording promises.
    expect(buffer.peek()).toEqual({ id: "note", content: "first" });
    expect(buffer.holdsUnsavedWork()).toBe(true);
  });

  it("lets the newest of two overlapping writes have the last word", async () => {
    const first = heldWrite<Ack>();
    const second = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    const sink = spySink(first.write);

    buffer.stage({ id: "note", content: "first" });
    const one = buffer.flush(sink);
    await first.started;

    buffer.stage({ id: "note", content: "first and second" });
    const two = buffer.flush({ ...sink, write: second.write });
    await second.started;

    // The newer write lands first; the older one straggles in behind it. Order
    // of arrival must not decide the answer.
    second.resolve({ updatedAt: 2 });
    await two;
    first.resolve({ updatedAt: 1 });
    await one;
    await drain();

    expect(sink.onSaved).toHaveBeenCalledTimes(1);
    expect(sink.onSaved).toHaveBeenCalledWith(
      { id: "note", content: "first and second" },
      { updatedAt: 2 },
    );
    expect(buffer.holdsUnsavedWork()).toBe(false);
  });
});

describe("the ordinary path", () => {
  it("sends nothing when nothing was typed", async () => {
    const buffer = createSaveBuffer();
    const sink = spySink(() => Promise.resolve({ updatedAt: 1 }));
    await buffer.flush(sink);
    expect(sink.onSaving).not.toHaveBeenCalled();
    expect(sink.onSaved).not.toHaveBeenCalled();
  });

  it("announces the write and then the acknowledgement", async () => {
    const buffer = createSaveBuffer();
    const sink = spySink(() => Promise.resolve({ updatedAt: 7 }));
    buffer.stage({ id: "note", content: "done" });
    await buffer.flush(sink);
    expect(sink.onSaving).toHaveBeenCalledTimes(1);
    expect(sink.onSaved).toHaveBeenCalledWith({ id: "note", content: "done" }, { updatedAt: 7 });
    expect(buffer.holdsUnsavedWork()).toBe(false);
  });

  it("forgets a buffer whose note has gone", () => {
    const buffer = createSaveBuffer();
    buffer.stage({ id: "note", content: "orphaned" });
    expect(buffer.peek()).toEqual({ id: "note", content: "orphaned" });
    buffer.clear();
    expect(buffer.peek()).toBeNull();
    expect(buffer.holdsUnsavedWork()).toBe(false);
  });
});

describe("a cleared note with an outstanding save", () => {
  it("does not restore a deleted note when its old save fails", async () => {
    const held = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    const sink = spySink(held.write);
    buffer.stage({ id: "deleted-note", content: "rescued elsewhere" });
    const flushed = buffer.flush(sink);
    await held.started;
    buffer.clear();
    held.reject(new Error("note no longer exists"));
    await flushed;
    expect(buffer.peek()).toBeNull();
    expect(buffer.holdsUnsavedWork()).toBe(false);
    expect(sink.onFailed).not.toHaveBeenCalled();
    expect(sink.onSaved).not.toHaveBeenCalled();
  });

  it("does not report a cleared note's delayed acknowledgement as saved", async () => {
    const held = heldWrite<Ack>();
    const buffer = createSaveBuffer();
    const sink = spySink(held.write);
    buffer.stage({ id: "deleted-note", content: "old words" });
    const flushed = buffer.flush(sink);
    await held.started;
    buffer.clear();
    held.resolve({ updatedAt: 1 });
    await flushed;
    expect(buffer.peek()).toBeNull();
    expect(buffer.holdsUnsavedWork()).toBe(false);
    expect(sink.onSaved).not.toHaveBeenCalled();
    expect(sink.onFailed).not.toHaveBeenCalled();
  });
});
