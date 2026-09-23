# What to do first

One entry per active threshold. A finding that says what is wrong and nothing
about what to do is a page at 3am that makes somebody open a terminal and
start from scratch.

**"I do not know yet" is a valid entry**, and a better one than a guess. It
tells the reader the finding is real and the response is not established, which
is true and useful. A guess dressed as a procedure wastes the twenty minutes
that mattered.

## Entries

### A pod is crash-looping

Establish when it started before anything else. A pod that began failing during
a deploy nine minutes ago names its own cause; one that began forty minutes
ago, with no deploy, does not. The workload's status and the recent events for
that namespace answer this in two questions.

### A volume is near full

The reading gives capacity, not usage, for a PVC — so a volume reported at 80%
came from a metric, and a volume reported with no ratio at all means nobody
measured it. Check which of the two you have before acting.

### A node reports pressure

Memory, disk or PID pressure on a node is about the node, not about whatever
pod is loudest on it. Read the node's headroom against requests rather than
against usage: usage is what is happening, requests are what the scheduler
already promised.

### Usage could not be measured

The metrics server is a separate deployment that is separately absent. This is
not a cluster where nothing is using CPU — it is a cluster where nobody can
see. Treat it as a finding about monitoring, not an all-clear about load.

### A threshold fired that should not have

Do not widen it during the incident. Record the condition, resolve it, and put
the threshold change on the board with a reason. A threshold edited at 3am to
stop a page is a threshold nobody decided.

---

Gaps in this note are themselves worth reporting. A threshold with no entry
here is a line that fires into a vacuum.
