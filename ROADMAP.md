# Roadmap

What exists, what is missing, and what is deliberately out of scope. Direction, not a
work queue.

## Exists

- The engineering harness: worktree tooling, versioned git hooks, `bin/ci`, CI on every
  push and pull request, a release workflow bound to a `v*` tag.
- The licence position: MIT, with the ported MIT work attributed in `NOTICE`.
- The correctness rules the implementation has to satisfy, written down in `AGENTS.md`
  before any code exists, because they are the reason the project exists.

## Missing

- **Token spend from transcripts.** Read the files agent CLIs leave on disk and attribute
  spend per account, per model and per period.
- **Quota measurement.** Query the providers' endpoints for the state of each window, and
  report each reading with its freshness.
- **One report joining both**, in a form a person reads and a form a script parses.
- **Persistence.** Readings have to survive between runs to be comparable over time. The
  store is not chosen yet; SQLite is the candidate.
- **The first release.** A tagged binary for the supported platforms, with checksums.

## Deliberately out of scope

- **A server, a daemon or a container.** This is a binary that runs, answers and exits.
- **An async runtime.** About three endpoints are called, concurrently and once. Blocking
  requests in scoped threads cover it, and a runtime would be carried for nothing.
- **Cost forecasting and budget enforcement.** Measuring what happened is a different
  problem from predicting what will, and mixing them would make the measurement layer
  answerable for a guess.
- **Provider coverage for its own sake.** An endpoint is added when something is actually
  being routed through it.

## Undecided

- Publishing to crates.io. The binary is installable from a release and from source; a
  crate is a distribution choice that has not been made.
