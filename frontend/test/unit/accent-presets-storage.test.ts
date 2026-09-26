// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  ACCENT_PRESET_STORAGE_KEY,
  applyAccentPreset,
  applyStoredAccentPreset,
  DEFAULT_ACCENT_PRESET,
  readStoredAccentPreset,
  setAccentPreset,
} from "@/lib/accent-presets";

/**
 * Storage and DOM-apply contract (issue #2493, test-plan U5). Mirrors the
 * failure modes `crash-fallback.tsx` already has to defend against for the
 * `"theme"` key — a private window's `localStorage` getter throws, not just
 * returns null — plus this feature's own: an unknown id (a retired preset)
 * must fall back rather than leave a dead attribute on `<html>`, and a write
 * to this key must never collide with `next-themes`' own `"theme"` key.
 */

beforeEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.accentPreset;
});

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.accentPreset;
  vi.restoreAllMocks();
});

describe("readStoredAccentPreset / applyStoredAccentPreset", () => {
  it("leaves the attribute absent when nothing is stored", () => {
    applyStoredAccentPreset();
    expect(document.documentElement.dataset.accentPreset).toBeUndefined();
  });

  it("applies a stored known id", () => {
    localStorage.setItem(ACCENT_PRESET_STORAGE_KEY, "indigo");
    applyStoredAccentPreset();
    expect(document.documentElement.dataset.accentPreset).toBe("indigo");
  });

  it("falls back to default, without throwing, for a retired/unknown stored id", () => {
    localStorage.setItem(ACCENT_PRESET_STORAGE_KEY, "no-longer-shipped");
    expect(() => applyStoredAccentPreset()).not.toThrow();
    expect(document.documentElement.dataset.accentPreset).toBeUndefined();
    expect(readStoredAccentPreset()).toBe(DEFAULT_ACCENT_PRESET);
  });

  it("does not throw when localStorage.getItem itself throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("blocked");
    });
    expect(() => applyStoredAccentPreset()).not.toThrow();
    expect(document.documentElement.dataset.accentPreset).toBeUndefined();
  });

  it("selecting default removes the attribute rather than setting it to the literal string", () => {
    document.documentElement.dataset.accentPreset = "rose";
    applyAccentPreset(DEFAULT_ACCENT_PRESET);
    expect(document.documentElement.dataset.accentPreset).toBeUndefined();
    expect(document.documentElement.hasAttribute("data-accent-preset")).toBe(false);
  });
});

describe("setAccentPreset", () => {
  it("writes to oc.appearance.accentPreset and never touches the theme key", () => {
    setAccentPreset("teal");
    expect(localStorage.getItem(ACCENT_PRESET_STORAGE_KEY)).toBe("teal");
    expect(localStorage.getItem("theme")).toBeNull();
  });

  it("removes the key entirely when the default is chosen, rather than storing the literal id", () => {
    setAccentPreset("teal");
    setAccentPreset(DEFAULT_ACCENT_PRESET);
    expect(localStorage.getItem(ACCENT_PRESET_STORAGE_KEY)).toBeNull();
    expect(document.documentElement.dataset.accentPreset).toBeUndefined();
  });

  it("applies the choice for this tab even when the write itself throws", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("quota exceeded");
    });
    expect(() => setAccentPreset("green")).not.toThrow();
    expect(document.documentElement.dataset.accentPreset).toBe("green");
  });
});

describe("cross-tab storage events", () => {
  it("re-applies on a storage event for its own key", () => {
    localStorage.setItem(ACCENT_PRESET_STORAGE_KEY, "blue");
    window.dispatchEvent(
      new StorageEvent("storage", { key: ACCENT_PRESET_STORAGE_KEY, newValue: "blue" }),
    );
    expect(document.documentElement.dataset.accentPreset).toBe("blue");
  });

  it("ignores a storage event for an unrelated key, including next-themes' own", () => {
    document.documentElement.dataset.accentPreset = "amber";
    localStorage.setItem("theme", "dark");
    window.dispatchEvent(new StorageEvent("storage", { key: "theme", newValue: "dark" }));
    expect(document.documentElement.dataset.accentPreset).toBe("amber");
  });
});
