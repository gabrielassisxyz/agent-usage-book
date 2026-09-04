# Scheduling `aub sample`

`aub` has no daemon. Quota is measured by `aub sample`, a command that runs, does its
work, and exits; something outside the process has to invoke it on a cadence. This
document is that missing piece: the example unit files under `examples/scheduler/` and
`examples/hooks/`, and the two questions an operator has to answer to use them, are which
scheduler to point at `aub sample --due`, and where in the agent's own launch path to add
the session marker call.

Everything below assumes `aub` is already installed and on some absolute path. Find it
with `which aub`, or see the [README](../README.md#install) for install options; a
systemd unit, a cron entry, and a compositor keybinding do not read an interactive
shell's `PATH`, so every example here names that path explicitly rather than the bare
`aub` command name.

## The scheduler tick

The recommended pattern is that the scheduler invokes `aub sample --due` more often than
any account's configured sampling interval, for example once a minute against a five
minute interval. `aub` itself decides which configured accounts are actually due,
according to their own cadence and any approaching quota reset (`docs/PLAN.md` sections
5 and 14.1). A frequent, dumb tick plus a command that can say no is what makes
reset-edge sampling possible without a resident process: the interval or a reset time can
move without anyone rewriting the timer, because the timer was never told what the
interval is.

- **systemd**: [`examples/scheduler/systemd/aub-sample.service`](../examples/scheduler/systemd/aub-sample.service)
  and [`aub-sample.timer`](../examples/scheduler/systemd/aub-sample.timer). Install both
  under `~/.config/systemd/user/` (or `/etc/systemd/system/` for a system-wide account),
  then:

  ```sh
  systemctl --user daemon-reload
  systemctl --user enable --now aub-sample.timer
  ```

- **cron**, for platforms without systemd:
  [`examples/scheduler/cron/aub-sample.cron`](../examples/scheduler/cron/aub-sample.cron).
  Install with `crontab -e`, or as a file under `/etc/cron.d/` for a system-wide
  installation (that form needs an added user column).

A scheduled `aub sample --due` treats every remote outcome, including an unreachable
provider or an authentication failure, as evidence successfully recorded: the command
exits non-zero only when it could not persist or operate at all. `aub coverage` is the
mechanism for alarming on a source that has failed for too long; the timer does not need
to watch its own exit code for that.

## The hook integration

An explicit session/account marker from the launcher is the strongest evidence `aub` can
attribute usage from, ahead of provider-returned identity, ahead of a configured
credential-source mapping, and far ahead of guessing from timing (`docs/PLAN.md` section
19.2). It costs one command invocation from whatever already starts the agent session,
and it is the difference between spend landing on the right account and it landing in the
`unknown-account` bucket.

The exact invocation:

```sh
aub sample --account ACCOUNT --if-due --session-id SESSION [--run-id RUN]
```

- `ACCOUNT` must name an account already configured under `[[accounts]]` in `aub.toml`.
- `SESSION_ID` is whatever identifier the launching agent CLI assigns the session.
- `--run-id` is optional, and joins the record to a separate friction ledger that tracks
  the same run, when one exists. `aub` does not require one.

**The marker is recorded even when no poll is due.** `--if-due` only controls whether
this invocation is also allowed to reach the provider: the session/account marker itself
is written first, unconditionally, before that decision is even evaluated. A hook that
fires on every session start, possibly minutes after the scheduler tick last sampled the
same account, still records durable evidence of which account the session belongs to
without spending an extra request against the provider.

See [`examples/hooks/aub-session-start.sh`](../examples/hooks/aub-session-start.sh) for a
wrapper worth adapting: point your agent CLI's session-start hook, or a compositor
launcher keybinding, at a copy of it with `ACCOUNT` filled in.

## Bringing up a fresh machine

1. Install `aub` and note its absolute path.
2. Write `aub.toml` with at least one `[[accounts]]` entry (see the
   [README](../README.md#configuration)).
3. Install the systemd timer, or the cron entry, pointed at that path, and confirm it is
   running (`systemctl --user list-timers`, or `crontab -l`).
4. Wire the session-start hook into whatever starts an agent session on this machine.
5. `aub status` should move from `never observed` to a fresh reading within one sampling
   interval.
