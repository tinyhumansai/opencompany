import { useEffect, useMemo, useRef, useState } from "react";
import { ArrowUp, AtSign, Loader2, Paperclip, Plus, X } from "lucide-react";

import type { MessageIntent } from "@/api/tasks";
import type { AttachmentDto } from "@/api/types";
import { formatBytes } from "@/api/workspace";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useImeEnter } from "@/hooks/use-ime-enter";
import { COMPOSER_INTENT_HIDDEN } from "@/product-scope";
import { cn } from "@/lib/utils";
import { MentionPicker } from "@/views/room/MentionPicker";
import {
  activeMentionQuery,
  aliasSet,
  insertMention,
  mentionsOutsideChannel,
  mentionsOutsideRange,
  rankMentionables,
  reconcileMentions,
  resolvableMentions,
  stripCodeRegions,
  type Mention,
  type Mentionable,
} from "@/views/room/mentions";

interface Props {
  placeholder: string;
  disabled?: boolean;
  /**
   * May report whether the send actually journaled (issue #1682, codex
   * review round 4): `true` means it definitely did; `false` means the host
   * definitely never saw it (refused before any journal write — safe to
   * clean up an attachment it carried); `undefined` means the outcome is
   * AMBIGUOUS — a network drop, a timeout, an aborted request — where the
   * message may well have landed even though this call could not confirm it.
   * The composer only ever deletes an attachment on an explicit `false`.
   * Treating `undefined` as "not sent" would risk deleting the node out from
   * under a message that was actually delivered — a worse bug than the rare
   * orphaned upload this declines to clean up. `void` (the thread and
   * copilot composers, which never attach) is "nothing to reconcile."
   */
  onSend: (
    text: string,
    intent?: MessageIntent,
    attachments?: AttachmentDto[],
    mentions?: Mention[],
  ) => void | Promise<boolean | undefined>;
  /** A new revision replaces the draft and focuses the composer. */
  prefill?: { text: string; revision: number };
  /**
   * Focus the input whenever this changes — the open channel's id, so picking
   * a conversation in the sidebar puts the cursor in its composer and you can
   * start typing without a second click. Fine pointers only: on a touch screen
   * focusing an input raises the keyboard over the transcript you just opened.
   */
  focusKey?: string;
  /**
   * Called as the box is typed in, so the company can show a typing
   * indicator.
   *
   * Fired on **every** change rather than on a timer: throttling is the
   * caller's job, because it is per channel and this component does not know
   * which channel it is in. Absent on the composers where a typing indicator
   * would be noise.
   */
  onTyping?: () => void;
  /**
   * The ids of the teammates on this channel, for the outside-channel warning.
   * Absent when membership is unknown.
   */
  channelMemberIds?: string[];
  /**
   * Everything an `@` can name here, from `GET {scope}/chat/mentionables`.
   *
   * Absent — a host that predates the route, or a surface with no roster
   * loaded — simply means no picker opens; typing an `@` is then plain text
   * and the host still extracts what it can from it. So the composer degrades
   * to exactly its previous behaviour rather than to a broken one.
   */
  mentionables?: Mentionable[];
  /** Compact form, for the narrower thread panel. */
  compact?: boolean;
  /**
   * Show the what-is-this-message-for control (issues #580, #1152), opt-in per
   * composer.
   *
   * The channel and DM composers ask for it — either can open a board card, so
   * "just chatting" versus "do it once" versus "build me the workflow" belongs
   * at both prompt boxes. The thread and copilot composers never carry it, so
   * their `onSend` stays a plain `(text)` and their wire shape is unchanged.
   *
   * DMs were omitted when #580 landed (issue #845). Nothing downstream was
   * scoped to channels — the chat route reads `deliverable` off the payload
   * whatever thread it came from — so a DM asking for a workflow was sent as a
   * `once` card, dispatched to a desk agent holding no authoring tool, and came
   * back as a refusal. The control was the only part missing.
   */
  deliverableChoice?: boolean;
  /**
   * Upload one attachment's bytes and hand back its stored reference (issue
   * #1682). Given only where attaching makes sense — the channel and DM
   * composers — so the paperclip is present exactly when the surface can carry
   * a file. The composer holds the returned reference as a pending chip and
   * threads it onto the next `onSend`; the actual upload/verify lives in
   * `RoomView`.
   */
  uploadAttachment?: (file: File) => Promise<AttachmentDto>;
  /**
   * Delete a staged attachment's stored node once it is no longer going to be
   * sent (issue #1682, codex review finding).
   *
   * An upload lands — and is charged against the workspace quota — the
   * instant it succeeds, before the operator has sent anything. Called when
   * the pending chip's Remove is clicked, when a fresh pick replaces it, and
   * when the composer unmounts still holding one — every path that drops the
   * local reference without a send ever claiming the node.
   */
  deleteAttachment?: (nodeId: string) => void;
}

/**
 * A staged attachment together with the scope-bound delete that must free it.
 *
 * `deleteAttachment` is re-bound when the surrounding view switches company or
 * connection, while this composer stays mounted; the unmount cleanup below is
 * mounted only once and captured the *first* callback. Holding the node alone
 * would therefore delete the new company's node through the old company's
 * callback (orphaning it) or an old node through the new callback (targeting
 * the wrong workspace) once the scope moves mid-staging. Capturing the delete
 * alongside the reference keeps every cleanup on the company that owns the
 * upload (codex review finding on #1682).
 */
interface PendingAttachment {
  reference: AttachmentDto;
  /** Same optionality as the `deleteAttachment` prop it mirrors. */
  delete?: (nodeId: string) => void;
}

/**
 * The end of the `@name` the caret sits inside, or `from` when nothing follows.
 *
 * Scans forward while the text still reads as a name the directory knows: a
 * character is included only if the run up to it is a prefix of some alias.
 * That keeps `@Jane Doe` whole (two words, one name) while stopping at the
 * space in `@engineer and …` the moment "engineer and" ceases to be a name —
 * so picking over an existing mention replaces the mention, not the sentence.
 */
function activeMentionEnd(
  text: string,
  from: number,
  nameStart: number,
  aliases: ReadonlySet<string>,
): number {
  let i = from;
  // No aliases means no name can match, so the token cannot extend past its
  // start: stop scanning rather than walk the loop with `prefix` never set.
  if (aliases.size === 0) return nameStart;
  while (i < text.length) {
    const ch = text[i];
    const next =
      /[A-Za-z0-9_.-]/.test(ch)
        ? i + 1
        : ch === " " && /[A-Za-z0-9_]/.test(text[i + 1] ?? "")
          ? i + 1
          : null;
    if (next === null) return i;
    const name = text.slice(nameStart, next).toLowerCase();
    let prefix = false;
    for (const alias of aliases) {
      if (alias.startsWith(name)) {
        prefix = true;
        break;
      }
    }
    if (!prefix) return i;
    i = next;
  }
  return i;
}

/**
 * The composer dock.
 *
 * One line: a `+` menu (attach files, mention someone), the input, and Send.
 * The input grows with a multi-line draft up to a cap before scrolling. Enter
 * sends; Shift+Enter breaks the line, which is the convention every chat
 * client shares.
 */
export function MessageComposer({
  placeholder,
  disabled,
  onSend,
  prefill,
  focusKey,
  compact,
  channelMemberIds,
  deliverableChoice,
  mentionables,
  onTyping,
  uploadAttachment,
  deleteAttachment,
}: Props) {
  const [draft, setDraft] = useState("");
  const imeEnter = useImeEnter();
  // Up to the server's bounded maximum of twenty files can ride one message.
  // Each keeps the delete callback for the company scope that owns its node.
  const [pending, setPending] = useState<PendingAttachment[]>([]);
  // Mirrors `pending` for the unmount cleanup below, which needs the latest
  // value inside a closure captured once at mount.
  const pendingRef = useRef<PendingAttachment[]>([]);
  // Whether this instance is still mounted, checked after every `await`
  // (issue #1682, codex review finding). Without it, an upload that lands
  // after the operator has already navigated away resolves into a
  // continuation on a dead component: the unmount cleanup below ran and saw
  // nothing pending, so nothing would ever free the node that upload just
  // charged against the quota.
  const mountedRef = useRef(true);
  useEffect(() => {
    return () => {
      mountedRef.current = false;
    };
  }, []);
  // The cleanup callback for the scope currently on screen. `RoomView`
  // re-binds `deleteAttachment` (and `uploadAttachment`) when the company or
  // connection changes while this composer stays mounted; an in-flight
  // upload's continuation compares its captured callback against this to know
  // whether the scope it was sent to is still the one showing before staging
  // the result (codex review finding).
  const scopeDeleteRef = useRef(deleteAttachment);
  scopeDeleteRef.current = deleteAttachment;
  // The upload is in flight: the paperclip spins and Send waits, so a message
  // cannot post ahead of the bytes it references. `uploadingCountRef` includes
  // queued batches as well as the one currently uploading: clearing the state
  // when any one batch finishes would let Send race the rest of the queue.
  const [uploading, setUploading] = useState(false);
  const uploadingCountRef = useRef(0);
  // Paste and drop may dispatch several `addFiles` calls before the first
  // upload resolves. Start each batch only after its predecessor has staged
  // its files, so its capacity calculation observes the latest pending count.
  const uploadQueueRef = useRef<Promise<void>>(Promise.resolve());
  const [attachError, setAttachError] = useState<string>();
  const fileInput = useRef<HTMLInputElement>(null);
  const [dragDepth, setDragDepth] = useState(0);
  // What the draft currently resolves to. Reconciled on every edit, so editing
  // or backspacing through a chip un-mentions it rather than leaving a ping
  // for somebody whose name is no longer in the message.
  const [mentions, setMentions] = useState<Mention[]>([]);
  // Where the caret is in an `@query`, or null when it is not in one. Held in
  // state (not derived at render) because it has to survive the mouse leaving
  // the textarea to click a row.
  const [query, setQuery] = useState<{ start: number; query: string } | null>(null);
  // Teammate ids the draft addresses who cannot see this channel. Checked on
  // send; a non-empty list warns rather than sending, and a second send passes
  // through after the user has confirmed.
  const [outsideWarning, setOutsideWarning] = useState<string[] | null>(null);
  const [activeRow, setActiveRow] = useState(0);
  // What the NEXT line is for, and only the next one. It starts and resets
  // unselected: an intent is an operator assertion, so no button may claim one
  // until the operator presses it (issue #984). An unmarked message therefore
  // reaches the host without an override and lets triage decide whether it is
  // work or conversation.
  const [intent, setIntent] = useState<MessageIntent>();
  const input = useRef<HTMLTextAreaElement>(null);

  // A first-run card lives above the timeline, outside this component. The
  // revision lets it request the same prompt more than once after an operator
  // edits or clears it; comparing text alone would make the second click inert.
  useEffect(() => {
    if (!prefill) return;
    setDraft(prefill.text);
    setMentions([]);
    setOutsideWarning(null);
    closePicker();
    setIntent("once");
    input.current?.focus();
  }, [prefill]);

  useEffect(() => {
    if (focusKey === undefined) return;
    if (typeof window.matchMedia === "function" && !window.matchMedia("(pointer: fine)").matches) {
      return;
    }
    input.current?.focus({ preventScroll: true });
  }, [focusKey]);

  function closePicker() {
    setQuery(null);
    setActiveRow(0);
  }

  const rows = useMemo(
    () => (query && mentionables ? rankMentionables(mentionables, query.query) : []),
    [query, mentionables],
  );
  // Built once per directory, not per keystroke.
  const aliases = useMemo(
    () => (mentionables ? aliasSet(mentionables) : undefined),
    [mentionables],
  );
  const pickerOpen = query !== null && rows.length > 0;

  /** Re-read the caret's mention query after any edit or caret move. */
  function syncQuery(text: string, caret: number | null) {
    if (!mentionables || caret === null) {
      closePicker();
      return;
    }
    // Code regions are masked so an `@` inside backticks never opens the picker
    // (a supplied mention would survive revalidation, since the host's code
    // mask only applies to its own extraction). Masking preserves offsets, so
    // the range this returns is valid against the raw `text`.
    const next = activeMentionQuery(stripCodeRegions(text), caret, aliases);
    setQuery(next);
    setActiveRow(0);
  }

  function onChange(e: React.ChangeEvent<HTMLTextAreaElement>) {
    const text = e.target.value;
    setDraft(text);
    // Trailing the text, so a mention whose span was edited away goes with it.
    // The previous draft disambiguates which of two same-text mentions the
    // edit deleted — without it, deleting the second `@Sam @Sam` re-anchors
    // the deleted Sam onto the survivor and pings the wrong person.
    setMentions((current) => reconcileMentions(text, current, draft, e.target.selectionStart));
    setOutsideWarning(null);
    syncQuery(text, e.target.selectionStart);
    onTyping?.();
  }

  function pick(entry: Mentionable) {
    const el = input.current;
    if (!query || !el) return;
    const caret = el.selectionStart ?? query.start;
    // Replace the whole `@name`, not just the part before the caret. Moving the
    // caret into an existing `@engineer` (the onSelect path) opens the picker
    // with `query.query` = "eng"; replacing only that would leave `@ceo ineer`.
    // A selection the reader made is honoured, so replacing spans what they
    // selected when the selection reaches past the token.
    const selectionEnd = Math.max(el.selectionEnd ?? caret, caret);
    const tokenEnd = activeMentionEnd(
      draft,
      caret,
      query.start + 1,
      aliases ?? new Set<string>(),
    );
    const range = { start: query.start, end: Math.max(selectionEnd, tokenEnd) };
    const result = insertMention(draft, range, entry);
    // `insertMention` is typed to always return a result, but guard anyway:
    // it is a module-boundary call and a future refactor must not let a
    // malformed range turn a pick into a dereference of `undefined`.
    if (!result) return;
    setDraft(result.text);
    // A pick replaces the token under the caret, so any mention the range
    // touched is gone from the draft. Drop those before reconciling, or the
    // replaced identity re-anchors onto a same-text duplicate elsewhere —
    // replacing the picked first `@Sam` in `@Sam then @Sam` with `@engineer`
    // would otherwise move Sam onto the second, hand-typed span.
    setMentions((current) =>
      reconcileMentions(result.text, [
        ...mentionsOutsideRange(current, range),
        result.mention,
      ]),
    );
    setOutsideWarning(null);
    closePicker();
    // After React repaints, same as `wrap` below.
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(result.caret, result.caret);
    });
  }

  // Deletes a staged attachment that never made it onto a sent message (issue
  // #1682, codex review finding): removing it, replacing it with a fresh
  // pick, or leaving the composer all drop the local reference while the
  // upload stays live on the server, charged against the workspace quota
  // forever. Centralized here so every one of those paths — not just the
  // Remove button — clears the same way.
  function clearPending(nodeId?: string) {
    // The node was created under the company whose delete is stored beside it
    // (see `PendingAttachment`) — never the latest callback, which may already
    // be bound to a scope this node does not belong to.
    const removed = nodeId
      ? pendingRef.current.filter((item) => item.reference.nodeId === nodeId)
      : pendingRef.current;
    for (const item of removed) item.delete?.(item.reference.nodeId);
    pendingRef.current = nodeId
      ? pendingRef.current.filter((item) => item.reference.nodeId !== nodeId)
      : [];
    setPending(pendingRef.current);
  }

  // Unmounting still holding a pending attachment (closing the thread panel,
  // switching channels) is the same leak as clicking Remove — clean it up on
  // the way out. Reads through the ref rather than `pending` because an
  // unmount-only cleanup must not re-run on every state change. The delete
  // comes from the stored pair, so a company switch while the composer stayed
  // mounted still frees the node in the company that owns it.
  useEffect(() => {
    return () => {
      for (const item of pendingRef.current) item.delete?.(item.reference.nodeId);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- unmount-only, see above
  }, []);

  function send() {
    const text = draft.trim();
    // A message must carry text; an attachment rides an operator's words, it is
    // not a message on its own. Also held back while the upload is mid-flight,
    // so a send never references bytes that have not landed.
    if (!text || disabled || uploading) return;
    // The trim can shift every span, so the list is re-anchored against exactly
    // what is being sent — never against the untrimmed draft.
    let sending = reconcileMentions(text, mentions);
    // A previously selected mention can be wrapped in Markdown code after the
    // picker closes. The host's fallback extractor masks code, so supplied
    // mentions must obey the same rule rather than bypassing it.
    const masked = stripCodeRegions(text);
    sending = sending.filter(
      (m) =>
        masked.slice(m.offset, m.offset + m.text.length) ===
        text.slice(m.offset, m.offset + m.text.length),
    );
    // A mention completed by hand (`@ceo ` — the query closed on the finished
    // name) never entered `mentions`. When anything was picked, the host uses
    // the supplied list exclusively, so sending just the picks would silently
    // skip the typed one and the person would never be notified. Resolve every
    // span the directory can name and send the union, keeping the picker's
    // explicit targets for names the host's extraction would refuse as
    // ambiguous.
    if (mentionables) {
      for (const m of resolvableMentions(text, mentionables)) {
        if (!sending.some((s) => s.text === m.text && s.offset === m.offset)) {
          sending = [...sending, m];
        }
      }
      sending = reconcileMentions(text, sending);
    }
    // On first send with outside-channel mentions, warn instead of sending.
    // The directory rows carry each desk's membership, so a desk mention is
    // judged by its blast radius, not skipped because its target is a desk.
    const outside = mentionsOutsideChannel(sending, channelMemberIds, mentionables);
    if (outside.length > 0 && !outsideWarning) {
      setOutsideWarning(outside);
      return;
    }
    setOutsideWarning(null);
    setMentions([]);
    closePicker();
    setDraft("");
    // The pair, not just the node id: the reconciliation below must free a
    // node the send failed to claim through the delete bound to the company
    // that owns it (see `PendingAttachment`), whichever scope is current now.
    const inFlight = pendingRef.current;
    const result = onSend(
      text,
      deliverableChoice ? intent : undefined,
      pending.length > 0 ? pending.map((item) => item.reference) : undefined,
      // Preserve absent-versus-empty: a loaded directory that resolves no
      // spans intentionally sends [] to suppress host fallback extraction.
      mentionables ? sending : undefined,
    );
    // Back to unselected, not to a default (issue #984).
    setIntent(undefined);
    // The reference is cleared from the composer's own state immediately —
    // the shell's optimistic bubble already carries it — WITHOUT deleting the
    // node yet (unlike `clearPending`): whether it is actually claimed is
    // still pending on `result` below.
    pendingRef.current = [];
    setPending([]);
    setAttachError(undefined);
    // If the caller reports whether the send journaled (issue #1682, codex
    // review round 4), clean up an attachment only on an explicit `false` —
    // the host definitely never saw it. `undefined` (ambiguous: a network
    // drop, a timeout — the message may have landed anyway) and `true`
    // (definitely landed) both leave the node alone. A caller that returns
    // `void` has nothing to reconcile here.
    if (inFlight.length > 0 && result instanceof Promise) {
      void result.then((sent) => {
        if (sent === false) {
          for (const item of inFlight) item.delete?.(item.reference.nodeId);
        }
      });
    }
  }

  /** Upload picked or dropped files sequentially and stage every successful one. */
  function addFiles(files: File[]): Promise<void> {
    if (!uploadAttachment || files.length === 0) return Promise.resolve();
    const upload = uploadAttachment;
    uploadingCountRef.current += 1;
    setUploading(true);
    const batch = async () => {
      try {
        const room = Math.max(0, 20 - pendingRef.current.length);
        const selected = files.filter((file) => file.size > 0).slice(0, room);
        if (selected.length === 0) {
          setAttachError(
            room === 0
              ? "A message can carry at most 20 files."
              : "Empty files and folders can't be attached.",
          );
          return;
        }
        setAttachError(undefined);
        for (const file of selected) {
          const reference = await upload(file);
          if (!mountedRef.current || scopeDeleteRef.current !== deleteAttachment) {
            deleteAttachment?.(reference.nodeId);
            continue;
          }
          const staged: PendingAttachment = { reference, delete: deleteAttachment };
          pendingRef.current = [...pendingRef.current, staged];
          setPending(pendingRef.current);
        }
        if (files.length > selected.length) {
          setAttachError("Some files were skipped: messages accept 20 non-empty files.");
        }
      } catch (err) {
        if (!mountedRef.current) return;
        // The filename is operator content — the message says an upload failed
        // without echoing what it was called.
        setAttachError(err instanceof Error ? err.message : "Couldn't attach that file.");
      } finally {
        uploadingCountRef.current -= 1;
        if (mountedRef.current) setUploading(uploadingCountRef.current > 0);
      }
    };
    const queued = uploadQueueRef.current.then(batch, batch);
    // A failed batch must not block later paste/drop events. `batch` handles
    // upload errors itself, but retain this rejection handler for future
    // changes that throw before its cleanup.
    uploadQueueRef.current = queued.catch(() => undefined);
    return queued;
  }

  async function onPickFile(e: React.ChangeEvent<HTMLInputElement>) {
    const files = Array.from(e.target.files ?? []);
    e.target.value = "";
    await addFiles(files);
  }

  function onPaste(e: React.ClipboardEvent<HTMLTextAreaElement>) {
    if (disabled || !uploadAttachment) return;
    const files = Array.from(e.clipboardData.items)
      .filter((item) => item.kind === "file")
      .map((item) => item.getAsFile())
      .filter((file): file is File => file !== null);
    if (files.length === 0) return;
    // A screenshot copied from the clipboard is a real attachment. Prevent the
    // browser from inserting a useless object replacement character or local
    // filename, while ordinary text paste keeps its native behaviour.
    e.preventDefault();
    void addFiles(files);
  }

  function carriesFiles(event: React.DragEvent): boolean {
    return !!uploadAttachment && !disabled && Array.from(event.dataTransfer.types).includes("Files");
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    // While the picker is open it owns these keys. Enter in particular PICKS
    // and does not send — a person mid-`@name` is choosing somebody, not
    // finishing a message, and sending there is unrecoverable.
    if (pickerOpen) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActiveRow((i) => (i + 1) % rows.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setActiveRow((i) => (i - 1 + rows.length) % rows.length);
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const entry = rows[activeRow];
        // `rows` is non-empty here by `pickerOpen`, but a render between this
        // keydown and the picker closing can shrink the list, leaving
        // `activeRow` past its end. Picking `undefined` would throw inside
        // `insertMention`, so resolve the row and guard before calling.
        if (entry) pick(entry);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        closePicker();
        return;
      }
    }
    // An Enter that confirms an IME candidate finishes the composition — it
    // does not send the half-written draft. isComposing alone misses Safari,
    // whose completing Enter arrives after compositionend (WebKit bug
    // 165004), so the shared guard covers all three signals.
    if (e.key === "Enter" && !e.shiftKey && !imeEnter.confirmsComposition(e)) {
      e.preventDefault();
      send();
    }
  }

  /**
   * Types an `@` at the caret and lets the ordinary path take over, rather than
   * opening the picker directly: one code path decides when a picker is open,
   * so the `+` menu's "Mention someone" and the keyboard can never disagree.
   */
  function startMention() {
    const el = input.current;
    if (!el) return;
    const at = el.selectionStart ?? draft.length;
    // A separator first when the caret is mid-word, or the `@` would land
    // inside another token and open nothing.
    const lead = at > 0 && !/[\s([{]/.test(draft[at - 1] ?? " ") ? " " : "";
    const next = `${draft.slice(0, at)}${lead}@${draft.slice(at)}`;
    const caret = at + lead.length + 1;
    // The insertion shifts every recorded mention at/after it and breaks the
    // literal of one it lands inside. Reconcile now, as `onChange` does for a
    // keystroke, so the stale span cannot re-anchor onto a same-text duplicate
    // at send time.
    setMentions((current) => reconcileMentions(next, current, draft, caret));
    setDraft(next);
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(caret, caret);
      syncQuery(next, caret);
    });
  }

  return (
    <div
      // 8px from the bottom and sides in the channel pane: the floating
      // sidebar card keeps 8px from the window's edges, and the composer is
      // the one box that runs along the same bottom edge, so the two share it.
      className={cn("shrink-0", compact ? "px-4 pb-3" : "px-2 pb-2")}
      // The guided tour spotlights the channel composer. The thread panel's
      // compact copy stays unlabelled so the tour can't anchor on the wrong one.
      data-tour={compact ? undefined : "chat-composer"}
    >
      <div
        className={cn(
          // Rounded to a pill while it is one line; the radius holds as it
          // grows, so a multi-line draft reads as the same control.
          // The transcript's column (`max-w-4xl`, centred), and a fill and edge
          // of its own: `bg-muted` with a 15% ink border, so the box reads as
          // the place to type rather than as white on a white page.
          "relative mx-auto w-full max-w-4xl overflow-hidden rounded-3xl border border-foreground/15 bg-muted/60 shadow-sm",
          dragDepth > 0 && "border-primary ring-2 ring-primary/40",
        )}
        onDragEnter={(event) => {
          if (!carriesFiles(event)) return;
          event.preventDefault();
          setDragDepth((depth) => depth + 1);
        }}
        onDragOver={(event) => {
          if (!carriesFiles(event)) return;
          event.preventDefault();
          event.dataTransfer.dropEffect = "copy";
        }}
        onDragLeave={(event) => {
          if (!carriesFiles(event)) return;
          setDragDepth((depth) => Math.max(0, depth - 1));
        }}
        onDrop={(event) => {
          if (!carriesFiles(event)) return;
          event.preventDefault();
          setDragDepth(0);
          void addFiles(Array.from(event.dataTransfer.files));
        }}
      >
        {dragDepth > 0 && (
          <div className="pointer-events-none absolute inset-0 z-20 flex items-center justify-center rounded-3xl border-2 border-dashed border-primary bg-card/90 text-sm font-medium text-primary">
            Drop files to attach them
          </div>
        )}
        {pickerOpen && (
          <MentionPicker
            entries={rows}
            active={activeRow}
            onPick={pick}
            onHover={setActiveRow}
          />
        )}
        {/* The staged attachment (issue #1682), shown above the box the moment
            its upload lands and cleared on send or removal. One chip in v1. */}
        {pending.length > 0 && (
          <div className="flex flex-wrap gap-1.5 border-b px-3 py-1.5">
            {pending.map((item) => (
              <span key={item.reference.nodeId} className="flex min-w-0 max-w-full items-center gap-1.5 rounded-md bg-muted px-2 py-1">
                <Paperclip className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
                <span className="min-w-0 truncate text-xs font-medium" title={item.reference.name}>
                  {item.reference.name}
                </span>
                <span className="shrink-0 text-2xs text-muted-foreground">
                  {formatBytes(item.reference.size)}
                </span>
                <Button
                  variant="ghost"
                  size="icon"
                  className="size-5 shrink-0 text-muted-foreground"
                  aria-label={`Remove ${item.reference.name}`}
                  title="Remove attachment"
                  onClick={() => clearPending(item.reference.nodeId)}
                >
                  <X className="size-3" />
                </Button>
              </span>
            ))}
          </div>
        )}
        {attachError && (
          <p role="alert" className="border-b px-3 py-1.5 text-2xs text-destructive">
            {attachError}
          </p>
        )}

        {outsideWarning && (
          <p
            role="alert"
            className="flex items-center gap-1.5 border-b bg-warning/10 px-3 py-1.5 text-xs text-muted-foreground"
          >
            <svg className="size-3.5 shrink-0" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
              <path d="M12 9v4m0 4h.01M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z" />
            </svg>
            <span className="min-w-0">
              <span className="font-medium">{outsideWarning.join(", ")}</span>
              {" "}can't see this channel — send again to notify anyway
            </span>
          </p>
        )}
        {deliverableChoice && !compact && !COMPOSER_INTENT_HIDDEN && (
          <div
            className="flex items-center gap-0.5 border-b px-2 py-1"
            role="group"
            // Issue #1152: the group asks what the message is for, not only
            // what it should produce — "Just chatting" produces nothing.
            aria-label="What this message is for"
          >
            {(
              [
                { value: "chat", label: "Just chatting", title: "Chat without automatically creating a task." },
                { value: "once", label: "Do it once", title: "Ask the team to do this once." },
                { value: "workflow", label: "Build me the automation", title: "Turn this into a repeating workflow." },
              ] as const
            ).map((option) => (
              <button
                key={option.value}
                type="button"
                aria-pressed={intent === option.value}
                onClick={() => setIntent(option.value)}
                data-testid={`composer-deliverable-${option.value}`}
                title={option.title}
                className={cn(
                  "rounded-md px-2 py-1 text-2xs font-medium transition-colors",
                  intent === option.value
                    ? "bg-primary/10 text-brand-700 dark:text-brand-300"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                {option.label}
              </button>
            ))}
          </div>
        )}

        {/* One line: a `+` for everything that is not typing, the input, and
            Send. The formatting row, the `@` and paperclip glyphs and the
            autonomy pill that shared a toolbar under a two-row box are gone or
            folded into the `+` menu; Markdown still renders when typed, and
            `@` still opens the picker. The input grows only when Shift+Enter
            adds a line, up to `max-h-48`, then scrolls. */}
        <div className="flex items-end gap-1.5 p-1.5">
          {uploadAttachment && (
            <input
              ref={fileInput}
              type="file"
              multiple
              className="hidden"
              aria-hidden
              tabIndex={-1}
              onChange={(e) => void onPickFile(e)}
            />
          )}
          <DropdownMenu>
            <DropdownMenuTrigger
              aria-label="Add to message"
              title="Add to message"
              className="flex size-9 shrink-0 items-center justify-center rounded-full bg-background text-muted-foreground shadow-xs transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none aria-expanded:bg-accent aria-expanded:text-foreground"
            >
              {uploading ? <Loader2 className="size-4 animate-spin" /> : <Plus className="size-4" />}
            </DropdownMenuTrigger>
            <DropdownMenuContent side="top" align="start" className="w-auto min-w-44">
              {/* Attaching (issue #1682) is offered exactly where it works —
                  a composer given an `uploadAttachment`. */}
              {uploadAttachment && (
                <DropdownMenuItem
                  disabled={disabled || uploading}
                  onClick={() => fileInput.current?.click()}
                >
                  <Paperclip className="size-4" aria-hidden /> Attach files
                </DropdownMenuItem>
              )}
              <DropdownMenuItem onClick={startMention}>
                <AtSign className="size-4" aria-hidden /> Mention someone
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
          {/* A native textarea rather than the design-system one: the composer
              needs a ref for the caret, and `Textarea` is a plain function
              component (React 18 — no ref forwarding). */}
          <textarea
            ref={input}
            value={draft}
            onChange={onChange}
            onPaste={onPaste}
            onKeyDown={onKeyDown}
            onCompositionEnd={imeEnter.onCompositionEnd}
            // A click or an arrow can move the caret into (or out of) an
            // existing `@name` without changing the text, so the query is
            // re-read on selection changes too, not only on edits.
            onSelect={(e) => {
              const el = e.currentTarget;
              syncQuery(el.value, el.selectionStart);
            }}
            onBlur={closePicker}
            role="combobox"
            aria-autocomplete="list"
            aria-controls={pickerOpen ? "mention-picker" : undefined}
            aria-activedescendant={pickerOpen ? `mention-option-${activeRow}` : undefined}
            aria-expanded={pickerOpen}
            aria-label={placeholder}
            placeholder={placeholder}
            rows={1}
            // `outline-none` drops the native outline unconditionally, and no
            // ring replaces it, by explicit operator request: the composer
            // stays quiet on focus, full stop (a `focus-visible` ring was
            // tried here to keep a CodeRabbit-flagged keyboard-focus
            // indicator, but WebKit/most UAs treat a text field's
            // `:focus-visible` as matching on pointer focus too, not just
            // keyboard, so it fired on every click anyway).
            className="field-sizing-content max-h-48 min-h-9 flex-1 resize-none rounded-sm bg-transparent px-1 py-2 text-sm leading-5 outline-none placeholder:text-muted-foreground"
          />
          <Button
            size="icon"
            className="size-9 shrink-0 rounded-full"
            onClick={send}
            disabled={disabled || !draft.trim()}
            aria-label="Send"
          >
            <ArrowUp className="size-4" />
          </Button>
        </div>
      </div>
      {/* No "Enter to send · Shift+Enter for a new line" line under the box: it
          cost a row of height under every transcript to restate the one
          convention every chat input shares. */}
    </div>
  );
}
