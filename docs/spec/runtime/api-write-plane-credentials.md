# Credential-bearing console writes

The console write routes that put something in the `SecretStore`: the company's
TinyHumans key, the custom domain, SMTP, and the retired native OAuth bridge
that deliberately writes nothing.

Split out of [`api-write-plane.md`](api-write-plane.md), which was at the
repository's 500-line ceiling. That file is the write plane's route map; this
one is the subset that handles secrets.

## Credential-bearing surfaces (feature-gated)

These write secrets to the `SecretStore` and expose only non-secret status. The
native OAuth compatibility routes below deliberately **do not** write a
credential: the old credential was unreachable by agents.

```text
GET    …/credential                         whether the company has its own key + which tier it presents
PUT    …/credential                         set / rotate / clear the company's TinyHumans key  [admin]
POST   …/credential/link/start              begin a PKCE key grant; answers the hub URL to navigate to  [admin]
POST   …/credential/link/finish             redeem the returned code; stores the minted key  [admin]
GET    /auth/key/callback                    the host's own return leg for a key grant (desktop); trust is the parked `state`
GET    …/domain                             the stored domain + records + last verify result, or `null`
PUT    …/domain                             set the custom domain  [admin]
POST   …/domain/verify                       server-side DNS check
GET    …/smtp                               non-secret SMTP status (`configured: false` when unset)
PUT    …/smtp                               store SMTP credentials (secret store)  [admin]
POST   …/smtp/test                           send a test email  [admin]
POST   …/connections/{provider}/start        retired native OAuth bridge → 410 JSON until 2026-09-30  [feature: oauth]
POST   …/connections/{provider}/disconnect   drop a legacy stored OAuth token  [feature: oauth]
GET    /api/v1/oauth/callback                retired browser landing page → 410 HTML until 2026-09-30  [feature: oauth]
```

The two `GET`s are the REST siblings of the GraphQL `Company.domain` /
`Company.smtp` reads and share their loaders, so the planes cannot disagree
about the fields they both carry. They can still differ in *detail*: REST
answers the full `DomainStatus` and `SmtpStatus`, while `DomainStatusGql` omits
the per-record `checks` from the last verify pass and `SmtpStatusGql` omits
`security`, `from_name` and `from_email`. Both are open to any member (the
`[admin]` line guards the company's outward identity, not the reading of it) and
neither carries credential material: the SMTP password is absent from
`SmtpStatus` by construction.

`PUT …/smtp` treats the password as a **patch** — a body that omits it keeps the
stored one, so a form can offer "stored — leave blank to keep" instead of
charging a credential re-entry for a from-name fix. A body carrying one behaves
exactly as before, and one that supplies neither with nothing stored is `400`.
A supplied password is stored **byte for byte** — leading and trailing
whitespace is preserved, because it can be significant to the remote server.
Trimming decides only *whether* one was supplied: a value that is empty or
entirely whitespace counts as omitted and keeps the stored password, so an
all-whitespace password cannot be set through this route. Any value with a
non-whitespace character in it is stored exactly as sent.

Keeping the stored password costs no read-modify-write. The configuration and
the password live under separate secret keys, so a passwordless save rewrites
the configuration and never touches the secret — a rotation arriving at the same
moment survives instead of being reverted, however many processes are writing.

Credentials written before that split still carry the password inside the
configuration blob; reads fall back to it, and the first passwordless save after
the split migrates it to its own key. It is **read-only**: the configuration
blob is rewritten on every save without it, guaranteed by
`#[serde(skip_serializing)]` on the field rather than by every construction site
remembering to pass `None` (issue #1770). Writing it back would not only put a
credential in the blob, it would overwrite the pre-split password that the
legacy read path still depends on. That migration is the one path that must
read and then write, so `PUT …/smtp` serializes per company for the duration of
the handler. The lock is in-process, which covers the deployed topology (a
tenant is a single container); two replicas of one company would reopen the
window on the legacy path alone, and closing it there would need a conditional
write that `SecretStore` cannot express today.

`…/credential` is the company's **one** TinyHumans key, presented by every
surface wired to it — [`credentials.md`](credentials.md) has the resolution
order, the rotation guarantee, and where a grant's return leg lands.

## Retired native OAuth callback

`/api/v1/oauth/callback` stays reachable for a browser that began consent just
before a deploy. It returns a non-caching `410 Gone` HTML page saying the
authorization was not saved, why native OAuth cannot make agents able to use
the provider, and to use Composio instead — ignoring the provider's `code` and
`state` rather than exchanging or storing them.

`POST …/connections/{provider}/start` is likewise a `410 Gone` JSON response
with stable code `native_oauth_retired`, a message, and `removalAfter:
"2026-09-30"`. Both send `Deprecation: true` and a `Sunset: Wed, 30 Sep 2026
00:00:00 GMT` header. #1023 removes the bridge after the cache window
established by #979, keeping Disconnect and the read projection so tenants can
release credentials written before #828.
