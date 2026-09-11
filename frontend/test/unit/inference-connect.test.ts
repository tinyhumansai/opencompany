// The connect flow's decisions, exercised as functions rather than through a
// rendered dialog.
//
// Everything here is a branch the add flow makes before any request is sent:
// what each category offers, what it asks for, and whether a typed name may be
// used. If one of these is ever reachable only by opening a modal and clicking,
// it has been put in the wrong place.

import { describe, expect, it } from "vitest";

import {
  MAX_PROVIDER_NAME_CHARS,
  addOptions,
  checkProviderName,
  checkSlug,
  endpointHasCredentials,
  credentialAsk,
  customProviderReady,
  isConnected,
  normalizeEndpoint,
  providerMenu,
  slugErrorCopy,
  slugify,
} from "@/inference/connect";
import { CLOUD_PROVIDERS } from "@/inference/catalogue";
import { stripEnvelopePrefix } from "@/inference/ProvidersTab";
import type { Provider } from "@/inference/types";

function provider(slug: string, kind = slug): Provider {
  return {
    id: `prv_${slug}`,
    slug,
    label: slug,
    kind,
    baseUrl: `https://${slug}.example/v1`,
    models: {},
    enabled: true,
    keyConfigured: true,
  };
}

describe("what the add dialog offers", () => {
  it("lists only what is not yet connected", () => {
    // Offering to add something twice is how you get two rows for one provider.
    const options = addOptions([provider("groq")]);
    expect(options.cloud.some((o) => o.value === "groq")).toBe(false);
    expect(options.cloud.some((o) => o.value === "openai")).toBe(true);
    expect(options.cloud).toHaveLength(CLOUD_PROVIDERS.length - 1);
  });

  it("hides Codex once OpenAI is connected, because its login IS an OpenAI key", () => {
    // The trap: keying the already-connected check on the literal `codex` never
    // matches, so the dialog would offer it forever however many times it was
    // connected.
    const options = addOptions([provider("openai")]);
    expect(options.cli.some((o) => o.value === "codex")).toBe(false);
    expect(options.cli.some((o) => o.value === "claude-code")).toBe(true);
    expect(isConnected([provider("openai")], "codex")).toBe(true);
  });

  it("gives each category the detail line its question implies", () => {
    const options = addOptions([]);
    expect(options.cloud.find((o) => o.value === "openai")?.detail).toBe("api.openai.com");
    expect(options.cloud.find((o) => o.value === "anthropic")?.detail).toBe("api.anthropic.com");
    expect(options.local[0]?.detail).toBe("Runs on this machine");
    expect(options.cli[0]?.detail).toBe("Uses a login another CLI already holds");
  });
});

describe("what each category asks for", () => {
  it("asks a cloud provider for a key and never for an endpoint", () => {
    // The endpoint is a preset. The paths in that table — /openai/v1,
    // /v1beta/openai, /api/paas/v4 — are not something to retype.
    const ask = credentialAsk("openai");
    expect(ask).toMatchObject({ needsKey: true, needsEndpoint: false, keyPlaceholder: "sk-..." });
  });

  it("asks a local runtime for an endpoint and not a key", () => {
    const ask = credentialAsk("ollama");
    expect(ask).toMatchObject({ needsKey: false, needsEndpoint: true });
    expect(ask.defaultEndpoint).toBe("http://localhost:11434");
  });

  it("asks omlx for both, because it is the one local runtime that wants both", () => {
    expect(credentialAsk("omlx")).toMatchObject({ needsKey: true, needsEndpoint: true });
  });

  it("asks a CLI login for nothing", () => {
    // Another tool already holds the credential.
    expect(credentialAsk("claude-code")).toMatchObject({
      needsKey: false,
      needsEndpoint: false,
    });
  });

  it("asks a custom provider for both", () => {
    expect(credentialAsk("custom")).toMatchObject({ needsKey: true, needsEndpoint: true });
  });
});

describe("the slug, which is derived and never typed", () => {
  it("falls out of the name", () => {
    expect(slugify("Acme Gateway")).toBe("acme-gateway");
    expect(slugify("  My Provider!! ")).toBe("my-provider");
    expect(slugify("Z.AI 2")).toBe("z-ai-2");
  });

  it("names the three ways it can fail, because they need three sentences", () => {
    expect(checkSlug([], "")).toBe("empty");
    expect(checkSlug([provider("acme")], "acme")).toBe("taken");
    // A typed name may not shadow something we ship: a routing entry saying
    // `groq` would then mean two things.
    expect(checkSlug([], "groq")).toBe("reserved");
    expect(checkSlug([], "acme")).toBeNull();
  });

  it("says what to do about each", () => {
    expect(slugErrorCopy("empty")).toBe("Enter a provider name to generate a slug.");
    expect(slugErrorCopy("taken")).toContain("already has a provider");
    expect(slugErrorCopy("reserved")).toContain("built-in");
    expect(slugErrorCopy("too-long")).toContain(String(MAX_PROVIDER_NAME_CHARS));
  });

  it("bounds the name, because the name becomes the address of a secret", () => {
    // Mirrors `store::MAX_PROVIDER_NAME_CHARS`. The host holds the rule; this
    // only spares the operator a round trip. An unbounded name produced an
    // unbounded secret key, which 500ed a credential read and truncated a
    // stored key on the way to failing the delete that truncated it.
    const atLimit = "a".repeat(MAX_PROVIDER_NAME_CHARS);
    const pastLimit = "a".repeat(MAX_PROVIDER_NAME_CHARS + 1);

    expect(checkProviderName(atLimit)).toBeNull();
    expect(checkProviderName(pastLimit)).toBe("too-long");
    expect(checkProviderName("   ")).toBe("empty");

    expect(checkSlug([], atLimit)).toBeNull();
    expect(checkSlug([], pastLimit)).toBe("too-long");

    expect(customProviderReady([], { label: atLimit, baseUrl: "https://a.example/v1" })).toBe(
      true,
    );
    expect(customProviderReady([], { label: pastLimit, baseUrl: "https://a.example/v1" })).toBe(
      false,
    );
  });
});

describe("the endpoint an operator types", () => {
  it("gains the /v1 an OpenAI surface lives at when a bare origin is given", () => {
    expect(normalizeEndpoint("http://localhost:11434")).toBe("http://localhost:11434/v1");
    expect(normalizeEndpoint("  http://localhost:11434/ ")).toBe("http://localhost:11434/v1");
  });

  it("leaves a path exactly as typed, because appending is not guessing", () => {
    expect(normalizeEndpoint("https://acme.example/api/gateway")).toBe(
      "https://acme.example/api/gateway",
    );
  });

  it("refuses anything that is not http or https", () => {
    for (const bad of ["file:///etc/passwd", "ftp://acme.example/v1", "localhost:11434", "", "http://"]) {
      expect(normalizeEndpoint(bad)).toBeNull();
    }
  });

  it("refuses one that carries a credential, because an endpoint is stored as written", () => {
    // Mirrors `catalogue::endpoint_has_credentials`. A `baseUrl` is returned
    // by a `ScopedCompany` route every console reader calls, so a password in
    // one is a password on the wire for everybody.
    for (const bad of [
      "http://alice:hunter2@127.0.0.1:8597/v1",
      "https://alice@api.acme.example/v1",
      "http://alice:hun@ter2@127.0.0.1:8597/v1",
    ]) {
      expect(endpointHasCredentials(bad)).toBe(true);
      expect(normalizeEndpoint(bad)).toBeNull();
    }
  });

  it("does not mistake an @ in the path for a credential", () => {
    for (const good of [
      "https://api.acme.example/v1/@me",
      "https://api.acme.example/v1?to=a@b",
      "https://api.acme.example/v1#a@b",
    ]) {
      expect(endpointHasCredentials(good)).toBe(false);
      expect(normalizeEndpoint(good)).toBe(good);
    }
  });
});

describe("the custom-provider dialog's Add button", () => {
  it("stays disabled until both the name and the URL are usable", () => {
    expect(customProviderReady([], { label: "", baseUrl: "https://acme.example/v1" })).toBe(false);
    expect(customProviderReady([], { label: "Acme", baseUrl: "" })).toBe(false);
    expect(customProviderReady([], { label: "Groq", baseUrl: "https://acme.example/v1" })).toBe(
      false,
    );
    expect(customProviderReady([], { label: "Acme", baseUrl: "https://acme.example/v1" })).toBe(
      true,
    );
  });
});

describe("the message a refusal shows the operator", () => {
  it("drops the envelope's machine prefix", () => {
    // `invalid request:` says which *kind* of error this is to a caller that
    // might branch on it, and nothing at all to the person standing in front of
    // the field they have to correct. The sentence after it is already written
    // for them.
    expect(
      stripEnvelopePrefix("invalid request: Could not reach Groq: the provider rejected the credential."),
    ).toBe("Could not reach Groq: the provider rejected the credential.");
  });

  it("leaves a message that has no prefix alone", () => {
    expect(stripEnvelopePrefix("That did not work.")).toBe("That did not work.");
  });

  it("does not eat a colon that is part of the sentence", () => {
    // "Could not reach Groq: …" has its own colon, and only a known envelope
    // keyword at the very start may be removed.
    expect(stripEnvelopePrefix("Could not reach Groq: the provider rejected it.")).toBe(
      "Could not reach Groq: the provider rejected it.",
    );
  });
});

describe("providerMenu", () => {
  const row = (over: Partial<Parameters<typeof providerMenu>[0]> = {}) => ({
    kind: "openrouter",
    enabled: true,
    keyConfigured: true,
    ...over,
  });
  const ids = (over = {}) => providerMenu(row(over)).map((a) => a.id);

  it("offers key actions on a provider that has a key to act on", () => {
    expect(ids()).toEqual(["edit", "default", "replaceKey", "removeKey", "remove"]);
  });

  it("never offers key actions to a local runtime or a CLI login", () => {
    // Derived from `credentialAsk`, not from a list of kinds here — Ollama is
    // asked for an endpoint and Claude Code holds its credential in another
    // tool, so neither has a key this page could replace or remove.
    expect(ids({ kind: "ollama", keyConfigured: false })).not.toContain("replaceKey");
    expect(ids({ kind: "ollama", keyConfigured: false })).not.toContain("removeKey");
    expect(ids({ kind: "claude-code", keyConfigured: false })).not.toContain("replaceKey");
    expect(ids({ kind: "claude-code", keyConfigured: false })).not.toContain("removeKey");
  });

  it("asks a local runtime to edit its endpoint rather than its key", () => {
    expect(providerMenu(row({ kind: "ollama" }))[0].label).toBe("Edit endpoint");
    expect(providerMenu(row())[0].label).toBe("Edit");
  });

  it("does not offer to remove a key that is not there", () => {
    // The store has no delete, so removing a key is a write of the empty
    // string — offering it against nothing is a destructive-looking no-op.
    expect(ids({ keyConfigured: false })).toContain("replaceKey");
    expect(ids({ keyConfigured: false })).not.toContain("removeKey");
    expect(providerMenu(row({ keyConfigured: false }))[2].label).toBe("Add a key");
  });

  it("omits Set as default where it would change nothing", () => {
    expect(ids({ isDefault: true })).not.toContain("default");
    // Switched off, so it cannot be a routing target at all.
    expect(ids({ enabled: false })).not.toContain("default");
  });

  it("marks exactly the two removals as destructive", () => {
    const destructive = providerMenu(row())
      .filter((a) => a.destructive)
      .map((a) => a.id);
    expect(destructive).toEqual(["removeKey", "remove"]);
  });

  it("names the two removals differently, because they are different", () => {
    const labels = Object.fromEntries(providerMenu(row()).map((a) => [a.id, a.label]));
    expect(labels.removeKey).toBe("Remove key");
    expect(labels.remove).toBe("Remove provider");
  });
});
