import { describe, expect, it } from "vitest";

import { buildTimeline, type Channel } from "@/views/room/model";
import type { ChatMessage } from "@/lib/chat";
import type { ChatOutput } from "@/api/types";

/**
 * Timeline folding by parent.
 *
 * A reply must be folded into its parent rather than laid out inline. When that
 * goes wrong the transcript still renders — thread replies simply appear in the
 * channel, out of order and stripped of the question they answer, which reads
 * as the company talking to itself. Nothing throws, so only an assertion
 * catches it.
 */

const CHANNEL: Channel = {
  id: "engineering",
  name: "engineering",
  voice: "Engineering",
  kind: "channel",
  purpose: "",
};

const MINUTE = 60_000;
/** A fixed instant, so day-boundary grouping cannot drift with the clock. */
const T0 = new Date("2026-03-02T10:00:00Z").getTime();

function message(over: Partial<ChatMessage> & Pick<ChatMessage, "id">): ChatMessage {
  return { from: "you", text: "…", at: T0, ...over };
}

describe("buildTimeline", () => {
  /**
   * Issue #1890 D part 2 changed this, and it is the load-bearing change.
   *
   * Part 1 threads every answer under the message that opened it, so folding
   * every parented line — which is what this file asserted before — would leave
   * the channel a column of your own questions each wearing a "1 reply" chip,
   * with every answer deleted from the view. A question answered with nothing
   * in between is not a thread anyone opened, and now renders as what it is.
   */
  it("lays a first reply out inline when nothing came between question and answer", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "can we ship?" }),
        message({ id: "b", from: "company", text: "yes", parentId: "a" }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a", "b"]);
    // And it is not ALSO on the parent's chip: a reader seeing the answer must
    // not be told there is one more thing to open.
    expect(entries[0].replies).toEqual([]);
  });

  /**
   * The operator replied to their own root before the agent got there.
   *
   * `bucket[0]` is merely the earliest reply, so that follow-up was promoted
   * into the channel and the agent's actual answer stayed folded behind the
   * root's chip — a thread a person deliberately opened, flattened, showing
   * them their own words twice and the reply not at all (codex on #1972).
   */
  it("keeps a thread folded when the operator replied first, not the agent", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "can we ship?" }),
        message({ id: "b", text: "…or next week?", parentId: "a", at: T0 + 1 }),
        message({ id: "c", from: "company", text: "yes", parentId: "a", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a"]);
    expect(entries[0].replies.map((r) => r.id)).toEqual(["b", "c"]);
  });

  /**
   * A colleague is not the runtime.
   *
   * `fromHistory` projects `from` off `mine`, so another signed-in person's
   * reply arrives as `company` and is told apart only by `byPerson`. Without
   * that term their message was promoted just as the operator's own follow-up
   * had been — the same defect, for every viewer except the one who wrote it
   * (codex + coderabbit on #2001).
   */
  it("keeps a thread folded when a colleague replied before the runtime did", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "can we ship?" }),
        message({
          id: "b",
          from: "company",
          byPerson: true,
          text: "asking the team",
          parentId: "a",
          at: T0 + 1,
        }),
        message({ id: "c", from: "company", text: "yes", parentId: "a", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a"]);
    expect(entries[0].replies.map((r) => r.id)).toEqual(["b", "c"]);
  });

  /**
   * `undefined` is not `true`, and the difference is what keeps the ordinary
   * case working: every locally built company line — this console's own POST,
   * an `AgentReplyEvent` — leaves `byPerson` unset, and only `fromHistory` ever
   * sets it. Reading "unset" as "might be a person" would fold the live answer
   * this promotion exists for.
   */
  it("still promotes a runtime answer that carries no byPerson at all", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "can we ship?" }),
        message({ id: "b", from: "company", text: "yes", parentId: "a", at: T0 + 1 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a", "b"]);
  });

  /**
   * A row that only carries outputs did not speak.
   *
   * A seat that writes a file and then reports on it emits an empty `post`
   * carrying the workspace link and then the write-up. Counting the carrier
   * as a second utterance folded the answer: a live run wrote a campaign
   * brief, asked six teammates, and showed the operator a chip rather than
   * the report. The host already keeps such a row out of what other seats
   * read as speech; this is the same judgement on the render side.
   *
   * The carrier is promoted with it, never apart — lifting the write-up and
   * leaving its file links folded would put one turn's output on two
   * surfaces, the exact split this rule refuses for a capped turn.
   */
  it("promotes an answer whose turn also emitted an outputs-only carrier", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "own the pricing launch" }),
        message({
          id: "b",
          from: "company",
          text: "",
          parentId: "a",
          at: T0 + 1,
          outputs: [{ kind: "workspace-node", targetId: "n1", title: "brief.md" }],
        }),
        message({ id: "c", from: "company", text: "Brief is written and signed off.", parentId: "a", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a", "b", "c"]);
  });

  /**
   * Every child of the root shares its bucket, not just this turn's, so
   * carrier promotion stops at the operator's next line. Without the bound,
   * `[root, answer, follow-up, laterCarrier]` passes every other test — the
   * answer is the first non-carrier and nothing interleaves it with the root
   * — and the later carrier is lifted out of the thread the operator
   * deliberately opened.
   */
  it("does not promote a carrier that belongs to a later exchange", () => {
    const outputs: ChatOutput[] = [{ kind: "workspace-node", targetId: "n1", title: "brief.md" }];
    const entries = buildTimeline(
      [
        message({ id: "a", text: "own the pricing launch" }),
        message({ id: "b", from: "company", text: "On it.", parentId: "a", at: T0 + 1 }),
        message({ id: "c", text: "and the paid ads?", parentId: "a", at: T0 + 2 }),
        message({ id: "d", from: "company", text: "", parentId: "a", at: T0 + 3, outputs }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a", "b"]);
  });

  /**
   * Two things actually said still fold. The carrier rule narrows what counts
   * as speech; it does not retire the boundary promotion was built for.
   */
  it("still folds a turn that spoke twice, carrier or not", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "own the pricing launch" }),
        message({
          id: "b",
          from: "company",
          text: "",
          parentId: "a",
          at: T0 + 1,
          outputs: [{ kind: "workspace-node", targetId: "n1", title: "brief.md" }],
        }),
        message({ id: "c", from: "company", text: "Partial write-up.", parentId: "a", at: T0 + 2 }),
        message({ id: "d", from: "system", text: "This turn hit its cap.", parentId: "a", at: T0 + 3 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a"]);
  });

  /**
   * A settle marker is runtime-generated but it is not an answer, and #1890 B
   * put markers in the thread that raised the card on purpose. Promoting one
   * into the channel would undo that from the render side.
   */
  it("keeps a thread folded when a system marker is its first line", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "draft the launch email" }),
        message({ id: "b", from: "system", text: "finished → In review", parentId: "a", at: T0 + 1 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a"]);
    expect(entries[0].replies.map((r) => r.id)).toEqual(["b"]);
  });

  it("folds the pair instead once another conversation interleaved", () => {
    // The case the fold was always for: two conversations racing in one
    // channel, where laying the answer out inline would interleave them into
    // nonsense. `b` is a second question that arrived while `a` was being
    // worked on, so `a`'s answer collapses onto `a`'s summary row.
    const entries = buildTimeline(
      [
        message({ id: "a", text: "can we ship?" }),
        message({ id: "b", text: "unrelated question", at: T0 + 1 }),
        message({ id: "c", text: "yes", parentId: "a", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries.map((e) => e.message.id)).toEqual(["a", "b"]);
    expect(entries[0].replies.map((r) => r.id)).toEqual(["c"]);
  });

  /**
   * Wider than "another root", deliberately: a sibling thread's reply landing
   * between question and answer interleaves the two conversations on screen
   * just as visibly as a new question does, and the rule is about what a reader
   * sees rather than about the shape of the id graph.
   */
  it("counts a sibling thread's reply as interleaving too", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", text: "can we ship?" }),
        message({ id: "x", text: "other topic", at: T0 - 2 }),
        message({ id: "x1", text: "other answer", parentId: "x", at: T0 + 1 }),
        message({ id: "b", text: "yes", parentId: "a", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );
    // `a`'s answer folds, because `x`'s did land in between.
    const a = entries.find((e) => e.message.id === "a");
    expect(a?.replies.map((r) => r.id)).toEqual(["b"]);
  });

  it("keeps the remaining replies in order under their parent", () => {
    // The first is inline; the rest stay on the chip, oldest first.
    const entries = buildTimeline(
      [
        message({ id: "a" }),
        message({ id: "b", from: "company", parentId: "a", at: T0 + 1 }),
        message({ id: "c", parentId: "a", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );
    expect(entries.map((e) => e.message.id)).toEqual(["a", "b"]);
    expect(entries[0].replies.map((r) => r.id)).toEqual(["c"]);
  });

  it("renders a grandchild nowhere: the fold is exactly one level deep", () => {
    // The property the Rust side depends on (issue #435). Only a parentless
    // message becomes an entry, and only an entry carries a `replies` list, so
    // a reply-to-a-reply is bucketed under a row that is never itself rendered
    // and disappears — silently, with nothing thrown and the transcript looking
    // complete.
    //
    // `cycle_conversation` in `src/runtime/cycle.rs` parents an approval
    // continuation to the thread *root* rather than to the message that raised
    // it, and this is the whole reason: parenting to the raiser would put the
    // continuation exactly here whenever the raiser is itself a thread reply.
    // The existing "parent is not in this channel" case below does NOT pin
    // this — there the parent is absent from the transcript entirely, so it
    // would keep passing if the console ever grew a second fold level and
    // quietly made #435's routing choice unnecessary.
    const entries = buildTimeline(
      [
        message({ id: "a", text: "can we ship?" }),
        message({ id: "b", from: "company", text: "yes", parentId: "a", at: T0 + 1 }),
        message({ id: "c", text: "when?", parentId: "b", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );

    // The root and its direct reply are the rows — the reply inline, since
    // nothing came between them (#1890 D part 2). What matters here is
    // unchanged: the SECOND level is reachable from nowhere.
    expect(entries.map((e) => e.message.id)).toEqual(["a", "b"]);
    // And `c` is reachable from nowhere in the rendered output.
    const rendered = entries.flatMap((e) => [e.message.id, ...e.replies.map((r) => r.id)]);
    expect(rendered).not.toContain("c");
  });

  /**
   * **Reversed by issue #1890 D, deliberately.** This asserted that a reply
   * pointing at an id the transcript does not hold is *dropped* — safe while
   * only hand-opened threads carried a `parentId`, because such a reply was
   * rare and promoting it risked the "the console lost my thread" flicker
   * `reconcileIds` exists to prevent.
   *
   * Part 1 gives every answer a parent, so the same rule silently deletes
   * answers, and two of the cases are ordinary rather than exotic:
   *
   * - a reply to a message **another client** sent, since this console does not
   *   draw an operator line it did not send (`chat-live-events.spec.ts`);
   * - a reply that arrives before `reconcileIds` has swapped a locally-sent
   *   message's id for the host's — which a killed POST leaves pending for
   *   good (`chat-detached-post-failure.spec.ts`).
   *
   * Both were caught by E2E, not here. The trade is a reply whose root fell
   * outside the history window reading without its question, against an answer
   * that is simply gone; a lost answer is the failure this sub-issue exists to
   * prevent.
   */
  it("renders a reply whose parent is not in this channel flat, rather than losing it", () => {
    const entries = buildTimeline([message({ id: "b", parentId: "missing" })], CHANNEL, []);
    expect(entries.map((e) => e.message.id)).toEqual(["b"]);
    // No chip: there is no root here to summarise anything under.
    expect(entries[0].replies).toEqual([]);
  });

  it("renders every orphan of one absent parent, not just the first", () => {
    // The first-reply-inline rule needs a summary row to put the rest on. With
    // no root in the transcript there is none, so all of them render or the
    // remainder is lost for the same reason the single case was.
    const entries = buildTimeline(
      [
        message({ id: "b", parentId: "missing", at: T0 + 1 }),
        message({ id: "c", parentId: "missing", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );
    expect(entries.map((e) => e.message.id)).toEqual(["b", "c"]);
  });

  it("still renders a grandchild nowhere when its own parent IS present", () => {
    // The orphan arm is about a root the transcript never held. It must not
    // relax the one-level-deep rule for a reply whose parent is right there.
    const entries = buildTimeline(
      [
        message({ id: "a" }),
        message({ id: "b", from: "company", parentId: "a", at: T0 + 1 }),
        message({ id: "c", parentId: "b", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );
    expect(entries.map((e) => e.message.id)).toEqual(["a", "b"]);
  });

  it("groups consecutive lines from one sender into a run", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", from: "you", at: T0 }),
        message({ id: "b", from: "you", at: T0 + MINUTE }),
      ],
      CHANNEL,
      [],
    );
    expect(entries[0].continuation).toBe(false);
    expect(entries[1].continuation).toBe(true);
  });

  it("breaks the run once the grouping window has passed", () => {
    const entries = buildTimeline(
      [
        message({ id: "a", from: "you", at: T0 }),
        message({ id: "b", from: "you", at: T0 + 6 * MINUTE }),
      ],
      CHANNEL,
      [],
    );
    expect(entries[1].continuation).toBe(false);
  });

  it("ends a run after a row that carries replies", () => {
    // Otherwise the thread's summary row sits between two lines that read as a
    // single utterance.
    const entries = buildTimeline(
      [
        message({ id: "a", from: "you", at: T0 }),
        // Interleaved, so `a`'s reply folds onto its chip instead of rendering
        // inline (#1890 D part 2) — which is the state this test is about.
        message({ id: "x", from: "you", at: T0 + 1 }),
        message({ id: "r", from: "you", at: T0 + 2, parentId: "a" }),
        message({ id: "b", from: "you", at: T0 + MINUTE }),
      ],
      CHANNEL,
      [],
    );
    expect(entries[0].replies).toHaveLength(1);
    expect(entries[1].continuation).toBe(false);
  });

  it("starts a new day with a divider, and never continues a run across one", () => {
    const nextDay = T0 + 24 * 60 * MINUTE;
    const entries = buildTimeline(
      [
        message({ id: "a", from: "you", at: T0 }),
        message({ id: "b", from: "you", at: nextDay }),
      ],
      CHANNEL,
      [],
    );
    expect(entries[0].dayLabel).toBeDefined();
    expect(entries[1].dayLabel).toBeDefined();
    expect(entries[1].continuation).toBe(false);
  });
});

/**
 * Who a thread's summary pile draws (issue #1324).
 *
 * The facepile under a threaded row used to seed each tile on the *message's*
 * channel, and every reply in a thread carries the same channel — so a
 * three-face pile hashed to one tone and drew one colour three times. Combined
 * with `markOnly`, which renders a deliberately empty tile, the row showed
 * three identical featureless squares: not faces, and not information.
 *
 * Resolving the senders here is what makes the pile able to say anything. It
 * goes through the same `senderOf` every rendered row uses, so a face in the
 * summary is the same face the thread shows when it is opened — the agreement
 * #1170 and #1185 established for DMs, extended to threads.
 */
describe("buildTimeline: the voices behind a thread", () => {
  it("tells apart voices the message channel alone could not", () => {
    // Both replies are `from: "company"` on the same parent. Before, both
    // seeded on `channel` and collapsed to one tone; the *originating* channel
    // is what actually distinguishes them.
    const entries = buildTimeline(
      [
        message({ id: "a", text: "who can take this?" }),
        // Interleaved so the whole thread folds — the pile is what is under
        // test, and an inline first reply would leave only one face in it.
        message({ id: "x", text: "meanwhile" }),
        message({ id: "b", from: "company", channel: "agent_ada", parentId: "a", at: T0 + 1 }),
        message({ id: "c", from: "company", channel: "agent_bo", parentId: "a", at: T0 + 2 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries[0].replySenders.map((s) => s.key)).toEqual(["agent:agent_ada", "agent:agent_bo"]);
  });

  it("counts a person once however often they replied — a pile is people, not messages", () => {
    const entries = buildTimeline(
      [
        message({ id: "a" }),
        message({ id: "x", text: "meanwhile" }),
        message({ id: "b", parentId: "a", at: T0 + 1 }),
        message({ id: "c", parentId: "a", at: T0 + 2 }),
        message({ id: "d", parentId: "a", at: T0 + 3 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries[0].replies).toHaveLength(3);
    expect(entries[0].replySenders.map((s) => s.key)).toEqual(["you"]);
  });

  it("keeps the voices in the order they first spoke", () => {
    const entries = buildTimeline(
      [
        message({ id: "a" }),
        message({ id: "x", text: "meanwhile" }),
        message({ id: "b", from: "company", channel: "agent_bo", parentId: "a", at: T0 + 1 }),
        message({ id: "c", parentId: "a", at: T0 + 2 }),
        // Bo again — already seen, so this must not move them down the pile.
        message({ id: "d", from: "company", channel: "agent_bo", parentId: "a", at: T0 + 3 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries[0].replySenders.map((s) => s.key)).toEqual(["agent:agent_bo", "you"]);
  });

  it("draws no face for a system line — it has no voice to draw", () => {
    // A pile that counted this would claim one more participant than the
    // thread has.
    const entries = buildTimeline(
      [
        message({ id: "a" }),
        message({ id: "x", text: "meanwhile" }),
        message({ id: "b", from: "system", text: "approved", parentId: "a", at: T0 + 1 }),
      ],
      CHANNEL,
      [],
    );

    expect(entries[0].replies).toHaveLength(1);
    expect(entries[0].replySenders).toEqual([]);
  });

  it("leaves a row with no replies with no voices", () => {
    const entries = buildTimeline([message({ id: "a" })], CHANNEL, []);
    expect(entries[0].replySenders).toEqual([]);
  });
});
