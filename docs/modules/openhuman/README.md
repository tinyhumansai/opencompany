# OpenHuman Module

The openhuman module owns launcher and integration seams for the vendored
OpenHuman checkout at `vendor/openhuman`. It intentionally shells out through
Cargo for now, because OpenHuman is a full application crate and should not be a
heavy default dependency of the OpenCompany library.

Use `opencompany open-human --dry-run` to inspect the generated command before
running the vendored checkout.
