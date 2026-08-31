import { describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { listInferenceModels } from "@/api/inference";

describe("the inference model catalog client", () => {
  it("gets the addressed company's model-list route", async () => {
    const calls: string[] = [];
    const client = {
      scopeFor: (company: string | null) =>
        company ? `/api/v1/companies/${company}` : "/api/v1/company",
      get: async (path: string) => {
        calls.push(path);
        return [{ id: "provider/model", name: "Model" }];
      },
    } as unknown as OpenCompanyClient;

    await expect(listInferenceModels(client, "acme")).resolves.toEqual([
      { id: "provider/model", name: "Model" },
    ]);
    expect(calls).toEqual(["/api/v1/companies/acme/inference/models"]);
  });
});
