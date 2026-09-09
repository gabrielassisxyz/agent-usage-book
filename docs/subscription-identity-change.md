# Subscription identity change: detection and recovery

When the subscription behind an account's credential path changes (one login
replaced by another's under the same file), `aub` refuses to attribute the
new subscription's readings to the old logical name. This note says what
fires, what the operator does next, and how the affected interval is marked
(aub-iwkg).

## What fires

On the first tick whose reading names a different subscription than the
account's established one:

- No observation is stored for that reading: no `meter_observation` row, no
  `meter_window` rows, no spool entry. A well-formed reading from the wrong
  subscription is still the wrong subscription.
- The attempt result is committed with outcome `Unreachable` and class
  `subscription_changed`, so `aub status` renders the account stale (reason
  "credential changed") instead of ordinarily fresh.
- One row lands in `meter_subscription_change` with kind `changed`,
  naming the established identity, the intruding identity, the detecting
  attempt, and the newest stored observation as the interval anchor.
- `aub doctor` fails `subscription-identity-change` for the account while
  its newest history row is a change with no newer stored observation.

Repeat refusals for the same identity pair record attempt results but no
second history row: one row per episode, not one per tick.

## What counts as the subscription

The identity is the stable subscription fields of the credential material,
as each adapter interprets them; no new provider call is involved:

- Anthropic: `subscriptionType` and `rateLimitTier` from the credential
  file. Survives the rotating-pair refresh, which preserves every other
  field. Two subscriptions sharing one plan family and tier share an
  identity and are not detected; that weakness is stated, not fixed, here.
- Antigravity: a digest of the stable refresh token. Survives the
  access-token rotation. The digest reveals nothing usable.
- Codex: a digest of the JWT email, falling back to the plan label.
- Everywhere else: absent, and absent never blocks a reading.

An ordinary in-place token refresh for the same subscription is not a
change on any of these paths.

## Recovery

1. Run `aub doctor` and read the `subscription-identity-change` line: it
   names the account, the established and intruding identities, and the
   change id.
2. Find out which subscription is now behind the path. For an Anthropic
   file credential, compare the `subscriptionType` in the file with the one
   the previous login carried (the `.bak` beside the credential keeps the
   previous pair after a refresh; a backup of the state directory keeps
   older ones). Confirm with whoever owns the login.
3. If the switch was accidental (the wrong `claude login`, a profile
   pointing at the wrong home), log back into the original subscription.
   The next tick matches the established identity and sampling resumes on
   its own; the change row stays as the history of the gap, and `aub
   doctor` passes with a historical note once a newer observation exists.
4. If the switch was deliberate (the old subscription is gone and the new
   one is what this machine should meter), do not keep sampling it under
   the old name: that re-creates the silent seam this detection exists to
   end, with one subscription's consumption filed under another's name.
   Rename instead: replace the account stanza with a fresh logical name
   for the new subscription and remove the old one. The new name
   establishes on its first tick; the old name's history stays intact and
   queryable. There is deliberately no in-place re-acknowledgement: a
   logical name denotes one subscription epoch.
5. Two accounts must never share one credential path. If the rename left
   the old and the new stanza pointing at the same file, configuration
   resolution refuses it before anything samples.

## Marking the affected interval

Nothing is rewritten, so marking means querying. The untrustworthy stretch
for one account runs from its change row's `detected_at` to the first
observation stored after it (or to now, while readings are still refused):

```sql
SELECT id, previous_identity, current_identity, detecting_attempt_id,
       previous_observation_id, detected_at
  FROM meter_subscription_change
 WHERE account_id = (SELECT id FROM account
                      WHERE provider_key = '<provider>'
                        AND logical_name = '<name>')
 ORDER BY id;
```

The refused ticks are independently visible as attempt results with
outcome `unreachable` and classification `subscription_changed`:

```sql
SELECT attempt_id, completed_at
  FROM meter_attempt_result
 WHERE sanitized_error_classification LIKE 'subscription_changed%'
 ORDER BY completed_at;
```

Exclude evidence with `received_at` inside that span from calibration
fits: every reading in it is well-formed and misattributed, which no
downstream well-formedness check looks for.

## Fallback

If detection proves too eager in practice, reduce the condition to a
doctor finding without blocking storage: keep recording the
`meter_subscription_change` row on a mismatch and commit the observation
anyway. That is a configuration change in the sampler gate, not a schema
revert; the table and its history stay as they are.
