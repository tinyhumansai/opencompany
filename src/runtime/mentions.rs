//! Turning `@text` into somebody: the pure half of chat mentions.
//!
//! Everything here is IO-free. The caller does the one roster read and the one
//! user read and hands the results in, which is what lets the same code answer
//! for the console, for the API, and for a turn — and what keeps it in
//! `runtime/` rather than in the harness.
//!
//! # Why this is not in `harness/built_in/`
//!
//! `src/harness/built_in/` compiles only under the `openhuman` feature, and
//! mention resolution has to work on the hosted (default) build too — the chat
//! POST validates mentions there whether or not a brain is compiled in. This is
//! the same reason [`desk_lead`](crate::runtime::delegation_tools::desk_lead)
//! was lifted out of the harness: routing helpers that a non-harness path needs
//! do not live behind a harness feature.
//!
//! # The shape of the problem
//!
//! ```text
//! body text ──► strip_code_regions ──► extract_with_known ──┐
//!                                                            ├─► normalize ─► Vec<Mention>
//! client-supplied mentions (the picker's answer) ────────────┘
//!                            │
//!                            └─► revalidate (drops nothing, demotes to `quiet`)
//! ```
//!
//! Two entry points, deliberately:
//!
//! * The **picker** knows the caret, the query, and the row a human clicked.
//!   That is the only place ambiguity can be resolved *correctly*, by asking.
//!   Its answer arrives as structured mentions and is re-validated, never
//!   re-derived.
//! * **Extraction** is the fallback for everything that has no picker — `curl`,
//!   the API, an older console, and every agent-authored reply. It resolves
//!   what it can and leaves the rest as literal text.
//!
//! # Two rules that are load-bearing
//!
//! **Never guess a ping.** An `@name` matching two people resolves to nobody
//! and stays literal text. Silently picking the first match is how a message
//! meant for one colleague reaches another, and there is no way for either of
//! them to notice.
//!
//! **Never render a chip that does not resolve.** The renderer highlights the
//! spans this module returned and nothing else, so an `@word` that matched no
//! one is drawn as the plain text it is. A chip is a claim that somebody was
//! notified; drawing one where nobody was is worse than drawing nothing.

use std::collections::{HashMap, HashSet};

use crate::ports::types::{Actor, ActorKind, CompanyRecord, MENTION_CAP, Mention, MentionTarget};
use crate::ports::users::UserRecord;

/// The typed aliases that stand in for `@everyone`.
///
/// Three spellings because three products taught people three habits, and a
/// person who types the one their last tool used should not silently address
/// nobody. They are equivalent: all three resolve to
/// [`MentionTarget::Everyone`].
pub const EVERYONE_ALIASES: [&str; 3] = ["everyone", "channel", "here"];

/// One mentionable thing and every spelling that reaches it.
///
/// Built once per resolution by [`directory`] and used for **both** the
/// name-to-target map and the render spans, so a chip and the person it points
/// at cannot drift apart — the failure `block/buzz` calls out, where a display
/// name that renders but never resolves leaves a chip pointing at nobody.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionAlias {
    /// What this alias resolves to.
    pub target: MentionTarget,
    /// Every spelling that reaches [`Self::target`], lowercased. May contain
    /// spaces — a human's display name is a legitimate alias and is often two
    /// words.
    pub aliases: Vec<String>,
}

/// Every mentionable thing in a company, with all its spellings.
///
/// The union is agent id ∪ agent display name ∪ desk id ∪ desk name ∪ user
/// label ∪ user slug ∪ the [`EVERYONE_ALIASES`].
///
/// # Ambiguity is preserved, not resolved
///
/// An alias shared by two targets appears on both, and
/// [`extract_with_known`] then refuses it. Deduplicating here — keeping the
/// first target for a repeated name — would bury the collision at the one point
/// where it is still cheap to notice.
pub fn directory(record: &CompanyRecord, users: &[UserRecord]) -> Vec<MentionAlias> {
    let mut out = Vec::new();

    // Teammates. The id is the authored, typable handle (`engineer`); the
    // display name is what an operator who never read the manifest will type.
    // Read through `effective_agents()` rather than the raw manifest so an
    // operator-renamed teammate is addressable by the name actually shown —
    // `id = "ceo", name = "Ada"` must resolve `@Ada`, not just `@ceo`.
    for agent in record.effective_agents() {
        let mut aliases = vec![agent.id.to_lowercase()];
        if let Some(name) = agent.name.as_deref() {
            let name = name.to_lowercase();
            if !aliases.contains(&name) {
                aliases.push(name);
            }
        }
        out.push(MentionAlias {
            target: MentionTarget::Agent { id: agent.id },
            aliases,
        });
    }
    for overlay in &record.overlay_agents {
        if record.is_retired(&overlay.id) {
            continue;
        }
        // An operator-added teammate carries both: a slug id since #686, and a
        // display name which is the only handle a teammate added before that
        // has. Both spellings reach the same roster id.
        let mut aliases = vec![overlay.id.to_lowercase()];
        let name = overlay.name.to_lowercase();
        if !aliases.contains(&name) {
            aliases.push(name);
        }
        out.push(MentionAlias {
            target: MentionTarget::Agent {
                id: overlay.id.clone(),
            },
            aliases,
        });
    }

    // Desks, by id and by name — the two spellings `resolve_desk_id` already
    // accepts, so `@#engineering` and `@#Engineering` behave the way
    // `delegate_to_desk` does.
    for chat in &record.manifest.group_chats {
        let mut aliases = vec![chat.id.to_lowercase()];
        let name = chat.name.to_lowercase();
        if !aliases.contains(&name) {
            aliases.push(name);
        }
        out.push(MentionAlias {
            target: MentionTarget::Desk {
                id: chat.id.clone(),
            },
            aliases,
        });
    }
    for desk in &record.overlay_desks {
        let mut aliases = vec![desk.id.to_lowercase()];
        let name = desk.name.to_lowercase();
        if !aliases.contains(&name) {
            aliases.push(name);
        }
        out.push(MentionAlias {
            target: MentionTarget::Desk {
                id: desk.id.clone(),
            },
            aliases,
        });
    }

    // People. Both their label and a typable slug of it, because a label is
    // frequently two words and `@Jane Doe` is only reachable by the
    // longest-first matcher, while `@jane-doe` is what somebody typing fast
    // will produce.
    for (user, slug) in users.iter().zip(user_slugs(users)) {
        let label = user_label(user).to_lowercase();
        let mut aliases = vec![label.clone()];
        if slug != label {
            aliases.push(slug);
        }
        out.push(MentionAlias {
            target: MentionTarget::User {
                id: user.id.clone(),
            },
            aliases,
        });
    }

    out.push(MentionAlias {
        target: MentionTarget::Everyone,
        aliases: EVERYONE_ALIASES.iter().map(|a| a.to_string()).collect(),
    });

    out
}

/// How a person is named to other members of their company.
///
/// Display name, else one derived from their login identity, else `"someone"`
/// — the same ladder [`author_labels`](crate::server::chat_history) walks, and
/// deliberately the same one: a mention chip that read differently from the
/// author line above it on the very same message would look like two people.
/// It is also the same rule `UserRecord::display_label` uses everywhere else a
/// person is named, so the identity a member sees in the profile pane is the
/// one they see on a mention chip.
///
/// Never the full identity. An email address is not a handle, and handing one
/// to every member of a company so they can @ each other would leak it.
pub fn user_label(user: &UserRecord) -> String {
    user.display_label()
        .unwrap_or_else(|| "someone".to_string())
}

/// A typable alias for each user, in the order given, disambiguated so no two
/// are equal.
///
/// # This is not a handle, and is never stored
///
/// A human in this system has no handle — only a display name, which can change
/// and is not unique. Minting one and persisting it would create a second
/// identity to keep in sync, and a rename would orphan it. So a mention is
/// carried by **user id plus byte span**, and this exists only to give the
/// picker something short to type and the extraction path a second spelling to
/// match. It is recomputed on every read, which is precisely why a rename can
/// never strand anything.
///
/// Collisions get `-2`, `-3`, … in the order the users are passed. Callers
/// therefore pass the list in a stable order — id order — so the suffix a
/// person gets does not move under them between two reads.
pub fn user_slugs(users: &[UserRecord]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    // Every slug actually handed out so far, natural or generated. A natural
    // label can already carry a `-2`-shaped suffix (`"Sam-2"` is a real
    // display name, not a disambiguation this function made up), so counting
    // per base alone can mint the same slug twice — `"Sam"`, `"Sam"`,
    // `"Sam-2"` would otherwise emit `sam`, `sam-2`, `sam-2`. Checking against
    // every emitted slug, not just this base's own counter, is what catches
    // that collision.
    let mut emitted: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(users.len());
    for user in users {
        let mut base = mention_slug(&user_label(user));
        if base.is_empty() {
            // A symbol-only display name ("🙂", "!!!") slugs to nothing, and an
            // empty alias would match every `@` — but dropping the alias makes
            // the person unmentionable while the picker still offers a row
            // (`mentionableText` falls back to the label, and the host refuses
            // the span because `opens_mention` needs a word char after `@`).
            // Fall back to the email local part, the same handle `user_label`
            // already uses when there is no display name, and then to the id,
            // which is guaranteed non-empty and typable.
            base = mention_slug(user.email.split('@').next().unwrap_or_default().trim());
            if base.is_empty() {
                base = user.id.clone();
            }
        }
        loop {
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            let candidate = if *count == 1 {
                base.clone()
            } else {
                format!("{base}-{count}")
            };
            if emitted.insert(candidate.clone()) {
                out.push(candidate);
                break;
            }
        }
    }
    out
}

/// Lowercase, non-alphanumeric runs collapsed to a single `-`, trimmed.
///
/// `"Jane Doe"` becomes `jane-doe`; `"Ana  M. Ruiz"` becomes `ana-m-ruiz`. A
/// label with nothing alphanumeric in it yields an empty string, which
/// [`directory`] then never offers as an alias, because an empty alias would
/// match every `@`.
pub fn mention_slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut pending_dash = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

/// Blanks fenced and inline code spans, **preserving byte offsets**.
///
/// Every byte of a code region is replaced with a space, so the result is the
/// same length as the input and an offset computed against it indexes the
/// original correctly. Stripping the regions instead would shift every
/// subsequent match and silently mis-place the chips.
///
/// Handles ``` fences (and `~~~`), and backtick spans of any run length, per
/// CommonMark's rule that a span closes on a backtick run of equal length.
pub fn strip_code_regions(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = bytes.to_vec();
    let mut i = 0usize;
    let mut at_line_start = true;

    while i < bytes.len() {
        let ch = bytes[i];

        if at_line_start {
            // A fence: three or more backticks or tildes at the start of a
            // line, closed by a run of the same character at least as long.
            if ch == b'`' || ch == b'~' {
                let run = run_len(bytes, i, ch);
                if run >= 3 {
                    let mut j = line_end(bytes, i);
                    let close = loop {
                        if j >= bytes.len() {
                            break bytes.len();
                        }
                        let line_start = j + 1;
                        if line_start >= bytes.len() {
                            break bytes.len();
                        }
                        let indent = line_start + leading_spaces(bytes, line_start);
                        // CommonMark: a closing fence may be followed only by
                        // spaces or tabs (or a CR in a CRLF line ending), never
                        // by text. A line like ```not-a-close stays inside the
                        // block, so blanking must not stop there and unmask a
                        // later `@` the renderer still shows as code. The same
                        // line also has to be the same character
                        // (`bytes[indent] == ch`) and at least as long, which
                        // `run_len` already enforces.
                        let close_run = run_len(bytes, indent, ch);
                        let after = indent + close_run;
                        if indent < bytes.len()
                            && bytes[indent] == ch
                            && close_run >= run
                            && bytes[after..line_end(bytes, indent)]
                                .iter()
                                .all(|b| *b == b' ' || *b == b'\t' || *b == b'\r')
                        {
                            break line_end(bytes, indent);
                        }
                        j = line_end(bytes, line_start);
                    };
                    blank(&mut out, i, close.min(bytes.len()));
                    i = close.min(bytes.len());
                    at_line_start = true;
                    continue;
                }
            }
        }

        if ch == b'`' {
            let run = run_len(bytes, i, b'`');
            // Scan for a closing run of exactly this length.
            let mut j = i + run;
            let mut close = None;
            while j < bytes.len() {
                if bytes[j] == b'`' {
                    let r = run_len(bytes, j, b'`');
                    if r == run {
                        close = Some(j + r);
                        break;
                    }
                    j += r;
                } else {
                    j += 1;
                }
            }
            if let Some(end) = close {
                blank(&mut out, i, end);
                i = end;
                at_line_start = false;
                continue;
            }
            // Unclosed: not a code span at all, so leave it be. An unbalanced
            // backtick must not swallow the rest of the message.
        }

        at_line_start = ch == b'\n';
        i += 1;
    }

    // Only ASCII bytes were replaced, and only with ASCII spaces, so every
    // multi-byte sequence outside a code region is untouched and the result is
    // still valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

fn blank(out: &mut [u8], from: usize, to: usize) {
    let to = to.min(out.len());
    for b in &mut out[from..to] {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

fn run_len(bytes: &[u8], from: usize, ch: u8) -> usize {
    let mut n = 0;
    while from + n < bytes.len() && bytes[from + n] == ch {
        n += 1;
    }
    n
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn leading_spaces(bytes: &[u8], from: usize) -> usize {
    let mut n = 0;
    while from + n < bytes.len() && bytes[from + n] == b' ' {
        n += 1;
    }
    n
}

/// Whether an `@` at `idx` opens a mention.
///
/// The condition that keeps `jane@acme.com` from reading as a mention of
/// `acme`: an `@` counts only at the start of the text or after whitespace or
/// an opening bracket, and only when something word-like follows it.
fn opens_mention(text: &str, bytes: &[u8], idx: usize) -> bool {
    if bytes[idx] != b'@' {
        return false;
    }
    let before_ok = match idx.checked_sub(1) {
        None => true,
        Some(prev) => matches!(
            bytes[prev],
            b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[' | b'{'
        ),
    };
    // `@#engineering` is the documented desk spelling (`MentionTarget::Desk`'s
    // own doc comment). The `#` is not itself word-like, so it needs its own
    // branch: either the char right after `@` is word-like, or it is `#` and
    // the char after THAT is — an `@#` with nothing nameable following it
    // opens nothing.
    //
    // Unicode-aware (`char::is_alphanumeric`), not the ASCII-only byte
    // predicate this used to be: a label like "Élodie" is a real alias
    // `directory` offers verbatim (see its people loop), and `@Élodie` must
    // open a mention exactly as `@engineer` does. `idx` is `@`'s byte offset
    // and `@` is one byte, so `idx + 1` is always a char boundary to slice
    // from; that only fails if `idx + 1` also needs a second character (the
    // `@#` arm), which re-slices from `idx + 1` rather than assuming `#` is
    // one byte at some other fixed offset — it is, but the slice makes that
    // true by construction instead of by charset assumption.
    let mut after_chars = text[idx + 1..].chars();
    let after_ok = match after_chars.next() {
        Some('#') => after_chars
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_'),
        Some(c) => c.is_alphanumeric() || c == '_',
        None => false,
    };
    before_ok && after_ok
}

/// Whether the byte at `idx` closes a mention cleanly.
///
/// End of text, whitespace, or ordinary trailing punctuation — so
/// `@engineer,` and `@engineer.` resolve, and `@engineerish` does not resolve
/// to `engineer`.
fn closes_mention(bytes: &[u8], idx: usize) -> bool {
    match bytes.get(idx) {
        None => true,
        Some(b) => matches!(
            b,
            b' ' | b'\t'
                | b'\n'
                | b'\r'
                | b','
                | b';'
                | b'.'
                | b'!'
                | b'?'
                | b':'
                | b')'
                | b']'
                | b'}'
                | b'\''
                | b'"'
        ),
    }
}

/// Every `@name` candidate in `text`, as `(byte offset of the '@', the name)`.
///
/// The single-word pass: the name charset is `[A-Za-z0-9._-]`, which is what a
/// slug or a roster id looks like. Multi-word display names are the business of
/// [`extract_with_known`], which needs the directory to know where a name ends.
///
/// Does **not** strip code regions — pass [`strip_code_regions`]'s output if
/// that is wanted, which every caller here does.
pub fn extract_at_names(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if opens_mention(text, bytes, i) {
            let mut j = i + 1;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || matches!(bytes[j], b'_' | b'-' | b'.'))
            {
                j += 1;
            }
            // Trailing `.` is sentence punctuation far more often than part of
            // a name, so it is not eaten.
            let mut end = j;
            while end > i + 1 && bytes[end - 1] == b'.' {
                end -= 1;
            }
            if end > i + 1 {
                out.push((i, text[i + 1..end].to_string()));
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    out
}

/// Resolve `text` against `dir`, returning one [`Mention`] per `@` that names
/// exactly one thing.
///
/// # Longest alias wins
///
/// Aliases are tried longest-first, so a company with both `Ann` and `Ann Lee`
/// resolves `@Ann Lee` to Ann Lee rather than to Ann with a stray `Lee` after
/// it. Without this the shorter name always wins, because it matches first.
///
/// # An ambiguous name resolves to nothing
///
/// When one alias reaches two targets — two people called "Sam", a desk and a
/// teammate sharing a name — the span is skipped entirely and stays literal
/// text. See the module docs: never guess a ping.
///
/// Offsets in the returned mentions index `text`, so pass the **original**
/// body, not a stripped copy, when the offsets have to line up with what a
/// reader sees. Callers that want code regions ignored should mask with
/// [`strip_code_regions`] first, which preserves offsets exactly so both hold.
pub fn extract_with_known(text: &str, dir: &[MentionAlias]) -> Vec<Mention> {
    // Longest first so a name that prefixes another cannot claim it.
    let mut by_alias: Vec<(&str, &MentionTarget)> = Vec::new();
    for entry in dir {
        for alias in &entry.aliases {
            if !alias.is_empty() {
                by_alias.push((alias.as_str(), &entry.target));
            }
        }
    }
    by_alias.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(b.0)));

    let bytes = text.as_bytes();
    let lowered = text.to_lowercase();
    // `to_lowercase` can change byte length for non-ASCII, which would
    // invalidate every offset. Fall back to a byte-wise ASCII fold, which
    // cannot, and which is all the aliases need.
    let lowered = if lowered.len() == text.len() {
        lowered
    } else {
        text.chars()
            .map(|c| {
                if c.is_ascii() {
                    c.to_ascii_lowercase()
                } else {
                    c
                }
            })
            .collect()
    };

    let mut out: Vec<Mention> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if !opens_mention(text, bytes, i) {
            i += 1;
            continue;
        }
        // `@#engineering` is the desk-only spelling `opens_mention` also
        // accepts (see its doc comment): the `#` is not part of any alias in
        // `dir`, so it is consumed here and the match is narrowed to desk
        // targets only, rather than asking every alias to carry a `#` twin.
        let (after, desk_only) = if bytes.get(i + 1) == Some(&b'#') {
            (i + 2, true)
        } else {
            (i + 1, false)
        };
        let mut matched: Option<(usize, &MentionTarget)> = None;
        let mut ambiguous = false;
        for (alias, target) in &by_alias {
            if desk_only && !matches!(target, MentionTarget::Desk { .. }) {
                continue;
            }
            let end = after + alias.len();
            if end > lowered.len() {
                continue;
            }
            // Byte comparison, not a `&lowered[after..end]` str slice: `end`
            // is an arbitrary byte offset (`after` plus some alias's byte
            // length), and nothing has proven it lands on a UTF-8 character
            // boundary in `lowered`. A message that puts a multi-byte
            // character where a shorter alias's end would fall — `@é` against
            // a one-character alias `j`, for instance — slices mid-character
            // and panics. Comparing `&[u8]` cannot panic on a partial
            // character: two byte spans are equal or they are not, and
            // `lowered`/`bytes` share `text`'s length either way (the fold
            // above guarantees it), so the offsets still line up.
            if &lowered.as_bytes()[after..end] != alias.as_bytes() || !closes_mention(bytes, end) {
                continue;
            }
            match matched {
                None => matched = Some((end, target)),
                // A second target claiming a span of the same length is a real
                // collision. A shorter one is not — longest already won.
                Some((prev_end, prev)) if prev_end == end && prev != *target => {
                    ambiguous = true;
                    break;
                }
                Some(_) => break,
            }
        }

        if let (Some((end, target)), false) = (matched, ambiguous) {
            out.push(Mention {
                target: (*target).clone(),
                text: text[i..end].to_string(),
                offset: i,
                quiet: false,
            });
            i = end;
            continue;
        }
        // Unresolved or ambiguous: leave it as text, and skip past this `@` so
        // a longer alias starting mid-word cannot re-match inside it.
        i = after;
    }
    out
}

/// Dedupe, drop self-mentions, and cap.
///
/// * **Deduped by target**, keeping the first span, so `@ada … @ada` pings once
///   and chips twice.
/// * **The sender is dropped.** You do not mention yourself, and a message that
///   notified its own author would badge every channel the moment you posted in
///   it.
/// * **Capped at [`MENTION_CAP`] pings.** Past the cap the tail is demoted to
///   [`Mention::quiet`] rather than removed — the spans survive, so what a
///   reader sees still matches what the author wrote, and only the notifying
///   stops.
///
/// * **At most one target per span.** A structured caller can otherwise
///   submit the same `(offset, text)` twice with two different live targets —
///   two people who share an alias, say — and both would survive dedupe-by-
///   target (which only catches the SAME target repeated) as separate,
///   non-quiet mentions. One run of text cannot literally name two different
///   people at once, so only the first target claiming a span is honoured;
///   sorting by offset first (below) and Rust's stable sort is what makes
///   "first" mean "first as the caller supplied it" for two entries at the
///   same offset — the picker's own ordering, so it still decides which of an
///   ambiguous pair a click meant.
///
/// Sorted by offset on the way out, so the order is the order a reader
/// encounters them rather than the order the matcher happened to find them.
pub fn normalize(mut mentions: Vec<Mention>, sender: Option<&Actor>) -> Vec<Mention> {
    mentions.sort_by_key(|m| m.offset);

    let mut seen: HashSet<MentionTarget> = HashSet::new();
    let mut seen_spans: HashSet<usize> = HashSet::new();
    let mut pings = 0usize;
    let mut out = Vec::with_capacity(mentions.len());

    for mut mention in mentions {
        if is_sender(&mention.target, sender) {
            continue;
        }
        if !seen_spans.insert(mention.offset) {
            continue;
        }
        let duplicate = !seen.insert(mention.target.clone());
        if duplicate || pings >= MENTION_CAP {
            mention.quiet = true;
        } else if !mention.quiet {
            pings += 1;
        }
        out.push(mention);
    }
    out
}

/// Whether a client-supplied mention's typed text is a real spelling of its
/// claimed target, per the same [`directory`] the extraction path matches
/// against.
///
/// The comparison strips the leading `@` (or `#` — [`directory`] aliases carry
/// neither) and folds ASCII case, mirroring [`extract_with_known`]'s own
/// matching rule so a span the extractor would have accepted is never rejected
/// here. `Everyone` and `Desk` targets are covered the same way, since a
/// caller can misclaim those exactly as easily as an agent or a user.
fn is_valid_alias_for(mention: &Mention, dir: &[MentionAlias]) -> bool {
    // Strips `@` and, for the desk spelling `opens_mention`/`extract_with_known`
    // both accept, the `#` right after it too — `@#engineering` must compare
    // against the same `"engineering"` alias `@engineering` does, not against
    // `"#engineering"`, which is nobody's alias and would fail every desk
    // mention the console's own picker can produce for that spelling.
    let body = mention.text.strip_prefix('@').unwrap_or(&mention.text);
    // `@#…` is the desk-only spelling. `extract_with_known` narrows a hashed
    // body to desk targets when scanning text, and revalidation must apply the
    // same rule: without it, a user or agent whose label happens to start with
    // `#` would pass the alias check below (the hash is stripped, leaving a
    // plain word) with a visually desk-shaped mention that never names them.
    let desk_spelling = body.strip_prefix('#');
    if desk_spelling.is_some() && !matches!(mention.target, MentionTarget::Desk { .. }) {
        return false;
    }
    let body = desk_spelling.unwrap_or(body);
    let body = body.to_lowercase();
    dir.iter()
        .any(|entry| entry.target == mention.target && entry.aliases.iter().any(|a| a == &body))
}

fn is_sender(target: &MentionTarget, sender: Option<&Actor>) -> bool {
    let Some(sender) = sender else {
        return false;
    };
    match (target, sender.kind) {
        (MentionTarget::User { id }, ActorKind::User) => id == &sender.id,
        (MentionTarget::Agent { id }, ActorKind::Agent) => id == &sender.id,
        _ => false,
    }
}

/// Re-check client-supplied mentions against the live company, demoting any
/// that no longer resolve.
///
/// The console's picker resolved these against whatever it had loaded, which
/// may be minutes old and may predate a teammate being retired or a person
/// being removed. Rather than trust it or reject the message, a target that no
/// longer exists is demoted to [`Mention::quiet`]: the chip goes, the text the
/// author typed stays exactly as they typed it, and nobody is pinged.
///
/// Failing closed like this matters most for agents, where a mention *routes*:
/// a stale picker must not be able to address a turn to a teammate the company
/// no longer has.
///
/// Spans that do not actually appear at their claimed offset are dropped
/// outright — that is a malformed body rather than a stale one, and honouring
/// it would let a caller draw a chip over text that says something else.
///
/// A span whose text is not actually a spelling of the claimed target is
/// demoted the same way a stale target is. Matching the byte span alone only
/// proves the caller copied real text out of the message; it does not prove
/// that text names the target it claims — a caller could otherwise pair
/// arbitrary text (`"hello"`, no `@` and no alias at all) with any live agent
/// id and have it persisted as a non-quiet mention, drawing a chip and a
/// routing decision over prose that never named anyone. The picker is still
/// trusted to pick *which* of several genuinely ambiguous aliases a click
/// meant — this only checks that the typed span is *a* valid spelling of the
/// target it was paired with. Checked only when the target is otherwise live:
/// a target that is already being demoted for having left the roster is not
/// in `dir` at all (it is built from the same live company and user list),
/// so it would fail this check for the wrong reason.
///
/// A span whose text is a real alias but sits somewhere `opens_mention`/
/// `closes_mention` would refuse — mid-word (`jane@engineer`), or inside a
/// fenced or inline code span — is dropped outright too, same as a span that
/// does not match its claimed text at all. Matching alias text is not enough
/// on its own: fallback extraction deliberately never treats either shape as
/// a mention, and a structured caller must not be able to manufacture a chip,
/// and — once mention routing is wired — a routing decision, from text that
/// reads as something else to every other path through this module. Checked
/// against a **masked** copy (`strip_code_regions`), which preserves every
/// offset, so a span the mask blanked out (inside a code region) fails the
/// open check exactly as one that was never `@`-shaped at all.
pub fn revalidate(
    text: &str,
    mentions: Vec<Mention>,
    record: &CompanyRecord,
    users: &[UserRecord],
) -> Vec<Mention> {
    let user_ids: HashSet<&str> = users.iter().map(|u| u.id.as_str()).collect();
    let dir = directory(record, users);
    let masked = strip_code_regions(text);
    let masked_bytes = masked.as_bytes();
    mentions
        .into_iter()
        .filter(|m| text.get(m.offset..m.offset + m.text.len()) == Some(m.text.as_str()))
        .filter(|m| {
            let end = m.offset + m.text.len();
            end <= masked_bytes.len()
                && opens_mention(&masked, masked_bytes, m.offset)
                && closes_mention(masked_bytes, end)
        })
        .map(|mut m| {
            let live = match &m.target {
                MentionTarget::Agent { id } => record.is_roster_agent(id),
                MentionTarget::User { id } => user_ids.contains(id.as_str()),
                MentionTarget::Desk { id } => record.resolve_desk_id(id).is_some(),
                MentionTarget::Everyone => true,
            };
            let live = live && is_valid_alias_for(&m, &dir);
            if !live {
                m.quiet = true;
            }
            m
        })
        .collect()
}

/// The whole server-side pipeline for one message body.
///
/// Uses `supplied` when the caller had a picker, and falls back to extracting
/// from the text when it did not. Either way the result is normalized, so both
/// paths obey the cap, the dedupe, and the no-self-mention rule identically.
pub fn resolve(
    text: &str,
    supplied: Option<Vec<Mention>>,
    sender: Option<&Actor>,
    record: &CompanyRecord,
    users: &[UserRecord],
) -> Vec<Mention> {
    let found = match supplied {
        Some(supplied) if !supplied.is_empty() => revalidate(text, supplied, record, users),
        // An explicitly empty list is still an answer — a console that ran its
        // picker and found nothing must not have the host guess on its behalf.
        Some(_) => Vec::new(),
        None => {
            let masked = strip_code_regions(text);
            extract_with_known(&masked, &directory(record, users))
                .into_iter()
                .map(|mut m| {
                    // Offsets are preserved by the mask, so re-read the span
                    // from the real body — the masked copy has spaces where a
                    // code region was.
                    if let Some(real) = text.get(m.offset..m.offset + m.text.len()) {
                        m.text = real.to_string();
                    }
                    m
                })
                .collect()
        }
    };
    normalize(found, sender)
}

/// The teammate an operator message addresses by name, or `None` to fall
/// through to the desk's own routing.
///
/// Returns the **first** non-quiet agent mention that is still on the roster.
/// First rather than last because that is the one the sentence is about:
/// "@ada can you check with @ben" is a question for Ada.
///
/// # Why this outranks the desk lead
///
/// Naming somebody in a room is a stronger address than the room's default
/// answerer. This is the same explicit-beats-implicit ordering the existing
/// resolver already applies between an addressed desk and the orchestrator —
/// mentions extend the ladder by one rung at the top rather than introducing a
/// second, competing notion of "who is this for".
///
/// Resolving nothing returns `None`, which leaves dispatch exactly as it was.
pub fn mention_responder(record: &CompanyRecord, mentions: &[Mention]) -> Option<String> {
    mentions
        .iter()
        .filter(|m| !m.quiet)
        .filter_map(|m| m.target.agent_id())
        .find(|id| record.is_roster_agent(id))
        .map(str::to_string)
}

/// Every teammate this message names, for the answering turn's context.
///
/// Expands [`MentionTarget::Desk`] and [`MentionTarget::Everyone`] against the
/// desk's effective membership, so `@everyone` in `#engineering` names the
/// engineering desk rather than the whole company.
///
/// The one channel where it *does* name the whole company is the built-in
/// `#general` (issue #1743), which is not a desk and has no membership of its
/// own: there, `@everyone` expands to the roster, derived at read time, so a
/// teammate added a minute ago is named without anything having been written.
///
/// # This is a list, not a fan-out
///
/// One operator message spawns exactly one turn — the invariant the chat POST
/// has always had — and nothing here changes that. These ids are *named to*
/// the responding teammate so it knows who else was addressed, and it spreads
/// the work, if it should, through the existing gated delegation seam. A
/// mention must not become a way to start N turns without an approval in sight.
///
/// Deduplicated, in first-mention order, and the responder itself is excluded:
/// telling a teammate it was mentioned in the message it is answering is noise.
pub fn mentioned_agents(
    record: &CompanyRecord,
    desk: &str,
    mentions: &[Mention],
    responder: Option<&str>,
) -> Vec<String> {
    fn push(out: &mut Vec<String>, record: &CompanyRecord, responder: Option<&str>, id: String) {
        if Some(id.as_str()) != responder && !out.contains(&id) && record.is_roster_agent(&id) {
            out.push(id);
        }
    }

    let mut out: Vec<String> = Vec::new();
    for mention in mentions.iter().filter(|m| !m.quiet) {
        match &mention.target {
            MentionTarget::Agent { id } => push(&mut out, record, responder, id.clone()),
            MentionTarget::Desk { id } => {
                if let Some(desk_id) = record.resolve_desk_id(id) {
                    for member in record.effective_desk_members(&desk_id) {
                        push(&mut out, record, responder, member);
                    }
                }
            }
            // An **overlay** desk cannot stand in for the built-in `#general`
            // channel here, and does not need filtering out: `resolve_desk_id`
            // declines to match one against a General spelling at all (issue
            // #1743), so a desk that took `general`/`main`/`General` before
            // those were reserved cannot narrow a company-wide broadcast to its
            // own membership. A desk the *blueprint* declares still wins, which
            // is the grandfathering this host has always honoured.
            MentionTarget::Everyone => match record.resolve_desk_id(desk) {
                Some(desk_id) => {
                    for member in record.effective_desk_members(&desk_id) {
                        push(&mut out, record, responder, member);
                    }
                }
                // The built-in `#general` channel is not a desk (issue #1743),
                // so it has no membership to expand against — it *is* the whole
                // roster, derived here on every read. Before this, `@everyone`
                // on the company-wide line resolved to nobody: the arm above
                // found no desk and the broadcast named no one, which is the
                // one channel where it should name everyone.
                //
                // Still a **list, not a fan-out** — see this function's note.
                // One operator message spawns one turn whatever it names, so a
                // broadcast here costs the same as any other message; it only
                // tells the answering teammate who else was addressed.
                //
                // Ordered by the same manifest-then-overlay walk `desk_ids`
                // uses, so "who is in #general" reads the same as every other
                // roster surface.
                None if crate::server::chat_history::is_general_chat(Some(desk)) => {
                    for id in crate::runtime::delegation_tools::roster_agent_ids(record) {
                        push(&mut out, record, responder, id);
                    }
                }
                None => {}
            },
            MentionTarget::User { .. } => {}
        }
    }
    out
}

/// Every person this message should notify.
///
/// Expands [`MentionTarget::Everyone`] to the whole user list — a broadcast
/// addresses the company's people, not the addressed desk's, because desk
/// membership is a teammate concept and every signed-in person can already see
/// every desk.
///
/// Quiet mentions notify nobody, which is what makes them quiet. Deduplicated,
/// in first-mention order.
pub fn mentioned_users(users: &[UserRecord], mentions: &[Mention]) -> Vec<String> {
    let known: HashSet<&str> = users.iter().map(|u| u.id.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    for mention in mentions.iter().filter(|m| !m.quiet) {
        match &mention.target {
            MentionTarget::User { id } => {
                if known.contains(id.as_str()) && !out.contains(id) {
                    out.push(id.clone());
                }
            }
            MentionTarget::Everyone => {
                for user in users {
                    if !out.contains(&user.id) {
                        out.push(user.id.clone());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::types::{AgentOverride, CompanyId, CompanyRecord, OverlayAgent};
    use crate::ports::users::{UserRole, UserStatus};

    const MANIFEST: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "engineer"
role = "Backend Engineer"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["engineer", "ceo"]
"#;

    fn record(toml_src: &str) -> CompanyRecord {
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest: toml::from_str(toml_src).expect("parse manifest"),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    fn acme() -> CompanyRecord {
        record(MANIFEST)
    }

    fn user(id: &str, email: &str, display: Option<&str>) -> UserRecord {
        UserRecord {
            id: id.to_string(),
            email: email.to_string(),
            display_name: display.map(str::to_string),
            avatar: None,
            role: UserRole::Member,
            status: UserStatus::Active,
            password_hash: None,
            must_change_password: false,
            created_at_millis: 0,
            last_seen_at_millis: None,
            updated_at_millis: 0,
        }
    }

    fn people() -> Vec<UserRecord> {
        vec![
            user("u1", "jane@acme.test", Some("Jane Doe")),
            user("u2", "sam@acme.test", None),
        ]
    }

    fn resolve_text(text: &str) -> Vec<Mention> {
        let record = acme();
        let users = people();
        resolve(text, None, None, &record, &users)
    }

    fn targets(mentions: &[Mention]) -> Vec<&MentionTarget> {
        mentions.iter().map(|m| &m.target).collect()
    }

    fn agent(id: &str) -> MentionTarget {
        MentionTarget::Agent { id: id.to_string() }
    }

    // -----------------------------------------------------------------------
    // What is, and is not, a mention
    // -----------------------------------------------------------------------

    #[test]
    fn a_roster_id_after_whitespace_is_a_mention() {
        let found = resolve_text("hey @engineer can you look?");
        assert_eq!(targets(&found), vec![&agent("engineer")]);
        assert_eq!(found[0].text, "@engineer");
        assert_eq!(found[0].offset, 4);
        assert!(!found[0].quiet);
    }

    #[test]
    fn a_mention_at_the_very_start_resolves() {
        let found = resolve_text("@engineer ping");
        assert_eq!(targets(&found), vec![&agent("engineer")]);
        assert_eq!(found[0].offset, 0);
    }

    /// The single most important negative case: an email address contains an
    /// `@` followed by something that can look exactly like a roster id.
    #[test]
    fn an_email_address_is_not_a_mention() {
        assert!(resolve_text("write to jane@engineer.com about it").is_empty());
        assert!(resolve_text("engineer@acme.test").is_empty());
    }

    #[test]
    fn trailing_punctuation_does_not_break_a_mention() {
        for text in ["@engineer, thoughts?", "ask @engineer.", "(@engineer)"] {
            let found = resolve_text(text);
            assert_eq!(targets(&found), vec![&agent("engineer")], "text: {text}");
            assert_eq!(found[0].text, "@engineer", "text: {text}");
        }
    }

    /// `@engineering` must not resolve to the `engineer` teammate — a mention
    /// has to end on a boundary, or every longer name becomes a misroute.
    #[test]
    fn a_longer_word_does_not_resolve_to_a_shorter_id() {
        let found = resolve_text("the @engineerish thing");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_unknown_name_resolves_to_nothing() {
        assert!(resolve_text("@nobody are you there").is_empty());
    }

    // -----------------------------------------------------------------------
    // Code regions
    // -----------------------------------------------------------------------

    #[test]
    fn a_mention_inside_an_inline_code_span_is_not_a_mention() {
        assert!(resolve_text("run `@engineer --help` first").is_empty());
    }

    #[test]
    fn a_mention_inside_a_fenced_block_is_not_a_mention() {
        let text = "before\n```\n@engineer\n```\nafter";
        assert!(resolve_text(text).is_empty());
    }

    /// A line like ```not-a-close is code, not a closing fence: CommonMark
    /// only lets a fence be followed by spaces or tabs. Closing the mask there
    /// would unmask a later `@engineer` the renderer still shows as code.
    #[test]
    fn a_false_closing_fence_does_not_unmask_a_later_mention() {
        let text = "before\n```\ncode\n```not-a-close\n@engineer\n```\nafter";
        assert!(resolve_text(text).is_empty());
    }

    /// Trailing whitespace on a closing fence is still a valid close
    /// (CommonMark allows spaces or tabs), so the block keeps masking.
    #[test]
    fn a_fence_closed_with_trailing_whitespace_still_masks() {
        let text = "before\n```\n@engineer\n```  \nafter";
        assert!(resolve_text(text).is_empty());
    }

    /// A CRLF line ending is still a close — the `\r` is part of the ending,
    /// not fence text, and must keep closing the block as it did before the
    /// suffix was restricted.
    #[test]
    fn a_fence_closed_over_crlf_still_masks() {
        let text = "before\n```\n@engineer\n```\r\nafter";
        assert!(resolve_text(text).is_empty());
    }

    /// The reason [`strip_code_regions`] blanks rather than removes: a mention
    /// *after* a code span must still land on its real byte offset.
    #[test]
    fn offsets_survive_a_masked_code_span() {
        let text = "`@ceo` but really @engineer";
        let found = resolve_text(text);
        assert_eq!(targets(&found), vec![&agent("engineer")]);
        let m = &found[0];
        assert_eq!(&text[m.offset..m.offset + m.text.len()], "@engineer");
    }

    #[test]
    fn masking_preserves_length_and_newlines() {
        let text = "a\n```\nxx\n```\nb `y` c";
        let masked = strip_code_regions(text);
        assert_eq!(masked.len(), text.len());
        assert_eq!(
            masked.matches('\n').count(),
            text.matches('\n').count(),
            "newlines are kept so line structure survives"
        );
        assert!(!masked.contains("xx"));
        assert!(masked.starts_with("a\n"));
    }

    /// An unbalanced backtick is not a code span, and must not swallow the rest
    /// of the message.
    #[test]
    fn an_unclosed_backtick_does_not_mask_the_rest() {
        let found = resolve_text("weird ` tick then @engineer");
        assert_eq!(targets(&found), vec![&agent("engineer")]);
    }

    /// A longer closing run cannot close a shorter opener: CommonMark only lets
    /// a *whole* run of exactly the opening length close a span, so
    /// `` `code @engineer here`` `` (one opener, two trailing) is not code and
    /// the mention — which opens after a space and closes before one — must
    /// resolve. The console's mask has to agree with this or it would suppress
    /// a mention the renderer still shows. (`` `@engineer`` `` does *not*
    /// resolve either side: an `@` right after a backtick is not a
    /// mention-opening position.)
    #[test]
    fn a_longer_backtick_run_does_not_close_a_shorter_opener() {
        let found = resolve_text("`code @engineer here``");
        assert_eq!(targets(&found), vec![&agent("engineer")]);
    }

    // -----------------------------------------------------------------------
    // Longest-alias-wins, and ambiguity
    // -----------------------------------------------------------------------

    /// `@Jane Doe` has a space in it and is only reachable because the matcher
    /// tries the whole display name, longest first.
    #[test]
    fn a_two_word_display_name_resolves() {
        let found = resolve_text("thanks @Jane Doe!");
        assert_eq!(
            targets(&found),
            vec![&MentionTarget::User {
                id: "u1".to_string()
            }]
        );
        assert_eq!(found[0].text, "@Jane Doe");
    }

    #[test]
    fn a_slug_also_reaches_a_person() {
        let found = resolve_text("thanks @jane-doe");
        assert_eq!(
            targets(&found),
            vec![&MentionTarget::User {
                id: "u1".to_string()
            }]
        );
    }

    #[test]
    fn a_person_with_no_display_name_is_reachable_by_their_local_part() {
        let found = resolve_text("@sam what do you think");
        assert_eq!(
            targets(&found),
            vec![&MentionTarget::User {
                id: "u2".to_string()
            }]
        );
    }

    /// One member's name prefixing another's is the case that silently
    /// misroutes if the matcher takes the first match rather than the longest.
    #[test]
    fn the_longest_alias_wins() {
        let users = vec![
            user("u1", "ann@acme.test", Some("Ann")),
            user("u2", "annlee@acme.test", Some("Ann Lee")),
        ];
        let found = resolve("ping @Ann Lee now", None, None, &acme(), &users);
        assert_eq!(
            targets(&found),
            vec![&MentionTarget::User {
                id: "u2".to_string()
            }],
            "the longer name must win, or Ann Lee can never be mentioned"
        );
    }

    /// Never guess a ping.
    #[test]
    fn an_ambiguous_name_resolves_to_nobody() {
        let users = vec![
            user("u1", "sam.a@acme.test", Some("Sam")),
            user("u2", "sam.b@acme.test", Some("Sam")),
        ];
        let found = resolve("hey @Sam", None, None, &acme(), &users);
        assert!(
            found.is_empty(),
            "two people share this name, so it must stay literal text: {found:?}"
        );
    }

    #[test]
    fn colliding_slugs_are_disambiguated_in_order() {
        let users = vec![
            user("u1", "a@acme.test", Some("Sam")),
            user("u2", "b@acme.test", Some("Sam")),
            user("u3", "c@acme.test", Some("Sam")),
        ];
        assert_eq!(user_slugs(&users), vec!["sam", "sam-2", "sam-3"]);
    }

    /// A natural display name can already look like a generated
    /// disambiguation (`"Sam-2"` is a real name someone can type at signup),
    /// and the counter must still notice it collided with the second `Sam`
    /// rather than handing both people the same slug.
    #[test]
    fn a_natural_label_matching_a_generated_suffix_does_not_collide() {
        let users = vec![
            user("u1", "a@acme.test", Some("Sam")),
            user("u2", "b@acme.test", Some("Sam")),
            user("u3", "c@acme.test", Some("Sam-2")),
        ];
        let slugs = user_slugs(&users);
        assert_eq!(
            slugs.len(),
            slugs.iter().collect::<std::collections::HashSet<_>>().len(),
            "every emitted slug must be unique: {slugs:?}"
        );
    }

    #[test]
    fn a_label_with_nothing_typable_yields_an_empty_slug() {
        assert_eq!(mention_slug("!!!"), "");
        assert_eq!(mention_slug("Ana  M. Ruiz"), "ana-m-ruiz");
    }

    /// A symbol-only display name ("🙂") slugs to nothing, which would leave
    /// the person unmentionable while the picker still advertises a row. The
    /// fallback must hand such a user a real, typable alias — the email local
    /// part, then the id — so the picker can insert a spelling the host's
    /// `opens_mention` accepts and the directory can resolve.
    #[test]
    fn a_symbol_only_display_name_still_gets_a_typable_slug() {
        let users = vec![
            user("u1", "smiley@acme.test", Some("🙂")),
            user("u2", "no_name@acme.test", Some("!!!")),
            user("u3", "plain@acme.test", Some("Ada")),
        ];
        assert_eq!(user_slugs(&users), vec!["smiley", "no-name", "ada"]);
    }

    // -----------------------------------------------------------------------
    // Desks and everyone
    // -----------------------------------------------------------------------

    #[test]
    fn a_desk_resolves_by_id_and_by_name() {
        for text in ["@engineering please", "@Engineering please"] {
            let found = resolve_text(text);
            assert_eq!(
                targets(&found),
                vec![&MentionTarget::Desk {
                    id: "engineering".to_string()
                }],
                "text: {text}"
            );
        }
    }

    /// `MentionTarget::Desk`'s own doc comment advertises `@#engineering` as
    /// the desk spelling; extraction must actually accept it, and a
    /// client-supplied mention using it must revalidate as live rather than
    /// being demoted for a text/target mismatch.
    #[test]
    fn a_hash_prefixed_desk_mention_resolves() {
        let found = resolve_text("@#engineering please");
        assert_eq!(
            targets(&found),
            vec![&MentionTarget::Desk {
                id: "engineering".to_string()
            }]
        );
        assert_eq!(found[0].text, "@#engineering");

        let supplied = vec![Mention {
            target: MentionTarget::Desk {
                id: "engineering".to_string(),
            },
            text: "@#engineering".to_string(),
            offset: 0,
            quiet: false,
        }];
        let out = resolve(
            "@#engineering please",
            Some(supplied),
            None,
            &acme(),
            &people(),
        );
        assert_eq!(out.len(), 1);
        assert!(!out[0].quiet, "{out:?}");
    }

    /// `@#` naming a non-desk alias must not resolve — the `#` is desk-only,
    /// so `@#engineer` (an agent id) must stay literal text rather than
    /// silently falling back to the unprefixed match.
    #[test]
    fn a_hash_prefixed_non_desk_alias_does_not_resolve() {
        let found = resolve_text("@#engineer please");
        assert!(found.is_empty(), "{found:?}");
    }

    /// The same desk-only rule applies when a structured caller supplies the
    /// span: `@#engineer` paired with the agent target must not ping them.
    /// `is_valid_alias_for` strips the hash for the alias comparison, so
    /// without the kind check here the `#`-shaped mention would validate.
    #[test]
    fn a_hash_prefixed_non_desk_target_is_demoted_in_revalidation() {
        let supplied = vec![Mention {
            target: agent("engineer"),
            text: "@#engineer".to_string(),
            offset: 0,
            quiet: false,
        }];
        let out = resolve(
            "@#engineer please",
            Some(supplied),
            None,
            &acme(),
            &people(),
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].quiet, "{out:?}");
    }

    #[test]
    fn every_everyone_alias_reaches_the_same_target() {
        for alias in EVERYONE_ALIASES {
            let found = resolve_text(&format!("@{alias} heads up"));
            assert_eq!(
                targets(&found),
                vec![&MentionTarget::Everyone],
                "alias: {alias}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // normalize: self, duplicates, the cap
    // -----------------------------------------------------------------------

    #[test]
    fn the_sender_is_not_mentioned_in_their_own_message() {
        let sender = Actor {
            kind: ActorKind::User,
            id: "u1".to_string(),
        };
        let found = resolve(
            "@Jane Doe and @sam",
            None,
            Some(&sender),
            &acme(),
            &people(),
        );
        assert_eq!(
            targets(&found),
            vec![&MentionTarget::User {
                id: "u2".to_string()
            }],
            "a message must not notify its own author"
        );
    }

    #[test]
    fn an_agent_does_not_mention_itself_in_its_own_reply() {
        let sender = Actor {
            kind: ActorKind::Agent,
            id: "engineer".to_string(),
        };
        let found = resolve(
            "@engineer and @ceo",
            None,
            Some(&sender),
            &acme(),
            &people(),
        );
        assert_eq!(targets(&found), vec![&agent("ceo")]);
    }

    #[test]
    fn a_repeated_mention_chips_twice_and_pings_once() {
        let found = resolve_text("@engineer ... @engineer again");
        assert_eq!(found.len(), 2, "both spans render");
        assert!(!found[0].quiet);
        assert!(found[1].quiet, "the second is render-only");
    }

    /// Past the cap the tail is demoted, never deleted — what a reader sees
    /// still has to match what the author wrote.
    #[test]
    fn the_cap_demotes_the_tail_and_keeps_every_span() {
        let mentions: Vec<Mention> = (0..MENTION_CAP + 5)
            .map(|i| Mention {
                target: MentionTarget::User {
                    id: format!("u{i}"),
                },
                text: format!("@u{i}"),
                offset: i * 8,
                quiet: false,
            })
            .collect();
        let out = normalize(mentions, None);
        assert_eq!(out.len(), MENTION_CAP + 5, "no span is dropped");
        assert_eq!(
            out.iter().filter(|m| !m.quiet).count(),
            MENTION_CAP,
            "exactly the cap pings"
        );
        assert!(out.last().expect("a tail mention").quiet);
    }

    #[test]
    fn mentions_come_back_in_reading_order() {
        let found = resolve_text("@ceo then @engineer");
        let offsets: Vec<usize> = found.iter().map(|m| m.offset).collect();
        let mut sorted = offsets.clone();
        sorted.sort_unstable();
        assert_eq!(offsets, sorted);
    }

    // -----------------------------------------------------------------------
    // revalidate: the client's answer is checked, not trusted
    // -----------------------------------------------------------------------

    #[test]
    fn a_supplied_mention_for_a_missing_teammate_is_demoted_not_dropped() {
        let supplied = vec![Mention {
            target: agent("ghost"),
            text: "@ghost".to_string(),
            offset: 0,
            quiet: false,
        }];
        let out = resolve("@ghost hello", Some(supplied), None, &acme(), &people());
        assert_eq!(out.len(), 1, "the span survives so the text still matches");
        assert!(out[0].quiet, "but it pings nobody");
    }

    #[test]
    fn a_supplied_span_that_is_not_in_the_text_is_dropped() {
        let supplied = vec![Mention {
            target: agent("engineer"),
            text: "@engineer".to_string(),
            offset: 40,
            quiet: false,
        }];
        let out = resolve("short message", Some(supplied), None, &acme(), &people());
        assert!(
            out.is_empty(),
            "a chip must never be drawn over text that says something else"
        );
    }

    /// A console that ran its picker and found nothing has given an answer; the
    /// host must not then extract on its behalf.
    #[test]
    fn an_explicitly_empty_list_suppresses_extraction() {
        let out = resolve(
            "@engineer hello",
            Some(Vec::new()),
            None,
            &acme(),
            &people(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn an_absent_list_falls_back_to_extraction() {
        let out = resolve("@engineer hello", None, None, &acme(), &people());
        assert_eq!(targets(&out), vec![&agent("engineer")]);
    }

    // -----------------------------------------------------------------------
    // Routing
    // -----------------------------------------------------------------------

    #[test]
    fn a_mentioned_teammate_becomes_the_responder() {
        let found = resolve_text("@engineer what is the build status");
        assert_eq!(
            mention_responder(&acme(), &found),
            Some("engineer".to_string())
        );
    }

    #[test]
    fn the_first_mentioned_teammate_answers() {
        let found = resolve_text("@ceo can you check with @engineer");
        assert_eq!(mention_responder(&acme(), &found), Some("ceo".to_string()));
    }

    #[test]
    fn a_quiet_mention_never_routes() {
        let mentions = vec![Mention {
            target: agent("engineer"),
            text: "@engineer".to_string(),
            offset: 0,
            quiet: true,
        }];
        assert_eq!(mention_responder(&acme(), &mentions), None);
    }

    #[test]
    fn an_off_roster_mention_falls_through_to_desk_routing() {
        let mentions = vec![Mention {
            target: agent("ghost"),
            text: "@ghost".to_string(),
            offset: 0,
            quiet: false,
        }];
        assert_eq!(
            mention_responder(&acme(), &mentions),
            None,
            "so the caller uses the desk lead, exactly as before"
        );
    }

    #[test]
    fn mentioning_only_people_does_not_change_the_responder() {
        let found = resolve_text("@Jane Doe thoughts?");
        assert_eq!(mention_responder(&acme(), &found), None);
    }

    // -----------------------------------------------------------------------
    // Expansion — a list for the turn's context, never a fan-out
    // -----------------------------------------------------------------------

    #[test]
    fn everyone_expands_to_the_addressed_desks_members() {
        let found = resolve_text("@everyone standup in five");
        let named = mentioned_agents(&acme(), "engineering", &found, None);
        assert_eq!(named, vec!["engineer".to_string(), "ceo".to_string()]);
    }

    #[test]
    fn everyone_notifies_every_person_in_the_company() {
        let found = resolve_text("@everyone standup in five");
        assert_eq!(
            mentioned_users(&people(), &found),
            vec!["u1".to_string(), "u2".to_string()],
            "a broadcast addresses the company's people, not a desk's teammates"
        );
    }

    /// `@everyone` on the built-in `#general` channel names the whole roster,
    /// under every spelling the host folds into it (issue #1743).
    ///
    /// Before this it named **nobody**: `#general` is not a desk, so
    /// `resolve_desk_id` found nothing and the broadcast arm expanded against
    /// an empty membership. The one channel where "everyone" literally means
    /// everyone was the one channel where `@everyone` reached no one.
    #[test]
    fn everyone_on_the_general_channel_names_the_whole_roster() {
        let found = resolve_text("@everyone standup in five");
        for spelling in ["general", "General", "main", "Main", ""] {
            assert_eq!(
                mentioned_agents(&acme(), spelling, &found, None),
                vec!["engineer".to_string(), "ceo".to_string()],
                "@everyone addressed as {spelling:?} must name the whole roster"
            );
        }
    }

    /// An **overlay** desk that took a General spelling before those were
    /// reserved must not narrow the company-wide broadcast (issue #1743).
    ///
    /// `resolve_desk_id` matches a desk by id *or* by case-insensitive name, so
    /// a persisted `{id: "ops", name: "General"}` is selected when
    /// `HarnessBrain::everyone_desk` folds the built-in `main` thread to
    /// `General` — and `@everyone` on the one channel where everyone means
    /// everyone would reach only that desk's members. A desk the *blueprint*
    /// declares is the company's own General desk and still wins; this is only
    /// about state `create_desk` used to accept and now refuses.
    #[test]
    fn an_overlay_desk_squatting_a_general_spelling_does_not_narrow_the_broadcast() {
        let mut record = acme();
        record.overlay_desks.push(crate::ports::types::OverlayDesk {
            id: "ops".to_string(),
            name: "General".to_string(),
            description: None,
            responder: Default::default(),
            members: vec!["ceo".to_string()],
        });
        let found = resolve_text("@everyone standup in five");
        for spelling in ["general", "General", "main", ""] {
            assert_eq!(
                mentioned_agents(&record, spelling, &found, None),
                vec!["engineer".to_string(), "ceo".to_string()],
                "@everyone addressed as {spelling:?} must still name the whole roster"
            );
        }
        // And the squatting desk keeps working as the desk it is, addressed by
        // its own id — this narrows the broadcast, nothing else.
        assert_eq!(
            mentioned_agents(&record, "ops", &found, None),
            vec!["ceo".to_string()],
            "the desk itself is unchanged"
        );
    }

    /// A named desk keeps expanding against **its own** membership, not the
    /// roster — the reservation above must not leak into every channel.
    #[test]
    fn everyone_on_a_named_desk_still_names_only_that_desk() {
        let mut record = acme();
        // A teammate on nobody's desk: on the roster, off `#engineering`.
        record.overlay_agents.push(OverlayAgent {
            id: "designer".to_string(),
            name: "Dana".to_string(),
            role: "Designer".to_string(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
        let found = resolve_text("@everyone standup in five");
        assert_eq!(
            mentioned_agents(&record, "engineering", &found, None),
            vec!["engineer".to_string(), "ceo".to_string()],
            "a desk broadcast is bounded by the desk"
        );
    }

    /// Membership of `#general` is **derived, never stored**: a teammate added
    /// to the roster a moment ago is in it, with no membership write anywhere
    /// (issue #1743).
    ///
    /// The proof is the mutation, not the assertion: the only thing this test
    /// changes is `overlay_agents` — the roster. `overlay_desk_members`,
    /// `overlay_desks` and `overlay_desk_order` are asserted still empty, so
    /// there is no second copy of "who is in #general" that could drift from
    /// the roster. That is the whole reason the channel is not a desk.
    #[test]
    fn a_teammate_added_to_the_roster_is_in_general_with_no_membership_write() {
        let mut record = acme();
        let found = resolve_text("@everyone standup in five");
        assert_eq!(
            mentioned_agents(&record, "general", &found, None),
            vec!["engineer".to_string(), "ceo".to_string()]
        );

        record.overlay_agents.push(OverlayAgent {
            id: "designer".to_string(),
            name: "Dana".to_string(),
            role: "Designer".to_string(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });

        assert_eq!(
            mentioned_agents(&record, "general", &found, None),
            vec![
                "engineer".to_string(),
                "ceo".to_string(),
                "designer".to_string()
            ],
            "the new teammate is in #general the moment it joins the roster"
        );
        assert!(
            record.overlay_desk_members.is_empty()
                && record.overlay_desks.is_empty()
                && record.overlay_desk_order.is_empty(),
            "nothing was written to any desk overlay to make that true"
        );
    }

    /// A retired teammate drops out of `#general` on the same read, for the
    /// same reason: `push` re-checks `is_roster_agent`, which is what a derived
    /// membership buys — there is no stale seat to clean up.
    #[test]
    fn a_retired_teammate_leaves_general_on_the_next_read() {
        let mut record = acme();
        record.overlay_retired_agents.push("engineer".to_string());
        let found = resolve_text("@everyone standup in five");
        assert_eq!(
            mentioned_agents(&record, "general", &found, None),
            vec!["ceo".to_string()]
        );
    }

    #[test]
    fn a_desk_mention_expands_to_that_desk_not_the_addressed_one() {
        let found = resolve_text("@engineering can you take this");
        let named = mentioned_agents(&acme(), "general", &found, None);
        assert_eq!(named, vec!["engineer".to_string(), "ceo".to_string()]);
    }

    #[test]
    fn the_responder_is_not_told_it_was_mentioned() {
        let found = resolve_text("@engineer and @ceo");
        let named = mentioned_agents(&acme(), "engineering", &found, Some("engineer"));
        assert_eq!(named, vec!["ceo".to_string()]);
    }

    #[test]
    fn expansion_deduplicates() {
        let found = resolve_text("@engineer and @engineering");
        let named = mentioned_agents(&acme(), "engineering", &found, None);
        assert_eq!(named, vec!["engineer".to_string(), "ceo".to_string()]);
    }

    #[test]
    fn a_quiet_mention_expands_to_nothing() {
        let mentions = vec![Mention {
            target: MentionTarget::Everyone,
            text: "@everyone".to_string(),
            offset: 0,
            quiet: true,
        }];
        assert!(mentioned_agents(&acme(), "engineering", &mentions, None).is_empty());
        assert!(mentioned_users(&people(), &mentions).is_empty());
    }

    #[test]
    fn a_person_who_has_left_is_not_notified() {
        let mentions = vec![Mention {
            target: MentionTarget::User {
                id: "gone".to_string(),
            },
            text: "@gone".to_string(),
            offset: 0,
            quiet: false,
        }];
        assert!(mentioned_users(&people(), &mentions).is_empty());
    }

    // -----------------------------------------------------------------------
    // Labels
    // -----------------------------------------------------------------------

    #[test]
    fn a_label_falls_back_from_display_name_to_a_derived_name() {
        assert_eq!(user_label(&user("u", "jane@x.test", Some("Jane"))), "Jane");
        // No chosen name: the same derived name `display_label` uses for the
        // profile pane, not the raw local part — the same person must read the
        // same way on a mention chip and in the people list.
        assert_eq!(user_label(&user("u", "jane.doe@x.test", None)), "Jane Doe");
        // A blanked display name is the same intent as `null`.
        assert_eq!(
            user_label(&user("u", "jane.doe@x.test", Some("  "))),
            "Jane Doe"
        );
        // An identity with no name in it to derive stays the honest fallback.
        assert_eq!(user_label(&user("u", "@x.test", None)), "someone");
    }

    #[test]
    fn a_label_never_leaks_the_full_identity() {
        let label = user_label(&user("u", "jane@acme.test", None));
        assert!(!label.contains('@'), "{label}");
        assert!(!label.contains("acme.test"), "{label}");
    }

    // -----------------------------------------------------------------------
    // Retired teammates
    // -----------------------------------------------------------------------

    #[test]
    fn a_retired_teammate_is_not_mentionable() {
        let mut record = acme();
        record.overlay_retired_agents.push("engineer".to_string());
        let found = resolve("@engineer hello", None, None, &record, &people());
        assert!(found.is_empty(), "{found:?}");
    }

    /// Issue: a manifest teammate's operator-set display name (an
    /// `AgentOverride`, applied through `effective_agents()`) must be a real
    /// alias, not just its authored roster id — an operator who renamed
    /// `ceo` to "Ada" expects `@Ada` to reach them.
    #[test]
    fn an_operator_renamed_manifest_agent_is_mentionable_by_the_new_name() {
        let mut record = acme();
        record.overlay_agent_edits.push(AgentOverride {
            agent_id: "ceo".to_string(),
            name: Some("Ada".to_string()),
            role: None,
            description: None,
            tools: None,
            instructions: None,
            avatar: None,
            ..Default::default()
        });
        let found = resolve("hey @Ada, got a sec?", None, None, &record, &people());
        assert_eq!(targets(&found), vec![&agent("ceo")], "{found:?}");
        // The authored id must keep working too — a rename is additive.
        let found = resolve("hey @ceo, got a sec?", None, None, &record, &people());
        assert_eq!(targets(&found), vec![&agent("ceo")], "{found:?}");
    }

    // -----------------------------------------------------------------------
    // Client-supplied mentions must actually name their claimed target
    // -----------------------------------------------------------------------

    /// A caller cannot pair arbitrary text with a live target's id and have it
    /// persisted as a real, notifying mention — the span must actually be a
    /// spelling of that target. `@hello` is syntactically a mention (starts
    /// with `@`, closes at the space), so this isolates the alias-mismatch
    /// path from the syntax check covered separately below.
    #[test]
    fn a_supplied_mention_whose_text_does_not_name_its_target_is_demoted() {
        let supplied = vec![Mention {
            target: agent("engineer"),
            text: "@hello".to_string(),
            offset: 0,
            quiet: false,
        }];
        let out = resolve("@hello there", Some(supplied), None, &acme(), &people());
        assert_eq!(out.len(), 1, "the span survives so the text still matches");
        assert!(
            out[0].quiet,
            "text that never named the target must not ping it: {out:?}"
        );
    }

    /// Text that never had `@`-shape at all (no `@`, nowhere) is dropped
    /// outright rather than kept and demoted — the same treatment a
    /// mid-word or in-code-span match gets, and consistent with fallback
    /// extraction, which would never have produced a mention here either.
    #[test]
    fn a_supplied_mention_with_no_at_sign_at_all_is_dropped() {
        let supplied = vec![Mention {
            target: agent("engineer"),
            text: "hello".to_string(),
            offset: 0,
            quiet: false,
        }];
        let out = resolve("hello there", Some(supplied), None, &acme(), &people());
        assert!(out.is_empty(), "{out:?}");
    }

    /// The picker is still trusted to disambiguate — a genuinely valid alias
    /// for the claimed target stays a real, notifying mention.
    #[test]
    fn a_supplied_mention_whose_text_does_name_its_target_still_notifies() {
        let supplied = vec![Mention {
            target: agent("engineer"),
            text: "@engineer".to_string(),
            offset: 0,
            quiet: false,
        }];
        let out = resolve("@engineer hello", Some(supplied), None, &acme(), &people());
        assert_eq!(out.len(), 1);
        assert!(!out[0].quiet, "{out:?}");
    }

    /// A live alias sitting somewhere `opens_mention`/`closes_mention` would
    /// refuse — mid-word, here — must be demoted the same way a mismatched
    /// span is, not trusted just because the text happens to spell a real
    /// alias.
    #[test]
    fn a_supplied_mention_mid_word_is_dropped() {
        let supplied = vec![Mention {
            target: agent("engineer"),
            text: "@engineer".to_string(),
            offset: 4,
            quiet: false,
        }];
        let out = resolve("jane@engineer", Some(supplied), None, &acme(), &people());
        assert!(
            out.is_empty(),
            "a span with no whitespace/bracket before it is not a mention: {out:?}"
        );
    }

    /// The same alias-shaped-but-not-a-mention rule applies inside a fenced or
    /// inline code span — fallback extraction already masks these, and a
    /// structured caller must not be able to route around that mask.
    #[test]
    fn a_supplied_mention_inside_a_code_span_is_dropped() {
        let text = "see `@engineer` for the review";
        let offset = text.find("@engineer").expect("span present");
        let supplied = vec![Mention {
            target: agent("engineer"),
            text: "@engineer".to_string(),
            offset,
            quiet: false,
        }];
        let out = resolve(text, Some(supplied), None, &acme(), &people());
        assert!(out.is_empty(), "{out:?}");
    }

    /// Two live targets can share an alias (two "Sam"s, say); a structured
    /// caller submitting the identical span for both must not double-ping —
    /// only the first-supplied target for that exact span survives.
    #[test]
    fn only_the_first_target_for_one_span_survives() {
        let users = vec![
            user("u1", "sam.one@acme.test", Some("Sam")),
            user("u2", "sam.two@acme.test", Some("Sam")),
        ];
        let supplied = vec![
            Mention {
                target: MentionTarget::User {
                    id: "u1".to_string(),
                },
                text: "@Sam".to_string(),
                offset: 0,
                quiet: false,
            },
            Mention {
                target: MentionTarget::User {
                    id: "u2".to_string(),
                },
                text: "@Sam".to_string(),
                offset: 0,
                quiet: false,
            },
        ];
        let out = resolve("@Sam please review", Some(supplied), None, &acme(), &users);
        assert_eq!(
            out.len(),
            1,
            "one run of text cannot name two different people: {out:?}"
        );
        assert_eq!(
            out[0].target,
            MentionTarget::User {
                id: "u1".to_string()
            },
            "the picker's own ordering decides which of the pair is honoured: {out:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Unicode
    // -----------------------------------------------------------------------

    /// A display name that starts with a non-ASCII letter is still a real
    /// alias `directory` offers verbatim — extraction must open a mention on
    /// it exactly as it does on an ASCII one.
    #[test]
    fn a_non_ascii_display_name_opens_a_mention() {
        let users = vec![user("u1", "elodie@acme.test", Some("Élodie"))];
        let found = resolve("hey @Élodie, can you look?", None, None, &acme(), &users);
        assert_eq!(
            targets(&found),
            vec![&MentionTarget::User {
                id: "u1".to_string()
            }],
            "{found:?}"
        );
    }

    /// A multi-byte character landing where a short alias's span would end
    /// must not panic — it simply does not match, the same as any other
    /// non-matching text.
    #[test]
    fn a_multibyte_character_at_a_short_aliass_boundary_does_not_panic() {
        let users = vec![user("u1", "j@acme.test", Some("J"))];
        // "é" is two UTF-8 bytes; a one-character alias ("j") ends inside it.
        let found = resolve("@é hello", None, None, &acme(), &users);
        assert!(found.is_empty(), "{found:?}");
    }
}
