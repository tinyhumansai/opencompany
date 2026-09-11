# Provider brand marks

Locally bundled, **never fetched at runtime**. A console that pulled brand logos
from a CDN would tell whoever hosts that CDN which model providers a company is
looking at, one request per row — and the marks would vanish on an offline host.

## Where each file came from

- `anthropic.svg`, `apple.svg`, `google.svg`, `huggingface.svg`, `nvidia.svg`,
  `ollama.svg`, `openai.svg`, `vercel.svg`, `xai.svg` — **Simple Icons**, via
  `react-icons@5.5.0`'s `si` set. The path data was extracted and written out as
  standalone SVGs rather than adding the dependency: `react-icons` is 83 MB
  installed and we need ten marks from it. `lucide-react`, which this console
  already uses, carries no brand marks.
- Everything else — carried across from openhuman's
  `app/src/assets/provider-icons/` at `5e543a76b`, unmodified.

## Licensing

Simple Icons publishes its icon files under **CC0 1.0**. The marks themselves
remain the trademarks of their respective owners and are used here only to
identify the provider a row refers to — nominative use — not to imply any
endorsement or affiliation.

Nothing in this directory is hand-traced. A provider with no sourced mark gets a
lettered swatch instead, which is why there is no `mistral.svg`: the Simple Icons
release we extracted from does not carry one, and drawing an approximation of
another company's logo would be worse than a letter that is at least unambiguous.

To remove a mark, delete the file and its entry in
`frontend/src/inference/provider-icon.tsx`. The letter takes over on its own.
