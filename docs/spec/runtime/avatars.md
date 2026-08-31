# Faces

Which icon a teammate wears, and which one you do.

Everybody in a company already has a face. The console hashes a stable id into
one of eleven mascots shipped in `frontend/public/avatars/` and draws that, which
is why a company nobody has customised reads as a roster of individuals rather
than a column of grey squares. This page is about the other half: what happens
when somebody **chooses**.

## The grammar

A chosen face is stored as one short string in exactly one of two forms:

| Form | Means |
|---|---|
| `tiny:<flavour>` | one of the eleven shipped mascots — a flavour of tiny |
| `blob:<nodeId>` | a custom image somebody uploaded, held as a binary workspace node |

Absent is a **third state and the default**: *nobody has chosen*. It is
deliberately distinct from either stored form, because "put the default face
back" has to be expressible and neither `tiny:` nor an empty string can express
it. Every read skips the key rather than defaulting it, and every write treats
`null` — and a blanked input, which is the same intent typed — as the reset.

The flavour list lives in `src/company/avatar.rs` (`TINY_FLAVOURS`) and is
mirrored in `frontend/src/lib/avatar.ts`. They are kept in step by
`frontend/test/unit/avatar-reference.test.ts`, which reads the Rust source: a
flavour one side accepts and the other has no file for renders as a broken image
on every surface at once, and nothing else in the build would notice.

### Why the grammar is closed

The obvious design is to store a URL. That is what this refuses, and the reason
is where the value ends up: an `src=` attribute on every console surface that
draws a face — chat gutters, facepiles, the org chart, the members pane, the
approvals list. A stored URL is therefore an instruction the console obeys on
behalf of whoever wrote it. `javascript:` is script injection.
`http://tracker.example/x.gif` is a beacon that fires for every viewer and
reports who looked at the roster and when. Either outlives the account that set
it.

Both accepted forms name something **this host already holds**, so rendering one
reaches nothing the viewer's session did not already reach.

The same reasoning is why a `blob:` reference is validated against its
*referent*, not just its shape (`avatar::resolve`): any member can type a node
id, and one pointed at a 60 MB PDF would make every face on the page try to
decode a PDF as an image, for everyone, on every load.

## Uploads

```text
POST …/avatars    multipart `file` → { avatar, nodeId, mime, size }
```

The bytes land as an ordinary binary workspace node in an `avatars/` folder, and
are read back through `GET …/workspace/blob/{nodeId}` — there is no second read
path, so the `nosniff` and inline-renderable rules that route argues for cannot
be got wrong twice.

Three things this route does that `POST …/workspace/upload` deliberately does
not:

- **The type is sniffed, not believed.** PNG, JPEG, GIF and WebP each begin with
  an unambiguous signature. A declared content type is a claim by whoever is
  uploading, and an avatar is served back to every member of the company for as
  long as the teammate exists.
- **The ceiling is an avatar's.** 4 MB (`MAX_AVATAR_BYTES`), against the
  workspace's tens of megabytes for documents.
- **One folder**, so an operator can see — and delete — what the company holds
  without hunting through the tree.

**GIFs are first-class.** An avatar is a small square somebody picked to be
recognisable, and a moving one is more recognisable, not less. Nothing
transcodes, so an animated GIF or WebP is stored and served as the bytes that
arrived and animates wherever a face is drawn. **SVG is refused**: it is a
document format that can carry script and fetch remote resources, so accepting
one would reintroduce, inside a file, precisely what refusing URLs keeps out.

## Where a face is stored

| Subject | Field | Written by |
|---|---|---|
| A teammate | `AgentOverride::avatar` on the company record | `PATCH …/team/{agent_id}`, `POST …/team` |
| A person | `UserRecord::avatar` | `PATCH …/auth/me` |

A teammate's face rides on the **override** row rather than on `OverlayAgent`,
so one field answers for both kinds of teammate: an override may name a
manifest-declared agent or an operator-added one, and choosing a face is the same
act whichever was clicked. `company.toml` is never rewritten, exactly as for a
persona or a budget cap.

The reset paths are per **field**, not per row: `clear_agent_avatar` drops the
face and leaves the persona, and `clear_agent_override` drops the persona and
leaves the face. A shared `retain_nonempty_agent_edits` is what removes a row
that ends up carrying nothing.

### Who may set what

A teammate's face is editable by any member, not only an admin. Picking a
colleague's icon is not a privilege boundary the way widening a tool grant is,
and a company whose only admin is away should not be stuck with eleven hashed
blobs. `tools` (a grant), `model` (a cost/scope choice) and `harness` (a routing
binding) remain the admin-gated fields on that route.

A **person's** face is writable only by that person. `PATCH …/users/{id}` is
admin-only and can set somebody's `displayName` — right for making a roster of
raw addresses legible — but a face is theirs to pick, so it is not on that route
at all. `PATCH …/auth/me` takes no user id in its path, which is what makes it
impossible to point at somebody else.

## Names, and why they are guessed rather than stored

A person who has not named themselves still has to be called something on every
surface that shows them. The honest options are the raw address, nothing, or a
guess. The raw address is refused on this project's own rule — being in a company
should not hand everyone your mailbox — and nothing leaves a chat message
attributed to a blank.

So: a guess, made at render time. `steven.enamakel@acme.com` reads as "Steven
Enamakel" — the local part only, split on the separators people use, each word
capitalised, the domain dropped. It lives in `derive_display_name`
(`src/ports/users.rs`) and is mirrored in `frontend/src/lib/person.ts`.

**It is never written into `display_name`.** That field means *what this person
chose*, and filling it with a guess would make the two indistinguishable: nothing
could tell somebody who had set their name from somebody who had never opened
the dialog, and whatever the guess was on the day they first signed in would be
frozen into the directory.

The guess refuses to guess where there is nothing to find — a wallet key, the
local owner of a company with no sign-in, a local part with no letters. `None`
there means "cannot say", and a caller renders something honest rather than a
name-shaped string that is not a name.

## The device suggestion

On the desktop, the profile dialog offers what the machine already knows: the
account's full name, and its account picture. Read through
`oc_device_identity` → `src-tauri/src/identity.rs`, per platform (GECOS on Linux,
`dscl` on macOS, `%PUBLIC%\AccountPictures` on Windows), every field optional and
every failure silently `None`.

It is **offered, never applied**. Nothing is stored until a person clicks it,
at which point what lands is a decision rather than their laptop's idea of who
they are published to their colleagues. The picture goes through the same
`POST …/avatars` upload as one chosen from a file dialog — sniffed, capped,
stored as bytes this host holds — because a `data:` URL is not a reference this
grammar accepts, and there is deliberately no shortcut that would make it one.

A browser reads none of this and should not be able to. There the dialog simply
starts from the name derived from the sign-in address.
