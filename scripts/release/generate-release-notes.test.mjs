// `node --test scripts/release/` — no test framework, no dependency, because
// this directory has no package.json and adding one to run four assertions
// would put a second npm project in a repository that has exactly one
// (`frontend/`). Run in CI by the `Console` job, alongside the other
// repository-wide policy checks.
//
// Pure-function unit tests do not shell out. The integration test near the
// bottom of this file creates a temporary git repository and calls the real
// `collectCommits` / `makeBranchShasFetcher` path to exercise the full
// attribution pipeline without a network call.
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  attributeMergeCommits,
  buildOpenAiRequest,
  serializeOpenAiPayload,
  buildReleasePayload,
  collectCommits,
  collectContributorStats,
  ensureAllPullRequestsLinked,
  extractPullRequestNumbers,
  extractResponseText,
  formatHandleList,
  groupPullRequestsByHighlight,
  makeBranchShasFetcher,
  parseArgs,
  parseGitHubRepoFromRemote,
  parseGitLog,
  releaseTitle,
  renderDeterministicNotes,
  trimBody,
} from './generate-release-notes.mjs';

test('parseArgs defaults and overrides', () => {
  const defaults = parseArgs([]);
  assert.equal(defaults.from, 'latest-release');
  assert.equal(defaults.to, 'latest-tag');
  assert.equal(defaults.noAi, false);

  const custom = parseArgs(['--from', 'v0.1.0', '--to', 'main', '--no-ai', '-o', 'notes.md']);
  assert.equal(custom.from, 'v0.1.0');
  assert.equal(custom.to, 'main');
  assert.equal(custom.noAi, true);
  assert.equal(custom.output, 'notes.md');

  assert.throws(() => parseArgs(['--nope']), /Unknown option/);
  // A flag swallowing the next flag as its value is the silent failure here:
  // `--from --no-ai` would otherwise set from='--no-ai' and produce an empty range.
  assert.throws(() => parseArgs(['--from', '--no-ai']), /requires a value/);
});

test('parseGitHubRepoFromRemote handles ssh and https', () => {
  assert.equal(parseGitHubRepoFromRemote('git@github.com:tinyhumansai/opencompany.git'), 'tinyhumansai/opencompany');
  assert.equal(parseGitHubRepoFromRemote('https://github.com/tinyhumansai/opencompany'), 'tinyhumansai/opencompany');
  assert.equal(parseGitHubRepoFromRemote('https://gitlab.com/a/b.git'), null);
  assert.equal(parseGitHubRepoFromRemote(''), null);
});

test('extractPullRequestNumbers takes the merge PR last', () => {
  assert.deepEqual(extractPullRequestNumbers('fix: thing (#12)'), [12]);
  assert.deepEqual(extractPullRequestNumbers('fix: closes (#12) (#34)'), [12, 34]);
  assert.deepEqual(extractPullRequestNumbers('chore: no pr'), []);
});

test('extractPullRequestNumbers reads regular merge-commit subjects', () => {
  // This repository merges most PRs with a merge commit, not a squash: 25 of
  // the last 200 subjects are this shape and carry no parenthesized number at
  // all. Matching only `(#N)` dropped every one of them — unfetched, unlinked,
  // and uncredited.
  assert.deepEqual(
    extractPullRequestNumbers('Merge pull request #1731 from theamazinghenk/feat/company-logo-backend'),
    [1731],
  );
  // Both forms present: the merge is the PR, so it must be last and therefore
  // primary. A `(#12)` inside is the issue the branch referenced.
  assert.deepEqual(extractPullRequestNumbers('Merge pull request #34 from a/fix-(#12)'), [12, 34]);
  assert.deepEqual(parseGitLog(
    ['m1', 'Merge pull request #1731 from x/y', 'Ada', 'a@e.com', '2026-08-01T00:00:00Z'].join('\x1f'),
  )[0].primaryPrNumber, 1731);
});

test('parseGitLog splits on ASCII separators', () => {
  const log = [
    ['aaaaaaaaaaaa1', 'feat: ledgers (#7)', 'Ada', 'ada@example.com', '2026-08-01T00:00:00Z'].join('\x1f'),
    ['bbbbbbbbbbbb2', 'chore: tidy', 'Bo', 'bo@example.com', '2026-08-02T00:00:00Z'].join('\x1f'),
  ].join('\x1e');

  const commits = parseGitLog(log);
  assert.equal(commits.length, 2);
  assert.equal(commits[0].primaryPrNumber, 7);
  assert.equal(commits[1].primaryPrNumber, null);
  assert.equal(parseGitLog('   ').length, 0);
});

test('collectContributorStats flags first-time contributors', () => {
  const commits = parseGitLog(
    [
      ['a1', 'feat: a (#1)', 'Ada', 'ada@example.com', '2026-08-01T00:00:00Z'].join('\x1f'),
      ['b2', 'feat: b (#2)', 'Bo', 'bo@example.com', '2026-08-02T00:00:00Z'].join('\x1f'),
      ['c3', 'feat: c (#3)', 'Ada', 'ada@example.com', '2026-08-03T00:00:00Z'].join('\x1f'),
    ].join('\x1e'),
  );
  // Case-insensitive on purpose: git records whatever casing the author's
  // config carries, and "ADA" is not a new contributor.
  const prior = new Set(['ADA'.toLowerCase()]);

  const stats = collectContributorStats(commits, prior);
  assert.deepEqual(
    stats.map((s) => [s.name, s.commits, s.prs, s.isNew]),
    [
      ['Ada', 2, [1, 3], false],
      ['Bo', 1, [2], true],
    ],
  );
});

test('collectContributorStats merges one person committing under two emails', () => {
  // The real shape this guards: a contributor whose PR merges land under their
  // personal address and whose web edits land under GitHub's noreply one. Keyed
  // on name+email they appeared twice — once with the PRs, once as a bare line
  // that also claimed to be a first-time contributor.
  const commits = parseGitLog(
    [
      ['a1', 'feat: a (#1)', 'Ada', 'ada@example.com', '2026-08-01T00:00:00Z'].join('\x1f'),
      ['a2', 'docs: b (#2)', 'Ada', '1234+ada@users.noreply.github.com', '2026-08-02T00:00:00Z'].join('\x1f'),
    ].join('\x1e'),
  );

  const stats = collectContributorStats(commits, new Set(['ada']));
  assert.equal(stats.length, 1);
  assert.equal(stats[0].commits, 2);
  assert.deepEqual(stats[0].prs, [1, 2]);
  assert.equal(stats[0].isNew, false);
});

test('groupPullRequestsByHighlight buckets by keyword and never drops a PR', () => {
  const prs = [
    { number: 1, title: 'feat: company logo backend', url: 'u1', labels: [] },
    { number: 2, title: 'fix: ledger fold ordering', url: 'u2', labels: [] },
    { number: 3, title: 'fix: run status legend', url: 'u3', labels: [] },
    { number: 4, title: 'chore: notarize the dmg', url: 'u4', labels: [] },
    { number: 5, title: 'something entirely unclassifiable', url: 'u5', labels: [] },
  ];

  const groups = groupPullRequestsByHighlight(prs);
  const placed = groups.flatMap((group) => group.pullRequests.map((pr) => pr.number));
  assert.deepEqual(placed.sort((a, b) => a - b), [1, 2, 3, 4, 5]);
  assert.ok(groups.every((group) => group.pullRequests.length > 0));
  // The unmatched PR falls into the last bucket rather than vanishing.
  assert.ok(groups.at(-1).pullRequests.some((pr) => pr.number === 5));
});

test('renderDeterministicNotes emits every PR link and omits an empty new-contributor section', () => {
  const commits = parseGitLog(
    [
      ['a1', 'feat: ledger fold (#1)', 'Ada', 'ada@example.com', '2026-08-01T00:00:00Z'].join('\x1f'),
      ['b2', 'fix: console nav (#2)', 'Bo', 'bo@example.com', '2026-08-02T00:00:00Z'].join('\x1f'),
    ].join('\x1e'),
  );
  const contributors = collectContributorStats(commits, new Set(['ada', 'bo']));
  const pullRequests = [
    { number: 1, title: 'feat: ledger fold', url: 'https://x/1', author: 'ada', labels: [], commits: [] },
    { number: 2, title: 'fix: console nav', url: 'https://x/2', author: 'bo', labels: [], commits: [] },
  ];
  const payload = buildReleasePayload({
    from: 'v0.1.0',
    to: 'v0.2.0',
    resolvedTo: 'v0.2.0',
    repo: 'tinyhumansai/opencompany',
    commits,
    pullRequests,
    contributors,
  });

  assert.equal(payload.totals.commits, 2);
  assert.equal(payload.totals.newContributors, 0);
  assert.equal(payload.range.compareUrl, 'https://github.com/tinyhumansai/opencompany/compare/v0.1.0...v0.2.0');

  const markdown = renderDeterministicNotes({ title: releaseTitle('v0.1.0', 'v0.2.0', 'v0.2.0'), payload });
  assert.match(markdown, /\[#1\]\(https:\/\/x\/1\)/);
  assert.match(markdown, /\[#2\]\(https:\/\/x\/2\)/);
  assert.match(markdown, /## Contributor Credits/);
  assert.doesNotMatch(markdown, /## New Contributors/);
  assert.match(markdown, /## Full Compare/);
});

test('renderDeterministicNotes celebrates first-time contributors when there are any', () => {
  const commits = parseGitLog(
    [['b2', 'fix: console nav (#2)', 'Bo', 'bo@example.com', '2026-08-02T00:00:00Z'].join('\x1f')].join('\x1e'),
  );
  const payload = buildReleasePayload({
    from: 'v0.1.0',
    to: 'v0.2.0',
    resolvedTo: 'v0.2.0',
    repo: 'tinyhumansai/opencompany',
    commits,
    pullRequests: [{ number: 2, title: 'fix: console nav', url: 'https://x/2', author: 'bo', labels: [], commits: [] }],
    contributors: collectContributorStats(commits, new Set()),
  });

  const markdown = renderDeterministicNotes({ title: 'v0.1.0 to v0.2.0', payload });
  assert.match(markdown, /## New Contributors/);
  assert.match(markdown, /Welcome Bo!/);
});

test('ensureAllPullRequestsLinked appends only what the model dropped', () => {
  const prs = [
    { number: 1, title: 'a', url: 'https://x/1', author: 'ada' },
    { number: 2, title: 'b', url: 'https://x/2', author: 'bo' },
  ];
  const complete = '# Notes\n\n[#1](https://x/1) and [#2](https://x/2)\n\n## Full Compare\n\nurl\n';
  assert.equal(ensureAllPullRequestsLinked(complete, prs), complete);

  const partial = '# Notes\n\n[#1](https://x/1)\n\n## Full Compare\n\nurl\n';
  const repaired = ensureAllPullRequestsLinked(partial, prs);
  assert.match(repaired, /### Additional highlights/);
  assert.match(repaired, /\[#2\]\(https:\/\/x\/2\)/);
  // Repaired in place, before the trailing sections — not stapled past them.
  assert.ok(repaired.indexOf('Additional highlights') < repaired.indexOf('## Full Compare'));
});

test('formatHandleList reads as English at every length', () => {
  assert.equal(formatHandleList(['a']), '@a');
  assert.equal(formatHandleList(['a', 'b']), '@a and @b');
  assert.equal(formatHandleList(['a', 'b', 'c']), '@a, @b, and @c');
});

test('trimBody strips PR-template comments and caps length', () => {
  assert.equal(trimBody('<!-- template -->\nreal text'), 'real text');
  assert.equal(trimBody(null), '');
  assert.ok(trimBody('x'.repeat(5000)).length <= 700);
});

test('extractResponseText reads both Responses API shapes', () => {
  assert.equal(extractResponseText({ output_text: 'hi' }), 'hi');
  assert.equal(
    extractResponseText({ output: [{ content: [{ text: 'a' }, { text: 'b' }] }] }),
    'a\nb',
  );
  assert.equal(extractResponseText({}), '');
});

test('buildOpenAiRequest tells the model that contributor names are not handles', () => {
  // The first real release cut with this generator credited "@Cyrus Gray" and
  // "@Jarno de Vries" — the model read `contributors[].name` as a GitHub login
  // and @-prefixed a display name, producing dead mentions. Only
  // `pullRequests[].author` is a handle.
  const request = buildOpenAiRequest({
    model: 'gpt-5.2',
    title: 'v0.1.0 to v0.2.0',
    payload: { contributors: [], pullRequests: [], uncategorizedCommits: [], totals: {}, range: {} },
  });

  assert.equal(request.model, 'gpt-5.2');
  assert.match(request.input[1].content, /DISPLAY NAME, not a GitHub handle/);
  assert.match(request.input[1].content, /Never prefix it with "@"/);
});

test('groupPullRequestsByHighlight anchors keywords to word starts', () => {
  // `includes` filed anything saying "support" or "metadata" under Ledgers,
  // via the `port` and `data` keywords. Note the last group doubles as the
  // catch-all, so "not misfiled" is asserted against the SPECIFIC group the
  // substring match wrongly chose, not against the fallback.
  const titleFor = (groups, n) =>
    groups.find((g) => g.pullRequests.some((pr) => pr.number === n)).title;

  const misfiled = groupPullRequestsByHighlight([
    { number: 1, title: 'feat: add support for wide screens', url: 'u1', labels: [] },
    { number: 2, title: 'fix: metadata handling', url: 'u2', labels: [] },
  ]);
  assert.doesNotMatch(titleFor(misfiled, 1), /Ledgers/);
  assert.doesNotMatch(titleFor(misfiled, 2), /Ledgers/);

  // Prefix and plural keywords must still match their real group.
  const stillMatched = groupPullRequestsByHighlight([
    { number: 3, title: 'chore: notarize the dmg', url: 'u3', labels: [] },
    { number: 4, title: 'feat: new skills surface', url: 'u4', labels: [] },
    { number: 5, title: 'refactor: persistence ports', url: 'u5', labels: [] },
  ]);
  assert.match(titleFor(stillMatched, 3), /Desktop/);
  assert.match(titleFor(stillMatched, 4), /Companies/);
  assert.match(titleFor(stillMatched, 5), /Ledgers/);
});

// ---------------------------------------------------------------------------
// attributeMergeCommits — issue #1901
//
// These tests use a synthetic commit graph so they run without shelling out
// to git. The `fetchBranchShas` argument is a pure function over the fake
// SHA map, making every assertion fully deterministic.
// ---------------------------------------------------------------------------

// Helper: build a commit record in the shape parseGitLog emits.
function makeCommit(sha, subject, authorName, authorEmail, { parents = [] } = {}) {
  const prNumbers = extractPullRequestNumbers(subject);
  return {
    sha,
    shortSha: sha.slice(0, 9),
    subject,
    authorName,
    authorEmail,
    authoredAt: '2026-08-01T00:00:00Z',
    prNumbers,
    primaryPrNumber: prNumbers.at(-1) || null,
    parents,
    isMerge: parents.length >= 2,
  };
}

test('attributeMergeCommits: regular merge PR credited to branch author, not maintainer', () => {
  // Mirrors the concrete bug: PR #1731 was a regular merge authored by Steven
  // (the maintainer). Its feature commit was authored by Jarno. Before this fix
  // the notes credited Steven with #1731 and welcomed Jarno with nothing.
  const mergeCommit = makeCommit(
    'merge000',
    'Merge pull request #1731 from theamazinghenk/feat/company-logo-backend',
    'Steven Enamakel',
    'steven@example.com',
    { parents: ['parent0', 'branch0'] },
  );
  const branchCommit = makeCommit(
    'branch0',
    'feat(company): uploadable company logo via API',
    'Jarno de Vries',
    'jarno@example.com',
    { parents: ['ancestor0'] },
  );

  const result = attributeMergeCommits(
    [branchCommit, mergeCommit],
    (sha) => (sha === 'merge000' ? ['branch0'] : []),
  );

  const stats = collectContributorStats(result, new Set());
  const jarno = stats.find((s) => s.name === 'Jarno de Vries');
  const steven = stats.find((s) => s.name === 'Steven Enamakel');

  assert.ok(jarno, 'Jarno must appear as a contributor');
  assert.deepEqual(jarno.prs, [1731], 'PR #1731 must be credited to Jarno');
  assert.ok(steven, 'Steven must still appear (he made the merge commit)');
  assert.deepEqual(steven.prs, [], 'Steven must NOT carry PR #1731');
});

test('attributeMergeCommits: squash-merged PR attribution is unchanged', () => {
  // Squash merges have a single parent: isMerge === false, author IS the
  // contributor. attributeMergeCommits must not touch them.
  const squash = makeCommit(
    'squash0',
    'feat: new feature (#42)',
    'Contributor',
    'c@example.com',
    { parents: ['p0'] },
  );
  const result = attributeMergeCommits([squash], () => {
    throw new Error('should not be called for squash commits');
  });

  assert.equal(result.length, 1);
  assert.equal(result[0].primaryPrNumber, 42, 'squash PR number must be preserved');
  assert.equal(result[0].authorName, 'Contributor');
});

test('attributeMergeCommits: multiple branch authors both receive the PR', () => {
  const mergeCommit = makeCommit(
    'merge1',
    'Merge pull request #99 from org/feature',
    'Maintainer',
    'm@example.com',
    { parents: ['p1', 'b1'] },
  );
  const branchCommit1 = makeCommit('b1', 'feat: first half', 'Alice', 'alice@example.com', { parents: ['p1'] });
  const branchCommit2 = makeCommit('b2', 'feat: second half', 'Bob', 'bob@example.com', { parents: ['b1'] });

  const result = attributeMergeCommits(
    [branchCommit2, branchCommit1, mergeCommit],
    (sha) => (sha === 'merge1' ? ['b1', 'b2'] : []),
  );

  const stats = collectContributorStats(result, new Set());
  const alice = stats.find((s) => s.name === 'Alice');
  const bob = stats.find((s) => s.name === 'Bob');
  const maintainer = stats.find((s) => s.name === 'Maintainer');

  assert.deepEqual(alice?.prs, [99]);
  assert.deepEqual(bob?.prs, [99]);
  assert.deepEqual(maintainer?.prs, []);
});

test('attributeMergeCommits: graceful fallback when fetchBranchShas throws', () => {
  // If git topology is unusual (e.g. shallow clone, detached HEAD), the shell
  // call may throw. The PR must remain on the merge author rather than being lost.
  const mergeCommit = makeCommit(
    'merge2',
    'Merge pull request #55 from org/fix',
    'Maintainer',
    'm@example.com',
    { parents: ['p2', 'b3'] },
  );

  const result = attributeMergeCommits([mergeCommit], () => {
    throw new Error('git topology error');
  });

  assert.equal(result.length, 1);
  assert.equal(result[0].primaryPrNumber, 55, 'PR must fall back to merge author on error');
  assert.equal(result[0].authorName, 'Maintainer');
});

test('attributeMergeCommits: graceful fallback when no branch SHAs in commit list', () => {
  // Branch commits may predate the range (e.g. only the merge landed in this
  // release window). If none of the returned SHAs appear in `commits`, the
  // merge commit is left untouched so the PR is not silently dropped.
  const mergeCommit = makeCommit(
    'merge3',
    'Merge pull request #77 from org/old',
    'Maintainer',
    'm@example.com',
    { parents: ['p3', 'b_old'] },
  );

  const result = attributeMergeCommits(
    [mergeCommit],
    () => ['b_old_that_is_not_in_list'],
  );

  assert.equal(result[0].primaryPrNumber, 77, 'PR must remain on merge author when branch SHA not in range');
});

test('attributeMergeCommits: commits without PR numbers are not affected', () => {
  const plain = makeCommit('c1', 'chore: tidy up', 'Dev', 'd@example.com', { parents: ['p'] });
  const result = attributeMergeCommits([plain], () => []);
  assert.equal(result[0], plain, 'plain commit must be returned as-is (same reference)');
});

test('parseGitLog preserves parents and isMerge fields', () => {
  // Six-field format used by collectCommits after the #1901 fix.
  const mergeEntry = [
    '6b05c31191541fada',
    'Merge pull request #1731 from theamazinghenk/feat/company-logo-backend',
    'Steven Enamakel',
    'steven@example.com',
    '2026-08-01T00:00:00Z',
    '554f357fb8a57cf3 de50018946eb2e13', // two parent SHAs
  ].join('\x1f');
  const featureEntry = [
    'de50018946eb2e13',
    'feat(company): uploadable company logo via API',
    'Jarno de Vries',
    'jarno@example.com',
    '2026-07-30T00:00:00Z',
    'e440c0b6b16bf70f', // single parent
  ].join('\x1f');

  const commits = parseGitLog([mergeEntry, featureEntry].join('\x1e'));
  assert.equal(commits.length, 2);

  const merge = commits[0];
  assert.equal(merge.isMerge, true);
  assert.equal(merge.parents.length, 2);
  assert.equal(merge.primaryPrNumber, 1731);

  const feature = commits[1];
  assert.equal(feature.isMerge, false);
  assert.equal(feature.parents.length, 1);
  assert.equal(feature.primaryPrNumber, null);
});

test('attributeMergeCommits end-to-end: mirrors real PR #1731 topology', () => {
  // Synthetic data shaped after the actual history:
  //   merge  6b05c  authored by Steven  carries PR #1731
  //   branch de500  authored by Jarno   carries no PR number
  // Expected after attribution: Jarno has prs=[1731], Steven has prs=[].
  const merge = makeCommit(
    '6b05c31191541fada2daf0910980e98c19732b27',
    'Merge pull request #1731 from theamazinghenk/feat/company-logo-backend',
    'Steven Enamakel',
    '31011319+senamakel@users.noreply.github.com',
    { parents: ['554f357fb8a57cf3628d59e0dc9d1f34b42f5cf9', 'de50018946eb2e135112316abc2fb96f058e2763'] },
  );
  const branch = makeCommit(
    'de50018946eb2e135112316abc2fb96f058e2763',
    'feat(company): uploadable company logo via API',
    'Jarno de Vries',
    'jarno@match-day.nl',
    { parents: ['e440c0b6b16bf70f5b912028d9bcb723c2624bee'] },
  );

  const attributed = attributeMergeCommits(
    [branch, merge],
    (sha) => (sha === merge.sha ? [branch.sha] : []),
  );
  const stats = collectContributorStats(attributed, new Set());
  const jarno = stats.find((s) => s.name === 'Jarno de Vries');
  const steven = stats.find((s) => s.name === 'Steven Enamakel');

  assert.deepEqual(jarno?.prs, [1731], 'PR #1731 must belong to Jarno');
  assert.deepEqual(steven?.prs, [], 'Steven must not hold PR #1731');
  // First-time contributor detection remains in git-identity space.
  assert.equal(jarno?.isNew, true, 'Jarno is a new contributor when prior set is empty');
});

test('attributeMergeCommits: cleared merge keeps prNumbers so it stays categorized', () => {
  // primaryPrNumber alone drives contributor/PR grouping. prNumbers is only
  // used to decide uncategorizedCommits — stripping the PR there would push
  // "Merge pull request #N…" into the noise bucket.
  const mergeCommit = makeCommit(
    'merge001',
    'Merge pull request #1800 from contributor/feat/thing',
    'Maintainer',
    'maintainer@example.com',
    { parents: ['base0', 'branch1'] },
  );
  const branchCommit = makeCommit(
    'branch1',
    'feat: the actual work',
    'Contributor',
    'contributor@example.com',
    { parents: ['base0'] },
  );

  const result = attributeMergeCommits(
    [branchCommit, mergeCommit],
    (sha) => (sha === 'merge001' ? ['branch1'] : []),
  );

  const mergeAfter = result.find((c) => c.sha === 'merge001');
  assert.equal(mergeAfter.primaryPrNumber, null, 'primaryPrNumber must be null');
  assert.deepEqual(mergeAfter.prNumbers, [1800], 'prNumbers must retain the PR for categorization');
});

test('attributeMergeCommits: same-PR branch commit clears merge without double-credit', () => {
  // Branch subjects sometimes already carry `(#N)` for the same PR the merge
  // closes. Excluding those branch commits as targets left the merge uncleared,
  // so both the maintainer and the branch author received PR N in
  // collectContributorStats — double attribution.
  // A different primaryPrNumber is a stacked or nested PR and must be left alone.
  const mergeCommit = makeCommit(
    'mergeSame',
    'Merge pull request #88 from org/feature',
    'Maintainer',
    'm@example.com',
    { parents: ['pSame', 'bSame'] },
  );
  const branchCommit = makeCommit(
    'bSame',
    'feat: already tagged (#88)',
    'Contributor',
    'c@example.com',
    { parents: ['pSame'] },
  );

  const result = attributeMergeCommits(
    [branchCommit, mergeCommit],
    (sha) => (sha === 'mergeSame' ? ['bSame'] : []),
  );

  const merge = result.find((c) => c.sha === 'mergeSame');
  const branch = result.find((c) => c.sha === 'bSame');
  assert.equal(merge.primaryPrNumber, null, 'merge primaryPrNumber must be cleared');
  assert.deepEqual(merge.prNumbers, [88], 'merge prNumbers stay for categorization');
  assert.equal(branch.primaryPrNumber, 88, 'branch keeps its existing primaryPrNumber');
  assert.deepEqual(branch.prNumbers, [88], 'branch metadata is preserved, not duplicated');

  const stats = collectContributorStats(result, new Set());
  const contributor = stats.find((s) => s.name === 'Contributor');
  const maintainer = stats.find((s) => s.name === 'Maintainer');
  assert.deepEqual(contributor?.prs, [88]);
  assert.deepEqual(maintainer?.prs, [], 'maintainer must not double-credit PR #88');
});

test('attributeMergeCommits: octopus merge credits both branch parents', () => {
  // A fetcher limited to sha^1..sha^2 misses commits reachable only through
  // a third or later parent: those authors would lose attribution when the merge
  // commit is cleared, because the merge is still cleared (one branch parent was
  // found) but only the sha^2 contributors receive the PR number.
  const octopusMerge = makeCommit(
    'octopus0',
    'Merge pull request #1900 from author/feat/big',
    'Maintainer',
    'maintainer@example.com',
    { parents: ['base0', 'branch2', 'branch3'] },
  );
  const branchCommitA = makeCommit(
    'branch2',
    'feat: first branch',
    'Author A',
    'a@example.com',
    { parents: ['base0'] },
  );
  const branchCommitB = makeCommit(
    'branch3',
    'feat: second branch',
    'Author B',
    'b@example.com',
    { parents: ['base0'] },
  );

  // The fetcher now receives `parents` and must return SHAs from ALL branch parents.
  const result = attributeMergeCommits(
    [branchCommitA, branchCommitB, octopusMerge],
    // Simulate the corrected fetcher: return both branch commit SHAs when called
    // with the octopus merge and its parent list.
    (sha, parents) => {
      if (sha !== 'octopus0') return [];
      // All non-first parents contribute; return their direct SHAs as if git
      // rev-list sha^2 sha^3 ^sha^1 returned them.
      return parents.slice(1);
    },
  );

  const a = result.find((c) => c.sha === 'branch2');
  const b = result.find((c) => c.sha === 'branch3');
  const merge = result.find((c) => c.sha === 'octopus0');

  assert.equal(a?.primaryPrNumber, 1900, 'Author A must receive PR #1900');
  assert.equal(b?.primaryPrNumber, 1900, 'Author B must receive PR #1900');
  assert.equal(merge?.primaryPrNumber, null, 'Maintainer (octopus merge) must not hold the PR');
});

test('attributeMergeCommits: nested merge never overwrites an earlier attribution', () => {
  // git log order is newest-first, so the OUTER merge (PR #200) is processed
  // before the INNER merge (PR #100) whose branch it contains. Both merges can
  // select the same branch commit. Reassigning it to the inner PR would erase
  // PR #200 from contributor statistics while both merges are cleared.
  // Instead the first attribution stands, and the inner merge keeps its own PR
  // through the no-targets fallback.
  const outerMerge = makeCommit(
    'mergeOut',
    'Merge pull request #200 from org/outer-feature',
    'Maintainer',
    'm@example.com',
    { parents: ['base0', 'mergeIn'] },
  );
  const innerMerge = makeCommit(
    'mergeIn',
    'Merge pull request #100 from org/inner-feature',
    'Maintainer',
    'm@example.com',
    { parents: ['base0', 'branch1'] },
  );
  const branchCommit = makeCommit(
    'branch1',
    'feat: shared branch work',
    'Contributor',
    'c@example.com',
    { parents: ['base0'] },
  );

  const fetch = (sha) => (sha === 'mergeOut' || sha === 'mergeIn' ? ['branch1'] : []);
  // git log emits newest-first: outer merge, then inner merge, then the branch commit.
  const result = attributeMergeCommits([outerMerge, innerMerge, branchCommit], fetch);

  const branch = result.find((c) => c.sha === 'branch1');
  const inner = result.find((c) => c.sha === 'mergeIn');
  const outer = result.find((c) => c.sha === 'mergeOut');
  assert.equal(branch.primaryPrNumber, 200, 'first (outer) attribution must stand');
  assert.equal(outer.primaryPrNumber, null, 'outer merge is cleared');
  assert.equal(inner.primaryPrNumber, 100, 'inner merge keeps PR #100 via the fallback');

  const stats = collectContributorStats(result, new Set());
  const contributor = stats.find((s) => s.name === 'Contributor');
  const maintainer = stats.find((s) => s.name === 'Maintainer');
  assert.deepEqual(contributor?.prs, [200], 'contributor keeps the outer PR');
  assert.deepEqual(maintainer?.prs, [100], 'PR #100 is preserved, not erased');
});

// ---------------------------------------------------------------------------
// Integration test — real git repository, no network access
// ---------------------------------------------------------------------------
//
// Creates a temporary git repo with a two-parent merge topology to exercise
// the full collectCommits → attributeMergeCommits → collectContributorStats
// pipeline, including the real `git log` format string and `git rev-list` call.
// This guards against the `%P` format field in collectCommits and the
// rev-list invocation in makeBranchShasFetcher both being correct.

test('integration: regular merge credits branch author via real git topology', async () => {
  const { mkdtempSync, rmSync } = await import('node:fs');
  const { execFileSync } = await import('node:child_process');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');

  const tmpDir = mkdtempSync(join(tmpdir(), 'oc-release-notes-test-'));
  const origCwd = process.cwd();

  try {
    const run = (args, opts = {}) =>
      execFileSync('git', args, { encoding: 'utf8', cwd: tmpDir, ...opts }).trim();

    run(['init', '-b', 'main']);
    run(['config', 'user.email', 'maintainer@test.invalid']);
    run(['config', 'user.name', 'Maintainer']);

    // Base commit — the "from" boundary of the release range.
    run(['commit', '--allow-empty', '-m', 'chore: init']);
    const fromRef = run(['rev-parse', 'HEAD']);

    // Feature branch: one commit by Contributor.
    run(['checkout', '-b', 'feature']);
    run(['-c', 'user.name=Contributor', '-c', 'user.email=contributor@test.invalid',
      'commit', '--allow-empty', '-m', 'feat: add widget']);

    // Merge back to main as Maintainer with a PR-style subject.
    run(['checkout', 'main']);
    run(['merge', '--no-ff', 'feature', '-m', 'Merge pull request #7 from owner/feature']);
    const toRef = run(['rev-parse', 'HEAD']);

    // Run the full attribution pipeline against the real repository.
    process.chdir(tmpDir);
    const rawCommits = collectCommits(fromRef, toRef);
    const commits = attributeMergeCommits(rawCommits, makeBranchShasFetcher());
    process.chdir(origCwd);

    const stats = collectContributorStats(commits, new Set());
    const contributor = stats.find((s) => s.name === 'Contributor');
    const maintainer = stats.find((s) => s.name === 'Maintainer');

    assert.ok(contributor, 'Contributor must appear as a contributor');
    assert.deepEqual(contributor.prs, [7], 'PR #7 must be attributed to Contributor');
    assert.ok(maintainer, 'Maintainer must appear (authored the merge commit)');
    assert.deepEqual(maintainer.prs, [], 'Maintainer must not hold PR #7');
  } finally {
    try { process.chdir(origCwd); } catch { /* ignore */ }
    rmSync(tmpDir, { recursive: true, force: true });
  }
});

test('integration: octopus merge credits every branch parent via real git topology', async () => {
  // Exercises the real makeBranchShasFetcher rev-list against an octopus merge:
  // commits reachable only through the third parent must still be attributed.
  const { mkdtempSync, rmSync } = await import('node:fs');
  const { execFileSync } = await import('node:child_process');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');

  const tmpDir = mkdtempSync(join(tmpdir(), 'oc-release-notes-octopus-'));
  const origCwd = process.cwd();

  try {
    const run = (args, opts = {}) =>
      execFileSync('git', args, { encoding: 'utf8', cwd: tmpDir, ...opts }).trim();

    run(['init', '-b', 'main']);
    run(['config', 'user.email', 'maintainer@test.invalid']);
    run(['config', 'user.name', 'Maintainer']);

    run(['commit', '--allow-empty', '-m', 'chore: init']);
    const fromRef = run(['rev-parse', 'HEAD']);

    run(['checkout', '-b', 'feature-a']);
    run(['-c', 'user.name=Author A', '-c', 'user.email=a@test.invalid',
      'commit', '--allow-empty', '-m', 'feat: work on branch a']);
    run(['checkout', 'main']);

    run(['checkout', '-b', 'feature-b']);
    run(['-c', 'user.name=Author B', '-c', 'user.email=b@test.invalid',
      'commit', '--allow-empty', '-m', 'feat: work on branch b']);
    run(['checkout', 'main']);

    // Three-parent merge: commits from feature-b are reachable only via sha^3.
    run(['merge', '--no-ff', 'feature-a', 'feature-b',
      '-m', 'Merge pull request #8 from owner/octopus']);
    const toRef = run(['rev-parse', 'HEAD']);

    process.chdir(tmpDir);
    const rawCommits = collectCommits(fromRef, toRef);
    const commits = attributeMergeCommits(rawCommits, makeBranchShasFetcher());
    process.chdir(origCwd);

    const stats = collectContributorStats(commits, new Set());
    const authorA = stats.find((s) => s.name === 'Author A');
    const authorB = stats.find((s) => s.name === 'Author B');
    const maintainer = stats.find((s) => s.name === 'Maintainer');

    assert.deepEqual(authorA?.prs, [8], 'Author A must receive PR #8');
    assert.deepEqual(authorB?.prs, [8], 'Author B (third parent) must receive PR #8');
    assert.deepEqual(maintainer?.prs ?? [], [], 'Maintainer must not hold PR #8');
  } finally {
    process.chdir(origCwd);
    rmSync(tmpDir, { recursive: true, force: true });
  }
});

test('serializeOpenAiPayload trims non-PR collections to reach the cap', () => {
  // A first-release range is mostly uncategorized commits. Trimming only PRs
  // could never get under the ceiling, so the request was rejected for size
  // after discarding the very PRs it existed to describe.
  const payload = {
    repo: 'tinyhumansai/opencompany',
    range: { from: 'root', to: 'v1', resolvedTo: 'v1', compareUrl: 'https://x/compare' },
    totals: { commits: 9000, pullRequests: 2, contributors: 400, newContributors: 1 },
    contributors: Array.from({ length: 400 }, (_, i) => ({
      name: `Person ${i}`,
      commits: 1,
      prs: [],
      isNew: i === 0,
    })),
    pullRequests: [
      { number: 1, title: 'a', url: 'https://x/1', author: 'ada', labels: [], body: 'b'.repeat(600), commits: [] },
      { number: 2, title: 'b', url: 'https://x/2', author: 'bo', labels: [], body: 'b'.repeat(600), commits: [] },
    ],
    uncategorizedCommits: Array.from({ length: 8700 }, (_, i) => ({
      sha: String(i),
      subject: 'chore: something reasonably wordy '.repeat(3),
      authorName: `Person ${i % 400}`,
    })),
  };

  const serialized = serializeOpenAiPayload(payload);
  assert.ok(serialized.length <= 120_000, `payload still ${serialized.length} chars`);

  const parsed = JSON.parse(serialized);
  assert.ok(parsed.uncategorizedCommits.length < 8700);
  assert.ok(parsed.omittedUncategorizedCommits > 0);
  // The PRs are what the notes are about — they must survive the trim.
  assert.equal(parsed.pullRequests.length, 2);
});
