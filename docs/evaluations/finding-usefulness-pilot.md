# Finding usefulness pilot

Status: planning-only runbook for [Issue #84](https://github.com/yuru-sha/codexlens/issues/84).
No real-history pilot has been run or recorded in this repository. Copy this
worksheet outside the repository before filling in any owner- or project-
specific values.

This is a bounded, local, human-reviewed evaluation of `doctor` findings and
`optimize --diff` proposals. It is not a population-wide accuracy study and it
does not authorize reading arbitrary history or changing instructions. The
pilot performs no raw-history upload, new external analytics service, or
background monitoring.

## 1. Authorization gate

Before reading any real history, the owner must record all of the following in
the private worksheet and authorization record. The `source/project scope`
decision is recorded as two separate fields:

| Decision | Required value |
| --- | --- |
| source scope | One explicitly named local source |
| project scope | One explicitly named project boundary |
| observation period | Start and end, including timezone and whether archives are included |
| archive inclusion | Yes or no, with the reason |
| storage location | A local path outside this repository for raw inputs, the derived store, and private notes |
| retention/deletion policy | Who may retain the material, for how long, and how it will be deleted |
| owner authorization | Owner, date, and scope confirmation |

An incomplete gate means stop. Do not put these selected values, raw logs,
excerpts, credentials, personal identifiers, or private paths in GitHub,
fixtures, or committed evaluation artifacts.

Create the private JSON authorization record before running the commands below.
It must contain non-empty string values for `source_scope`, `project_scope`,
`observation_period`, `archive_inclusion`, `storage_location`,
`retention_deletion_policy`, and `owner_authorization`.

The pilot depends on the reporting coverage and period contracts in [#82](https://github.com/yuru-sha/codexlens/issues/82)
and [#83](https://github.com/yuru-sha/codexlens/issues/83). Until those
contracts are available, run only synthetic validation or label a real run as
an unfiltered selected-store observation with its coverage limitations; do not
present it as a comparable before-and-after period.

## 2. Freeze and record the candidate

Use an owner-authorized input boundary and keep every output below outside the
repository:

```bash
set -eu

PILOT_DIR=/path/outside/repository/codexlens-pilot
PILOT_STORE="$PILOT_DIR/store.sqlite"
AUTHORIZED_CODEX_HOME=/path/owner-approved/codex-home
SELECTED_PROJECT=/path/owner-approved/project
AUTHORIZATION_RECORD="$PILOT_DIR/authorization.json"

mkdir -p "$PILOT_DIR"

python3 - "$AUTHORIZATION_RECORD" <<'PY'
import json
import sys

required = (
    "source_scope",
    "project_scope",
    "observation_period",
    "archive_inclusion",
    "storage_location",
    "retention_deletion_policy",
    "owner_authorization",
)
try:
    with open(sys.argv[1], encoding="utf-8") as stream:
        record = json.load(stream)
except (OSError, ValueError):
    raise SystemExit("authorization gate incomplete")
if not isinstance(record, dict) or any(
    not isinstance(record.get(field), str) or not record[field].strip()
    for field in required
):
    raise SystemExit("authorization gate incomplete")
PY

cargo run -- refresh \
  --codex-home "$AUTHORIZED_CODEX_HOME" \
  --store "$PILOT_STORE"
test -f "$PILOT_STORE"
cargo run -- sessions --frozen --format json --store "$PILOT_STORE" \
  > "$PILOT_DIR/sessions.json"
python3 - "$PILOT_DIR/sessions.json" "$SELECTED_PROJECT" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    sessions = json.load(stream)["data"]["sessions"]
selected = sys.argv[2]
if not sessions or any(session.get("project") != selected for session in sessions):
    raise SystemExit("project scope check failed")
PY
cargo run -- doctor --frozen --format json --store "$PILOT_STORE" \
  > "$PILOT_DIR/doctor.json"
cargo run -- optimize --diff --frozen --format json --store "$PILOT_STORE" \
  > "$PILOT_DIR/optimize-diff.json"
```

Add `--include-archived` only when it was selected at the authorization gate.
Never use `optimize --apply` for this pilot: optimize --apply requires separate review and authorization.

Project scope check: inspect `sessions.json` locally and continue only when
every non-null `project` value matches the owner-selected project. The current
refresh interface has no project filter, so an owner-prepared input directory
containing only the selected project is preferred. A mixed, unknown, or
unverifiable project set is a scope failure; discard the pilot store or mark
the run ineligible rather than silently evaluating it.

Record, without copying raw report content:

- the exact code version (`git rev-parse HEAD`);
- relevant settings and whether archives were included;
- the requested period and the resolved interval, when the period-filter
  contract is available;
- observed coverage and sample counts, including their counting semantics;
- store freshness, including the latest recorded ingestion time; and
- the fact that the store was explicitly refreshed before frozen reporting.

If activity timestamps or coverage fields are missing, record `unknown` and the
limitation. Never substitute ingestion time for unknown activity time.

## 3. Review a bounded finding sample

Run the focused JSON reports for each lens, not only `doctor`, and preserve the
same store, code version, settings, and interval for every lens. For each lens
and severity bucket:

1. record the population count before sampling;
2. select at most three findings in the report's deterministic order;
3. record the sample denominator and the number reviewed; and
4. give every reviewed finding exactly one usefulness judgment: `actionable`,
   `incorrect`, or `inconclusive`, with a short reason that contains no raw
   excerpt.

Keep usefulness separate from instruction-scope accuracy. Record the latter as
`correct`, `overbroad`, `wrong-target`, or `inconclusive`. An unavailable or
truncated historical instruction snapshot is a reason for `inconclusive`, not
evidence of a false finding.

Inspect an independent activity sample as well. Select it before reviewing the
emitted findings, record the selection method and denominator, and inspect at
most five bounded session/activity units. Record missed problems separately;
this is a coverage check, not a way to claim a population-wide accuracy rate.

Use this private worksheet shape:

| lens | severity | population denominator | reviewed | actionable | incorrect | inconclusive | reason |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
|  |  |  |  |  |  |  |  |

## 4. Review proposals without applying them

Evaluate every rendered `optimize --diff` proposal for usefulness and target
scope. Record the proposal action, target scope, evidence count, distinct
session count, usefulness judgment, scope judgment, and reason. Record skipped
proposals as limitations rather than silently dropping them. The diff is a
review artifact; applying any change needs separate review and authorization.

## 5. Compare periods only when comparable

For a before-and-after comparison, use comparable windows with the explicit intervals, timezone rules,
membership semantics, and resolved timestamps documented by #83. Keep the
code version, settings, source/project scope, archive choice, and sampling
method comparable. Report normalized quantities such as findings per session
and actionable findings per reviewed finding, with every denominator shown.
Describe observed associations as associations; do not make causal claims from
this pilot.

## 6. Publish only an aggregate

The committed or published result may contain only concise aggregate counts,
limitations, a prioritized decision, and newly constructed synthetic examples.
It must not contain raw logs, excerpts, credentials, personal identifiers, or
private paths. Keep the raw inputs, derived store, detailed worksheet, and
retained outputs under the owner-selected retention/deletion policy, then
verify that the repository and GitHub artifact contain none of them.

Synthetic example only (not a pilot result):

| lens/severity | sample denominator | reviewed | actionable | incorrect | inconclusive | reason |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| failure/medium | 4 | 3 | 2 | 1 | 0 | One repeated failure was not useful after local review |
| instructions/medium | 3 | 3 | 0 | 0 | 3 | Historical snapshot coverage was unavailable |

Synthetic proposal example: a high-confidence project-scoped `add` proposal
may be judged useful and `correct` when its bounded evidence supports the
project target. No corresponding `--apply` action is implied.

## 7. Prioritized decision

- Fix or separately escalate a confirmed high-severity failure or an
  incorrect high-confidence target.
- Create a narrowly scoped follow-up issue and a newly constructed synthetic regression case for each confirmed failure. Never copy or sanitize a real transcript into a fixture.
- Treat repeated `inconclusive` results as a coverage or metadata problem;
  prioritize the relevant contract issue rather than labeling the lens wrong.
- Leave unchanged findings with insufficient evidence, and state the reason
  and denominator.

End the private worksheet with the pilot's limitations and this decision. A
small bounded pilot can guide prioritization; it cannot establish a general
false-positive or population-wide accuracy rate.
