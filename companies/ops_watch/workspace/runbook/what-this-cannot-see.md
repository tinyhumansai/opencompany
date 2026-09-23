# What this cannot see

Written down rather than discovered during the first real outage.

## It lives inside the cluster it watches

The monitoring server runs as a pod in the cluster it reads. That placement is
what gives it pod-level detail at all — the projected service-account token,
the metrics API and per-pod state exist only from inside.

The cost is exact: **a cluster-wide outage silences this company at the moment
it is most needed.** Depth and independence want opposite placements, and this
one chose depth.

The answer is not to move the pod. It is to pair it with a small check from
outside, on different infrastructure, that watches for the hourly run going
missing. Until that exists, this is hourly depth and no independent liveness,
and saying so is the only honest position.

## It reports only what it was given a source for

The server offers a question only when it has something to answer it with. A
deployment with no Grafana token does not offer the Grafana questions; one with
no probe targets does not offer the outside-in check; the curated metric query
needs a query catalogue pinned against your own Prometheus first, because a
query that quietly matches nothing reads as "all clear".

**An absent question is a blind spot, not an all-clear.** The roster is
expected to know which ones it has and to say so in the daily post.

## It cannot see what nobody is measuring

A service with no alert, no metric and no probe is invisible here, and will go
on being invisible however many runs pass cleanly. Every clean run means "none
of the things being watched crossed a line" — never "nothing is wrong".

## It changes nothing

By design, and enforced twice over: there is no write verb in the code and none
in the cluster role. So this company can tell you production is broken and can
never fix it. That is the correct trade for something that reads attacker-
influenced text every hour, and it means every finding needs a human.
