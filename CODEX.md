# How we used Codex to build OVM

OVM was built with a mixed human-and-agent workflow. Claude handled more of the
primary implementation sessions, while OpenAI Codex served as both an
implementation partner and an independent reviewer. We did not use Codex for a
single prompt that generated the repository. We used it repeatedly against the
real working tree: inspecting code, reproducing failures, making scoped changes,
running tests, reviewing diffs, and checking the fixes from earlier reviews.

## A measured snapshot

For this submission, we counted the locally recorded sessions associated with
OVM's development checkouts and benchmark workspace:

| Agent | Recorded sessions | Share |
| --- | ---: | ---: |
| OpenAI Codex | 171 | 19.8% |
| Claude | 693 | 80.2% |

These numbers are session counts, not token, commit, or line-of-code attribution.
They include short diagnostic and test sessions, so they should be read as a
transparent order-of-magnitude comparison. They show that Codex was not the
majority agent, but it was a recurring and material part of the engineering
process.

## What Codex did

### Diagnosed and implemented product fixes

Codex worked directly on OVM behavior, not only prose or review. One example
started with a real `pi update` failure. Codex traced the executable through
OVM's launcher and version layout, separated Pi's extension update from its
self-update, corrected stale release-source metadata, implemented OVM-owned
update interception, found a launcher-symlink loop in the first version of the
fix, added lifecycle coverage, and verified the final flow with the Rust test
suite and the installed CLI.

### Ran adversarial reviews in rounds

We often gave Codex a narrow brief and required a severity, file-and-line
evidence, and a reproducible failure scenario. After fixes, a new Codex pass
reviewed the fix itself. This was used for:

- cross-process install locking and partial-install visibility;
- immutable self versions, activation, rollback, and crash recovery;
- the `claudex` proxy lifecycle, process identity, atomic state writes, and
  argument forwarding;
- background refresh process behavior; and
- release registry and fallback behavior.

This process caught issues that a happy-path implementation pass could miss,
including a race-prone fixed sleep in a two-process test and a symlink loop when
the launcher resolved itself through `current_exe()`.

### Challenged security boundaries

Before the public release, Codex reviewed download integrity, redirects, archive
extraction, token handling, credential storage, environment scrubbing, and the
local proxy's bearer-key boundary. Follow-up passes checked whether the fixes
really failed closed and whether the tests exercised the security property
rather than a helper function that merely resembled it.

That distinction mattered. Codex identified gaps around listener identity and
time-of-check/time-of-use behavior, and it pointed out when redirect tests did
not actually drive the production redirect policy.

### Hardened the benchmark harness

OVM's benchmarks run old and new AI CLI versions in isolated environments.
Codex reviewed the Linux clean-room setup for paths that might escape the
temporary home directory and overwrite a developer's real Codex configuration.
The final review verified canonical-path containment, symlink refusal, and the
safe path through the harness.

### Helped prepare the repository for open source

Codex performed read-only public-release audits across Rust, TypeScript, shell,
CI, packaging, and the benchmark pipeline. It also checked that OVM used the
current registry endpoint and retained a safe upstream fallback. These sessions
helped separate release blockers from maintainability follow-ups before the
fresh public repository was created.

## The workflow that worked for us

1. A human supplied the product goal, constraints, and acceptance criteria.
2. An agent inspected the live repository and implemented or proposed a focused
   change.
3. Tests exercised both the feature and its failure modes.
4. Codex received a fresh, read-only adversarial brief with no reliance on the
   implementation conversation.
5. Findings were fixed selectively, with false positives rejected rather than
   applied blindly.
6. Codex re-reviewed the exact concerns until they were fixed or explicitly
   accepted as a design decision.

The most useful role for Codex was not autocomplete. It was an engineering peer
that could move between implementation, systems diagnosis, security review, and
test design while staying grounded in the actual repository. Using a different
agent for implementation and review also reduced the chance that one model's
assumptions would pass through every stage unchallenged.

## Privacy note

The aggregate counts and examples above were derived from local session records.
Private transcripts, local paths, credentials, personal information, and raw
session identifiers are intentionally not included in this repository.
