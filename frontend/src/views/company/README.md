# Company — the org chart

`#/company` draws the company's declared structure as a three-level tree and is
the one place desks are created, staffed and led.

## Why this exists

Issue #302 removed the Desk nav entry for the v1 IA cut and said what that cost:
"desk creation and membership editing become unreachable", editable by hand in
the manifest and nowhere else. It called that temporary.

By the time #311 was picked up it was worse than #302 described. The chat
rebuild removed `"desks"` from `app-shell.tsx`'s `View` union altogether, so
`DesksView.tsx` and `DeskCreateDialog.tsx` were imported by nothing and
`#/desks` silently rewrote to Overview. Five host routes with a full backend
test suite behind them were reachable from no UI by any path, including by
typing a URL.

Issue #311 is explicit that the answer is not to put the flat list back: "build
the hierarchy surface that makes the Desks page unnecessary, and route creation
and membership editing through it." So the old screen was retired into this one
rather than re-listed.

## Three levels, and why nothing enforces it

```text
company            level 1   the company itself
└── desk           level 2   a [[group_chat]], or an operator-created overlay desk
    └── seat       level 3   one teammate on that desk
```

The cap is **structural**. A desk cannot name a parent desk — neither
`GroupChat` nor `OverlayDesk` has such a field — so a fourth level is not
rejected by a check, it is unrepresentable. Nothing in `lib/org.ts` validates
depth, and nothing should: #311 settles that this is "a new reader over existing
data, not a data change", and a parent pointer is precisely the data change it
rules out.

`MAX_DEPTH` and `depthOf()` exist so the number can be read and asserted against
a real tree instead of trusted from a comment.

## Rules the host owns, that this surface must not re-derive

| | |
|---|---|
| **The lead is `members[0]`** | It is the host's routing target. Changing the lead is `PUT …/desks/{id}/order` moving somebody to the front; there is no set-lead call and the console must not invent one. Never re-derive the lead by sorting or by name. |
| **Provenance decides the controls** | The host refuses to delete a blueprint desk or remove a blueprint member at runtime. The chart offers neither, because a control that always fails is worse than none. `overlayCreated` marks the desk, `overlayMembers` marks the seats. |
| **A write is followed by a refetch** | Every write changes something the host derives — the effective member union, the order, the overlay subset. Patching locally would be a second implementation of rules the host already owns, and the two would drift. |

## What is drawn beside the tree, not in it

**Not on a desk** (roster teammates staffed nowhere) and **People** (the humans
who can sign in) are listed alongside the chart. Neither has a position the
company declares. Placing them under a node would be inventing structure, which
is the failure `views/overview/README.md` already documents about its own
keyword-derived departments — this surface exists to be the truthful one.

A seat whose id resolves to no roster teammate is a third case, and it is
handled the opposite way: it stays, badged "Not on the roster". The chat member
pane drops such an id, which is right for a chat — you cannot message nobody.
Here it is a fact about the structure that only the operator can fix.

## Files

| | |
|---|---|
| `OrgChartView.tsx` | The view, the tree, and the desk/seat controls. |
| `DeskCreateDialog.tsx` | The create form. Moved here from `views/`, where nothing had rendered it since #302. |
| `@/lib/org.ts` | The pure model: `buildOrgTree`, `depthOf`, `addableTo`, `reorderedIds`. All of it testable without a DOM. |

## Tests

`test/unit/org-tree.test.ts` covers the derivation — lead by position,
provenance, the unresolvable seat, the unplaced roster, and the cap.
`test/e2e/org-tree.spec.ts` covers the surface in a browser, including the
reachability failure this issue is about, and asserts every write survives a
reload against a stateful stub.

## Deliberately not here

- **Chat alignment** — whether a desk thread becomes a view of this tree
  (#311's third open question). Filed as #485.
- **The Overview graph's invented departments** — replacing them with this real
  structure belongs with the replacement, not the foundation. Filed as #486.
