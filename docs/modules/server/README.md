# Server Module

The server module owns the Axum HTTP surface. The initial routes are:

- `GET /healthz`
- `GET /spec`
- `GET /tiny`

Add future API routes as focused handler groups rather than wiring behavior
directly in the binary entrypoint.
