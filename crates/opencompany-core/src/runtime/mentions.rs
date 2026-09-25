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
//! body text ──► strip_code_regions ──► scan ────────────────┐
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
/// [`scan`] then refuses it. Deduplicating here — keeping the
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

/// An `@name` that named **more than one** thing and therefore named nobody
/// (B-101).
///
/// The refusal is right — see the module docs, never guess a ping — but until
/// this existed it was communicated only by the *absence* of a chip. An absence
/// is invisible in a wall of text and completely invisible to anyone posting
/// over the API, so the sender's ping reached no one and nothing said so; the
/// channel's catch-all answered instead and spoke about the person in the third
/// person. Reporting the refusal is what turns a silent negative into something
/// a sender can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousMention {
    /// The literal span, exactly as typed — `"@Priya"`, `@` included.
    pub text: String,
    /// Byte offset of the `@` in the body the scan was given, on the same terms
    /// as [`Mention::offset`].
    pub offset: usize,
    /// The two claimants whose collision refused the span, in the order the
    /// scan met them.
    ///
    /// Two, not all of them: the scan short-circuits on the first collision,
    /// because by then the answer is already "nobody" and reading on would
    /// change nothing. A name claimed by three things reports the first two —
    /// enough to say what happened, and it keeps this report strictly additive
    /// to which spans resolve.
    pub targets: Vec<MentionTarget>,
}

/// One pass of [`scan`]: what resolved, and what was refused for being
/// ambiguous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extraction {
    /// One [`Mention`] per `@` that named exactly one thing.
    pub mentions: Vec<Mention>,
    /// One [`AmbiguousMention`] per `@` that named several. Empty in the
    /// overwhelmingly common case, which is why it costs callers nothing to
    /// ignore it — and why nothing did, for as long as it did not exist.
    pub ambiguous: Vec<AmbiguousMention>,
}

/// What a mention target is, in one noun phrase a sender will recognise.
///
/// A kind, not an identity. Naming the two colliding rows would need their
/// display labels, and the whole reason the span was refused is that those
/// labels are **the same string** — "Priya and Priya" tells a sender nothing.
/// What actually resolves their confusion is that one is a teammate and one is
/// a person, which is also exactly what the picker shows on its two rows.
fn target_noun(target: &MentionTarget) -> &'static str {
    match target {
        MentionTarget::Agent { .. } => "a teammate",
        MentionTarget::User { .. } => "a person",
        MentionTarget::Desk { .. } => "a desk",
        MentionTarget::Everyone => "the whole channel",
    }
}

/// The line posted back into a conversation when an `@name` reached more than
/// one thing and therefore reached nobody (B-101).
///
/// It has to carry three things, because the sender can act on all three: the
/// literal they typed, what it collided with, and the one move that fixes it.
/// "Never guess a ping" is the right rule and this does not soften it — the
/// message still pinged nobody. It only stops that being something the sender
/// has to deduce from a chip that never appeared.
///
/// Returns `None` for an empty list, so the caller posts nothing rather than an
/// empty line — which is every message ever sent, bar these.
pub fn ambiguity_note(refused: &[AmbiguousMention]) -> Option<String> {
    let (first, rest) = refused.split_first()?;
    let names = |a: &AmbiguousMention| a.text.clone();
    let head = if rest.is_empty() {
        let mut nouns = first.targets.iter().map(target_noun);
        match (nouns.next(), nouns.next()) {
            (Some(one), Some(two)) if one == two => {
                format!("{} matches two of these here", names(first))
            }
            (Some(one), Some(two)) => format!("{} matches {one} and {two} here", names(first)),
            // Unreachable by construction — a collision has two claimants — and
            // still worded rather than panicked, because a sentence nobody sees
            // is cheaper than an `unwrap` somebody eventually does.
            _ => format!("{} matches more than one thing here", names(first)),
        }
    } else {
        let all: Vec<String> = refused.iter().map(names).collect();
        format!("{} each match more than one thing here", all.join(", "))
    };
    let subject = if rest.is_empty() { "it" } else { "they" };
    Some(format!(
        "{head}, so {subject} pinged nobody. Pick the one you mean from the @ list and send again — \
         a name that reaches two people is never guessed at."
    ))
}

/// Resolve `text` against `dir`, reporting both what each `@` named and what it
/// refused to name.
///
/// # Longest alias wins
///
/// Aliases are tried longest-first, so a company with both `Ann` and `Ann Lee`
/// resolves `@Ann Lee` to Ann Lee rather than to Ann with a stray `Lee` after
/// it. Without this the shorter name always wins, because it matches first.
///
/// # An ambiguous name resolves to nothing — and says so
///
/// When one alias reaches two targets — two people called "Sam", a desk and a
/// teammate sharing a name — the span is skipped entirely and stays literal
/// text. See the module docs: never guess a ping. It also lands in
/// [`Extraction::ambiguous`], so the caller can tell the sender their ping went
/// nowhere instead of leaving them to infer it from a missing chip (B-101).
///
/// Offsets in the returned mentions index `text`, so pass the **original**
/// body, not a stripped copy, when the offsets have to line up with what a
/// reader sees. Callers that want code regions ignored should mask with
/// [`strip_code_regions`] first, which preserves offsets exactly so both hold.
pub fn scan(text: &str, dir: &[MentionAlias]) -> Extraction {
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
    let mut refused: Vec<AmbiguousMention> = Vec::new();
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
        // The collision that refused this span, when there was one: the end
        // offset it claimed and the two claimants (B-101). `Some` is exactly
        // the old `ambiguous = true`, carrying what it refused so the caller
        // can say so.
        let mut ambiguous: Option<(usize, [MentionTarget; 2])> = None;
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
                    ambiguous = Some((end, [prev.clone(), (*target).clone()]));
                    break;
                }
                Some(_) => break,
            }
        }

        if let (Some((end, target)), None) = (matched, &ambiguous) {
            out.push(Mention {
                target: (*target).clone(),
                text: text[i..end].to_string(),
                offset: i,
                quiet: false,
            });
            i = end;
            continue;
        }
        // Refused for ambiguity: still literal text, and now also reported, so
        // the sender learns their ping reached nobody rather than inferring it
        // from a chip that never appeared (B-101).
        if let Some((end, targets)) = ambiguous {
            refused.push(AmbiguousMention {
                text: text[i..end].to_string(),
                offset: i,
                targets: targets.to_vec(),
            });
        }
        // Unresolved or ambiguous: leave it as text, and skip past this `@` so
        // a longer alias starting mid-word cannot re-match inside it.
        i = after;
    }
    Extraction {
        mentions: out,
        ambiguous: refused,
    }
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
/// neither) and folds ASCII case, mirroring [`scan`]'s own
/// matching rule so a span the extractor would have accepted is never rejected
/// here. `Everyone` and `Desk` targets are covered the same way, since a
/// caller can misclaim those exactly as easily as an agent or a user.
fn is_valid_alias_for(mention: &Mention, dir: &[MentionAlias]) -> bool {
    // Strips `@` and, for the desk spelling `opens_mention`/`scan`
    // both accept, the `#` right after it too — `@#engineering` must compare
    // against the same `"engineering"` alias `@engineering` does, not against
    // `"#engineering"`, which is nobody's alias and would fail every desk
    // mention the console's own picker can produce for that spelling.
    let body = mention.text.strip_prefix('@').unwrap_or(&mention.text);
    // `@#…` is the desk-only spelling. `scan` narrows a hashed
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
///
/// [`resolve_reporting`] with the refusals dropped — what every caller that
/// only needs to route should use.
pub fn resolve(
    text: &str,
    supplied: Option<Vec<Mention>>,
    sender: Option<&Actor>,
    record: &CompanyRecord,
    users: &[UserRecord],
) -> Vec<Mention> {
    resolve_reporting(text, supplied, sender, record, users).mentions
}

/// [`resolve`], also reporting every `@name` that reached more than one thing
/// and therefore reached nobody (B-101).
///
/// # Routing comes from the caller; ambiguity is always the host's to judge
///
/// A `supplied` list still decides **who is pinged**, exactly as before — the
/// picker asked and the click answered, and nothing here second-guesses it.
/// But a supplied list is not evidence that nothing was refused, and treating
/// it that way is what made this bug invisible from the surface it was reported
/// on: the console runs the *same* refusal rule in `resolvableMentions`, and a
/// loaded directory then sends `[]` explicitly to suppress host extraction
/// (`MessageComposer`: "Preserve absent-versus-empty"). So a free-typed
/// `@Priya` was refused by the console, arrived here as "the picker resolved
/// nothing", and the host had nothing to report — the console's silent refusal
/// wearing the host's clothes.
///
/// So the scan runs either way, and its refusals are reported for every span
/// the supplied list did **not** claim. A span the picker did resolve is not
/// ambiguous — the operator settled it — so it is filtered out by overlap,
/// which is also what keeps a picked `@Priya` from being told it went nowhere.
///
/// The cost is one extra scan on the picker path: pure string work over the
/// directory, the same pass the extraction path has always run.
pub fn resolve_reporting(
    text: &str,
    supplied: Option<Vec<Mention>>,
    sender: Option<&Actor>,
    record: &CompanyRecord,
    users: &[UserRecord],
) -> Extraction {
    // Offsets survive the mask, so every span is re-read from the real body —
    // the masked copy has spaces where a code region was.
    let masked = strip_code_regions(text);
    let real_span = |offset: usize, len: usize, fallback: String| {
        text.get(offset..offset + len)
            .map(str::to_string)
            .unwrap_or(fallback)
    };
    let scanned = scan(&masked, &directory(record, users));

    let found = match supplied {
        Some(supplied) if !supplied.is_empty() => revalidate(text, supplied, record, users),
        // An explicitly empty list is still an answer — a console that ran its
        // picker and found nothing must not have the host guess on its behalf.
        Some(_) => Vec::new(),
        None => scanned
            .mentions
            .into_iter()
            .map(|mut m| {
                m.text = real_span(m.offset, m.text.len(), m.text);
                m
            })
            .collect(),
    };

    // Byte ranges the caller's own resolution claims. A refusal overlapping one
    // of them was settled by whoever supplied it and is not reported.
    //
    // Built only from the **non-quiet** entries of `found` (Codex review): a
    // picker-supplied mention `revalidate` demoted (its target renamed,
    // removed, or resolved to a different roster entry since the picker ran)
    // keeps its span in `found` — `revalidate` demotes rather than drops it,
    // so the composer can still show what the operator *typed* — but that
    // span was never actually delivered to anyone, `quiet` is exactly what
    // says so. Counting it as claimed anyway suppressed the live scan's own
    // finding at the same span: an old alias that now names two live targets
    // pinged nobody (quiet) and reported nothing (falsely claimed), the exact
    // silent failure B-101 exists to surface.
    let claimed: Vec<(usize, usize)> = found
        .iter()
        .filter(|m| !m.quiet)
        .map(|m| (m.offset, m.offset + m.text.len()))
        .collect();
    let ambiguous = scanned
        .ambiguous
        .into_iter()
        .filter(|a| {
            let (start, end) = (a.offset, a.offset + a.text.len());
            !claimed.iter().any(|(s, e)| start < *e && *s < end)
        })
        .map(|mut a| {
            a.text = real_span(a.offset, a.text.len(), a.text);
            a
        })
        .collect();

    Extraction {
        mentions: normalize(found, sender),
        ambiguous,
    }
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
///
/// # Naming an outsider does not summon them into the room
///
/// The candidate must be a member of `desk`. Roster membership alone was the
/// old test, and it let `@product_designer` typed in `#engineering` make the
/// designer answer *in #engineering* — a desk they are not on, in front of a
/// room they are not part of, while engineering's own lead stayed silent about
/// a question addressed to their channel.
///
/// That is the same defect the cross-desk referral exists to fix one level up,
/// and `tinyhivemind` already states the rule: an outsider "runs on their OWN
/// desk, not as a guest here". A mention that names one therefore resolves to
/// nobody and the ladder continues to the desk's own answerer, who can carry
/// the question across on the referral path — one agent speaking in both rooms,
/// and it is the one that belongs to this one.
///
/// **A channel with no membership is unrestricted**, which is not a loophole
/// but the same rule: the built-in `#general` is not a desk and has no members
/// to be outside of (issue #1743), and neither has a DM. `resolve_desk_id`
/// returning `None` is how both say so, and there the old behaviour is the
/// correct one.
pub fn mention_responder(
    record: &CompanyRecord,
    desk: Option<&str>,
    mentions: &[Mention],
) -> Option<String> {
    let members = desk
        .and_then(|desk| record.resolve_desk_id(desk))
        .map(|desk_id| record.effective_desk_members(&desk_id));
    mentions
        .iter()
        .filter(|m| !m.quiet)
        .filter_map(|m| m.target.agent_id())
        .find(|id| {
            record.is_roster_agent(id)
                && members
                    .as_ref()
                    .is_none_or(|members| members.iter().any(|member| member == id))
        })
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
#[path = "mentions_tests_core.rs"]
mod tests_core;
#[cfg(test)]
#[path = "mentions_tests_part1.rs"]
mod tests_part1;
#[cfg(test)]
#[path = "mentions_tests_part2.rs"]
mod tests_part2;
