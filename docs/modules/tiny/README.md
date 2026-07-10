# Tiny Module

The tiny module owns integration metadata for vendored runtime sources:

- `tinyagents`
- `openhuman`

TinyAgents is an optional dependency behind the `tiny` feature so the default
OpenCompany build stays fast. OpenHuman is tracked as a git submodule and
launched through Cargo rather than compiled into the OpenCompany library by
default.
