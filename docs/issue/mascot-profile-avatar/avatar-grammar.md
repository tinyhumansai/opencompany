# Extending the avatar-reference grammar

## Why a third closed form, not an upload

Today `crates/opencompany-core/src/company/avatar.rs` parses a stored avatar
reference into exactly two forms (`AvatarRef::Tiny(&str)` /
`AvatarRef::Blob(&str)`), enforced identically on the host and in
`frontend/src/lib/avatar.ts`. The module doc is explicit about why the
grammar is closed at all: a stored value ends up in an `src=` attribute on
every console surface that draws a face, so it must name something the host
already holds rather than an arbitrary URL — and SVG is refused for the same
reason one level down, because it's a document format that can carry script
and fetch remote resources, not a raster image.

A `.riv` file is exactly that class of thing: a programmable, document-like
format with its own runtime, not a raster image `sniff_image` can validate
by signature and dimensions. Accepting one as an arbitrary `blob:`-style
upload would reopen exactly the risk SVG was excluded for. The consistent
move is to treat it like `tiny:` instead — one curated file shipped with the
console, named by a closed enum, never user-uploaded.

## The change

Add a third variant, mirroring `Tiny` exactly:

```rust
pub enum AvatarRef<'a> {
    Tiny(&'a str),
    Blob(&'a str),
    Mascot(&'a str),   // new
}

pub const MASCOT_KINDS: [&str; 1] = ["animated"];   // v1: one kind
```

`parse()` gains a `strip_prefix("mascot:")` branch shaped exactly like the
`tiny:` branch — validated by list membership, not by any workspace lookup.

**`resolve()` needs no code change.** Its existing early return —

```rust
let AvatarRef::Blob(node_id) = parse(&stored)? else {
    return Ok(stored)
};
```

— already passes any non-`Blob` variant straight through untouched. This is
true for `Tiny` today and stays true for `Mascot` with zero new lines in
that function.

Shipping `MASCOT_KINDS` as a list of one (`["animated"]`) rather than a bare
boolean flag is deliberate even though v1 only has one member: it keeps the
same shape `TINY_FLAVOURS` uses (closed enum, validated membership, doc
comment pinning it to shipped files), so a second colorway or a second
character later is an addition to the list, not a grammar change.

## Everywhere "two forms" is hardcoded

Five places currently say "two forms" or "either stored form" and would
become actively wrong the moment a third lands. All five need updating in
the same change, not as an afterthought:

1. `avatar.rs` module doc comment, lines ~9–21 (`## The grammar` /
   `## Why the grammar is closed` — "in exactly one of two forms",
   "deliberately distinct from either stored form").
2. `refusal()` (`avatar.rs` line ~743) — the error text says
   `"tiny:<flavour>" ... or "blob:<nodeId>" ...` and should name the third.
3. The test named `refuses_anything_that_is_not_one_of_the_two_forms` in
   `avatar_tests_reference.rs` — rename (`...one_of_the_three_forms`) or
   split, since its hostile-input table exists to prove rejection of things
   that are *none* of the forms, which doesn't itself need new cases, but
   its name would misdescribe the grammar it's testing against.
4. `frontend/src/lib/avatar.ts` file header comment (mirrors #1).
5. `docs/spec/runtime/avatars.md` — the `## The grammar` table and
   `### Why the grammar is closed` section, which currently document only
   `tiny:`/`blob:`; needs a third row and a short paragraph explaining the
   `mascot:` form's "curated, not uploaded" posture, matching the rigor the
   existing SVG-refusal paragraph already models.

## Test mirrors needed

Read all three Rust test files plus the frontend cross-check test in full
before scoping this. Findings:

**`avatar_tests_reference.rs`** (parsing/grammar) — needs the `Tiny`
coverage mirrored for `Mascot`:
- `accepts_every_shipped_mascot_kind` — round-trip every `MASCOT_KINDS`
  entry through `normalize()`/`parse()` → `AvatarRef::Mascot(kind)`, mirroring
  `accepts_every_shipped_flavour`.
- `refuses_a_mascot_kind_with_no_file` — `parse("mascot:bogus")` errors,
  message names the bad kind and lists a valid one, mirroring
  `refuses_a_flavour_with_no_file`.
- Extend `refuses_an_unbounded_string` and `trims_on_the_way_in` with a
  `mascot:` case each (or generalize both to loop over all three prefixes).
- The existing hostile-input table doesn't need new entries — path-traversal
  or malformed `mascot:` values are already rejected the same way malformed
  `tiny:` values are, by failing list membership rather than needing a
  dedicated character-set check the way `blob:` does.

**`avatar_tests_formats.rs`** (image sniffing / decompression-bomb checks) —
**no changes.** This file is entirely about the `Blob`/binary-image path;
`Mascot`, like `Tiny`, never carries or sniffs bytes.

**`avatar_tests_resolve.rs`** (the async `resolve()` lookup path) — **no
changes required**, since `resolve()`'s early return already covers any
non-`Blob` variant. Worth adding one explicit smoke test —
`resolve_passes_a_mascot_reference_through_untouched` — confirming
`resolve()` never touches the fake `ScriptedStore` for a `mascot:` ref, since
there isn't currently an equivalent explicit test for `Tiny` either; this
would be new coverage, not strictly a mirror of existing coverage.

**`frontend/test/unit/avatar-reference.test.ts`** — this test keeps
`TINY_FLAVOURS` in sync between Rust and TypeScript by **reading
`avatar.rs` as raw text and regex-scraping the array literal out of it**
(`/pub const TINY_FLAVOURS: \[&str; \d+\] = \[([^\]]*)\]/s`), then comparing
the extracted list, sorted, against the TS constant — not an import, not
codegen, not a shared schema file. It also asserts every flavour has a
backing file on disk via `existsSync`. A `MASCOT_KINDS` const needs the same
treatment: a second regex scrape, a second sorted-equality assertion, and a
second file-backing check (against wherever `mascot.riv` lands). This also
means **`staticAvatarSrc` in `avatar.ts` needs a `mascot:` branch before
this new test can pass** — today it only branches on `trimmed.startsWith("tiny:")`
and returns `null` for anything else, which is exactly the behavior
[`rendering-strategy.md`](rendering-strategy.md) wants kept, but the test
still needs the branch to assert against.

## Who may set it

No change needed to the authorization model in
`docs/spec/runtime/avatars.md`: a teammate's face (including `mascot:`) is
editable by any member via the same `PATCH …/team/{id}` route a `tiny:`
choice already goes through, and a person's own face via `PATCH …/auth/me`.
