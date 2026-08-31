import { describe, expect, it } from "vitest";

import {
  type HostObservation,
  identityFailure,
} from "../e2e/host-identity";

/**
 * The verdicts behind issue #1773, tested without a server.
 *
 * The bug being guarded against is a suite that **passed** against the wrong
 * server, so the property that matters is not "a good host is accepted" — it is
 * that each specific wrong server is *refused*, and refused with a message
 * someone can act on. A check that silently accepts is exactly the failure, so
 * every case below asserts on the refusal itself and on what it says.
 *
 * `identityFailure` is a pure function of one observation for this reason: the
 * only alternative place to exercise these branches is a live Playwright run
 * against a deliberately wrong server, which is minutes per case and impossible
 * in CI.
 */

const HOST_ID = "8b7abc408c8d4e3618424dcc8c052333";
const OTHER_ID = "a334012b511584c99f6769dd2052c5bc";

/** What a real host answers on `/spec`, trimmed to the fields checked here. */
function specBody(instanceId = HOST_ID): string {
  return JSON.stringify({
    name: "opencompany",
    version: "0.1.0",
    instance_id: instanceId,
    capabilities: ["rest", "graphql", "sse"],
  });
}

/** A managed run whose host is serving the data root we manage. */
function correct(): HostObservation {
  return {
    url: "http://127.0.0.1:8123/spec",
    status: 200,
    contentType: "application/json",
    body: specBody(),
    home: "/repo/target/e2e/data",
    homeInstanceId: HOST_ID,
  };
}

describe("identityFailure", () => {
  it("accepts the host this run actually manages", () => {
    expect(identityFailure(correct())).toBeUndefined();
  });

  it("accepts a JSON content type carrying a charset", () => {
    // Axum sends `application/json`; a proxy in front of one may append a
    // charset. Refusing that would be a false alarm on a real host.
    expect(
      identityFailure({ ...correct(), contentType: "application/json; charset=utf-8" }),
    ).toBeUndefined();
  });

  it("refuses a console dev server, which is what actually happened", () => {
    // The incident: Vite's SPA fallback answers 200 text/html for every path,
    // so it satisfies both Playwright's status-code-only readiness check and a
    // `/healthz` poll. This is the one case that must never pass.
    const failure = identityFailure({
      ...correct(),
      contentType: "text/html",
      body: '<!doctype html>\n<html lang="en">\n  <head>\n    <title>OpenCompany Console</title>\n',
      homeInstanceId: undefined,
    });

    expect(failure).toBeDefined();
    expect(failure).toContain("not an OpenCompany host");
    // The evidence a reader needs without a second run.
    expect(failure).toContain("http://127.0.0.1:8123/spec");
    expect(failure).toContain("200");
    expect(failure).toContain("text/html");
    expect(failure).toContain("<!doctype html>");
  });

  it("refuses a DIFFERENT OpenCompany host, not merely a non-host", () => {
    // The second incident on 2026-08-25: a real host answering on a port,
    // from another agent's process, while this run's own host had exited.
    const failure = identityFailure({ ...correct(), body: specBody(OTHER_ID) });

    expect(failure).toBeDefined();
    expect(failure).toContain("DIFFERENT one");
    expect(failure).toContain(HOST_ID);
    expect(failure).toContain(OTHER_ID);
  });

  it("refuses a host that never touched the data root this run manages", () => {
    // Answering `/spec` is what mints the `instance-id` file under the
    // responder's own root, so a root of ours still holding none *after* the
    // request was not served by whoever answered.
    const failure = identityFailure({ ...correct(), homeInstanceId: undefined });

    expect(failure).toBeDefined();
    expect(failure).toContain("NOT serving the data root");
    expect(failure).toContain("/repo/target/e2e/data");
  });

  it("checks an explicitly named instance id, and in preference to the root", () => {
    // `PW_EXPECTED_INSTANCE_ID` is the seam for a host you brought yourself,
    // where there is no root of ours to compare against. It outranks the
    // derived expectation: the caller knows something this config does not.
    expect(
      identityFailure({
        url: "http://127.0.0.1:8591/spec",
        status: 200,
        contentType: "application/json",
        body: specBody(OTHER_ID),
        expectedInstanceId: HOST_ID,
      }),
    ).toContain("NOT the one this run was told to drive");

    expect(
      identityFailure({
        ...correct(),
        // A root disagreeing is ignored once the caller has named an id.
        homeInstanceId: OTHER_ID,
        expectedInstanceId: HOST_ID,
      }),
    ).toBeUndefined();
  });

  it("refuses a non-2xx answer", () => {
    const failure = identityFailure({ ...correct(), status: 502, body: "Bad Gateway" });

    expect(failure).toContain("502");
    expect(failure).toContain("Bad Gateway");
  });

  it("refuses JSON that is not this crate's spec", () => {
    const failure = identityFailure({
      ...correct(),
      body: JSON.stringify({ name: "some-other-service", instance_id: HOST_ID }),
    });

    expect(failure).toContain('"some-other-service"');
    expect(failure).toContain("not an OpenCompany host");
  });

  it("refuses a body that claims JSON and is not", () => {
    const failure = identityFailure({ ...correct(), body: "<html>oops</html>" });

    expect(failure).toContain("does not parse as JSON");
  });

  it("refuses a host too old to carry an instance id", () => {
    const failure = identityFailure({
      ...correct(),
      body: JSON.stringify({ name: "opencompany", version: "0.1.0" }),
    });

    expect(failure).toContain("no instance_id");
  });

  it("says nothing when there is nothing to compare against", () => {
    // A host the caller brought and did not identify. Passing here is honest:
    // the type check ran and held, and claiming an identity check that could
    // not happen would be worse than saying so in the docs.
    expect(
      identityFailure({
        url: "http://127.0.0.1:8080/spec",
        status: 200,
        contentType: "application/json",
        body: specBody(),
      }),
    ).toBeUndefined();
  });

  it("quotes a long body without pasting the whole page", () => {
    const failure = identityFailure({
      ...correct(),
      contentType: "text/html",
      body: `<html>${"x".repeat(5000)}</html>`,
    });

    expect(failure).toContain("…");
    // 200 bytes of body plus the surrounding explanation, not 5 KB of markup.
    expect(failure!.length).toBeLessThan(1200);
  });

  it("collapses whitespace so the first 200 bytes carry 200 bytes of signal", () => {
    // An HTML page's first 200 bytes are mostly newlines and indentation; a
    // doctype followed by twelve blank lines identifies nothing.
    const failure = identityFailure({
      ...correct(),
      contentType: "text/html",
      body: "<!doctype html>\n\n\n        <title>Someone else</title>",
    });

    expect(failure).toContain("<!doctype html> <title>Someone else</title>");
  });
});
