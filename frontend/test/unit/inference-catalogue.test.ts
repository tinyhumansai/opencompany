import { describe, expect, it } from "vitest";

import {
  AZURE_ENDPOINT_HOSTS,
  CLI_LOGINS,
  CLOUD_PROVIDERS,
  COPY,
  LOCAL_RUNTIMES,
  cliLogin,
  cloudProvider,
  endpointHost,
  isAzureEndpoint,
  isReservedSlug,
  localRuntime,
  rowDetail,
} from "@/inference/catalogue";

/**
 * The console half of the provider catalogue.
 *
 * Five Rust tests already assert that this table and
 * `src/company/inference/catalogue.rs` carry the same rows, so there is nothing
 * to gain by restating the data here. What those cannot reach is the behaviour
 * this side adds — host parsing, Azure detection, reserved slugs, the row detail
 * line — which is all pure, so it is all testable without a host or a render.
 */
describe("the provider catalogue the console renders from", () => {
  it("ships the three categories at the sizes the plan names", () => {
    expect(CLOUD_PROVIDERS).toHaveLength(26);
    expect(LOCAL_RUNTIMES).toHaveLength(3);
    expect(CLI_LOGINS).toHaveLength(2);
  });

  it("keeps Anthropic as the one non-bearer entry", () => {
    // A renderer that assumes one auth style breaks exactly one provider, and
    // it is the one people try first.
    const nonBearer = CLOUD_PROVIDERS.filter((p) => p.auth !== "bearer").map((p) => p.slug);
    expect(nonBearer).toEqual(["anthropic"]);
  });

  it("looks a provider up by slug in each category", () => {
    expect(cloudProvider("openrouter")?.endpoint).toBe("https://openrouter.ai/api/v1");
    expect(localRuntime("ollama")?.defaultEndpoint).toBe("http://localhost:11434");
    expect(cliLogin("codex")?.storedSlug).toBe("openai");
    expect(cloudProvider("nope")).toBeUndefined();
  });

  it("stores the Codex login under OpenAI so its connected check can match", () => {
    // Keying the already-connected check on the literal `codex` never matches,
    // so the dialog would offer Codex forever however often it was connected.
    expect(cliLogin("codex")?.storedSlug).toBe("openai");
    // And it hands over no key, so `claude-code` has nothing for a probe.
    expect(cliLogin("claude-code")?.probes).toBe(false);
  });
});

describe("reading the host out of an endpoint", () => {
  it("drops the scheme, userinfo, port and path", () => {
    expect(endpointHost("https://api.openai.com/v1")).toBe("api.openai.com");
    expect(endpointHost("api.groq.com/openai/v1")).toBe("api.groq.com");
    expect(endpointHost("http://user:pw@host.example:8080/v1")).toBe("host.example");
    expect(endpointHost("http://[::1]:11434/v1")).toBe("::1");
    expect(endpointHost("HTTPS://API.OpenAI.com/v1")).toBe("api.openai.com");
  });

  it("answers empty rather than throwing on nothing", () => {
    expect(endpointHost("   ")).toBe("");
    expect(endpointHost(null)).toBe("");
    expect(endpointHost(undefined)).toBe("");
  });
});

describe("recognising an Azure endpoint", () => {
  it("matches a resource subdomain of every documented parent", () => {
    for (const host of AZURE_ENDPOINT_HOSTS) {
      expect(isAzureEndpoint(`https://my-resource.${host}/openai/v1`)).toBe(true);
    }
    expect(isAzureEndpoint("https://openai.azure.com/openai/v1")).toBe(true);
  });

  it("leaves the Foundry serverless hosts alone, on purpose", () => {
    // Classifying these would relabel a correct model id as a "deployment
    // name" — the exact confusion the Azure rule exists to prevent.
    expect(isAzureEndpoint("https://r.inference.ai.azure.com/models")).toBe(false);
    expect(isAzureEndpoint("https://r.models.ai.azure.com/models")).toBe(false);
  });

  it("does not match a lookalike host without a dot boundary", () => {
    expect(isAzureEndpoint("https://myopenai.azure.comx/v1")).toBe(false);
    expect(isAzureEndpoint("https://openai.azure.com.evil.test/v1")).toBe(false);
    expect(isAzureEndpoint("https://api.openai.com/v1")).toBe(false);
  });
});

describe("reserving the names the catalogue already owns", () => {
  it("covers cloud slugs, local slugs and the slug a CLI login stores under", () => {
    expect(isReservedSlug("openrouter")).toBe(true);
    expect(isReservedSlug("ollama")).toBe(true);
    expect(isReservedSlug("claude-code")).toBe(true);
    // Codex stores under `openai`, which a cloud row already reserves.
    expect(isReservedSlug("openai")).toBe(true);
  });

  it("leaves a name nobody ships available", () => {
    expect(isReservedSlug("acme-gateway")).toBe(false);
    expect(isReservedSlug("  acme-gateway  ")).toBe(false);
  });
});

describe("the detail line under a row's label", () => {
  it("shows a cloud row its endpoint's host, not the whole URL", () => {
    // The path is noise at a glance; the host is the part an operator
    // recognises as the account they are paying.
    expect(rowDetail("cloud", "https://api.deepinfra.com/v1/openai")).toBe("api.deepinfra.com");
  });

  it("says what a local runtime and a CLI login are instead", () => {
    expect(rowDetail("local", "http://localhost:11434")).toBe(COPY.detailLocal);
    expect(rowDetail("cli")).toBe(COPY.detailCli);
  });
});
