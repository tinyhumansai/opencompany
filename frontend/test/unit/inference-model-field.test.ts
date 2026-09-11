// When the model field is a catalog select and when it is a text box.
//
// Four reasons roll into one answer and each is a decision somebody could get
// wrong, so it is a function rather than an expression inside the component —
// and this is what the component would otherwise have to be driven through a
// DOM to check.

import { describe, expect, it } from "vitest";

import { showsCatalogSelect } from "@/inference/ModelField";

const loaded = { models: ["gpt-5", "gpt-5-mini"], freeTextOnly: false };

describe("showsCatalogSelect", () => {
  it("shows the select once a catalog has loaded", () => {
    expect(showsCatalogSelect({ catalog: loaded, typed: false, value: "" })).toBe(true);
    expect(showsCatalogSelect({ catalog: loaded, typed: false, value: "gpt-5" })).toBe(true);
  });

  it("stays text while the catalog is still unknown", () => {
    // An empty list that fills in underneath a click is worse than a text box.
    expect(showsCatalogSelect({ catalog: null, typed: false, value: "" })).toBe(false);
  });

  it("stays text when the endpoint publishes nothing usable", () => {
    // An empty select reads as "this provider has no models", which nobody
    // established — the caller renders the host's reason beside it instead.
    expect(
      showsCatalogSelect({ catalog: { models: [], freeTextOnly: false }, typed: false, value: "" }),
    ).toBe(false);
  });

  it("stays text at an endpoint that routes on a deployment name", () => {
    // Azure publishes base model ids and routes on deployment names, so a
    // closed list makes the only correct value unreachable.
    expect(
      showsCatalogSelect({
        catalog: { models: ["gpt-5.6-terra-2026-07-09"], freeTextOnly: true },
        typed: false,
        value: "",
      }),
    ).toBe(false);
  });

  it("never upgrades out from under a value the list does not contain", () => {
    // Something typed by hand, or an id from a stale catalog. Switching to a
    // select would discard it — the same class of bug as stripping a value
    // mid-keystroke, which this feature has nine recorded instances of.
    expect(showsCatalogSelect({ catalog: loaded, typed: false, value: "my-deployment" })).toBe(
      false,
    );
  });

  it("lets the operator's own choice of the text field win", () => {
    expect(showsCatalogSelect({ catalog: loaded, typed: true, value: "" })).toBe(false);
  });
});
