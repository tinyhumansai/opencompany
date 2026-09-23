# How the hourly run works

Every hour, on the hour, whether or not anybody is looking.

1. **`health_watch` takes the reading.** Every question the server offers, in
   order, including the ones that were boring last hour. A question skipped is
   a blind spot created.
2. **`incident_analyst` compares it against the last run.** Not against its
   memory of the last run — against the `conditions` rows.
3. **The gate.** Did anything change? If yes, `runbook_keeper` writes the
   finding. If no, the run is recorded and nobody is told.
4. **The report.** A finding to the ops desk, or a recorded run and silence.

## What does not happen

The run does not report the current state. A restarting pod that was
restarting last hour is not news, and reporting it hourly is how a monitor
gets filtered into a folder.

The run does not go quiet when a question fails. "Could not be measured" and
"measured, and fine" are different readings, and only one of them is
reassuring — so a failed question is part of the reading, named.

## Once a day

At 09:00, the all-clear: headline numbers, when the last finding was, and
which questions could not be answered today. It is posted whether or not
anything happened, because a monitor that only speaks when something is wrong
is indistinguishable from one that has died — and the difference is otherwise
discovered during an incident.

See [[What to do first]] for the runbook, and [[What this cannot see]] for the
limits.
