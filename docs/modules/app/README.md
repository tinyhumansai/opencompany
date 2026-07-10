# App Module

The app module owns process-level configuration and shared Axum state. Keep it
small: it should compose module status and runtime configuration without
absorbing domain behavior from OpenHuman or the `tiny*` crates.
