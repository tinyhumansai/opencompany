# Privacy Scrubbing

Feedback goes to a **public** issue tracker; the scrubber is the
non-negotiable gate between a company's private context and the internet.
This document is normative.

## Redaction classes

Before anything leaves the machine, the scrubber MUST remove or mask:

| Class | Examples | Strategy |
| --- | --- | --- |
| **Secrets** | TinyHumans credential, GitHub tokens, channel HMACs, anything from `SecretStore` | exact + entropy-pattern match; a `SecretStore` value appearing anywhere aborts filing outright |
| **Wallet material** | seeds, private keys, delegated-signer grants; wallet addresses | keys abort; addresses masked (`sol:…abcd`) |
| **Personal data** | emails, phone numbers, personal names, physical addresses | pattern + roster/contact-list match → `⟨redacted:email⟩` placeholders |
| **Customer content** | message bodies, documents, deliverables belonging to the company's customers | never included; excerpts come only from runtime/brain output, not customer input |
| **Charter specifics** | prices, client lists, never-do rules, mission text | replaced by structural descriptions ("a priced skill", "a standing deny rule") |

What remains — and is all a useful issue needs: the category, the operator's
own words (scrubbed), template name + version, runtime version, the effect
kind or route involved, and error codes.

## The preview gate (normative)

- The operator MUST be shown the **exact final issue body** — post-scrub,
  byte-for-byte what will be posted.
- Nothing is transmitted without explicit confirmation or a standing
  per-category auto-consent ([README.md](README.md)); auto-consent filings
  are journaled and the operator can review every filed body after the fact.
- Scrubbing failures fail **closed**: if a redaction class can't be
  evaluated (e.g. secrets store unreadable), filing is blocked, not risked.

## Data minimization

- Issues carry the minimum to reproduce: prefer structural description over
  raw excerpt; excerpts are capped and always scrubbed.
- The unscrubbed original stays **local** in the feedback family — the
  maintainers can ask for more via the issue thread, and the operator
  decides again.
- Deletion rights ([company-brain/memory.md](../company-brain/memory.md))
  cover feedback items; deleting one does not delete the public issue (the
  operator is told this at preview time).
