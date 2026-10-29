# Serve cron delivery

`octos serve` uses the target session's durable continuation queue for cron delivery. The gateway's inbound-message path is unchanged.

## Commit point

A cron occurrence is acknowledged only after the exact session's durable queue accepts it. A one-shot job is deleted at that point. A later model-turn failure is reported by the session's normal turn lifecycle and does not re-fire the cron occurrence.

## Failure and retry

A missing target, an unopened workspace, or another queue refusal does not record success and does not delete the job. The job retains `last_error` and retries at most **5 times**, with **15 minutes** between attempts (about one hour). After the fifth refusal the job remains stored but is disabled, with `last_status = "disabled"` and the original fire time retained for diagnostics.

The original scheduled fire time is separate from retry backoff and is part of the queue dedupe key, so retrying one occurrence cannot create a second continuation.

## Missed executions

When a job is already due at service startup, it is delivered once and marked as a late delivery. Ordinary timer delivery has a 60-second grace window; only startup catch-up or a delayed retry beyond that window is marked late. A recurring job does not replay every missed interval: it catches up once, then advances from the accepted occurrence.
