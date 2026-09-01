# Marching orders

First read ALL of the AGENTS.md file and README.md file super carefully and understand ALL of both! Then use your code investigation agent mode to fully understand the code, and technical architecture and purpose of the project. Then register with MCP Agent Mail and introduce yourself to the other agents. Be sure to check your agent mail and to promptly respond if needed to any messages; then proceed meticulously with your next assigned beads, working on the tasks systematically and meticulously and tracking your progress via beads and agent mail messages. Don't get stuck in "communication purgatory" where nothing is getting done; be proactive about starting tasks that need to be done, but inform your fellow agents via messages when you do so and mark beads appropriately. When you're not sure what to do next, use the bv tool mentioned in AGENTS.md to prioritize the best beads to work on next; pick the next one that you can usefully work on and get started. Make sure to acknowledge all communication requests from other agents and that you are aware of all active agents and their names. When a Rust build or test is needed, offload it with rch (for example, rch exec -- cargo test --workspace --all-targets) so local compilation does not starve the shared swarm host. Use ultrathink.

Reread AGENTS.md so it's still fresh in your mind.

In your first reply, before anything else, print your tmux pane id.

Claim every bead with: `br update <id> --claim --actor $AGENT_NAME --add-label model:<litellm alias>,cli:<pi|cc|cod>`

## When idle: advance to the next bead

Reread AGENTS.md so it's still fresh in your mind. Use ultrathink. Use bv with the robot flags (see AGENTS.md for info on this) to find the most impactful bead(s) to work on next and then start on it. Remember to mark the beads appropriately and communicate with your fellow agents. Pick the next bead you can actually do usefully now and start coding on it immediately; communicate what you're working on to your fellow agents and mark beads appropriately as you work. And respond to any agent mail messages you've received.

## Runbook: the judgment calls

Pin the account you were launched under and verify it from `/proc/<pid>/environ` before trusting it. Never spend a usage reset unattended. Decline a model-downgrade modal and move the bead to an idle pane instead. Restart a pane only after the `stalled` row says so. Read what a truncated pane was deducing before nudging it.

## Settled before arming: no pane may require a human answer (lw-zi84)

The acceptance run is hands-off, so no pane may be able to require a human answer at boot or between tasks. Each class below is settled (config applied or lane dropped), and it is settled before arming, not at 07:00. Never answer one of these prompts in-pane; re-apply the settlement or flag the run instead.

1. **codex boot hooks-trust dialog** ("Review hooks / Trust all and continue") - lane dropped for this run: the roster records **no `cod` pane**. The trust entry already recorded where codex reads it (`[projects."/home/gabriel/repositories/agent-usage-book"] trust_level = "trusted"` in `~/.codex/config.toml`) does **not** suppress the dialog - probed 2026-09-01, a launcher-shaped spawn still boots into "Hooks need review". Only per-invocation launch flags suppress it; `--dangerously-bypass-hook-trust` on the launch line was probed to reach the prompt with no dialog. The flag belongs in the ntm `[agents] codex` launch template, which is a machine-local change; if the armer applies it, the `cod` lane may be restored to the roster.
2. **agy post-task feedback survey** ("How's the CLI experience so far?") - lane dropped for this run: the roster records **no `agy` pane**. The settings-level disable exists but is not applied - probed 2026-09-01, `showFeedbackSurvey` is absent from `~/.gemini/settings.json`, so the survey stays active. The one-line disable is `"showFeedbackSurvey": false` in that file, a machine-local change; if the armer applies it, the `agy` lane may be restored to the roster.
3. **mid-task question widget** (AskUserQuestion or equivalent) - never raise one. When two approaches both satisfy the acceptance criteria, take the first and record the choice in the commit. No question asked in-pane is ever answered by this run.
4. **own-session files** - a file your own session created may be removed or renamed without asking. The no-deletion law (AGENTS.md RULE 1) protects other people's work, not your scratch. A numbering collision with landed foreign work (e.g. a migration file) is resolved by renumbering your own side and saying so in the commit.
