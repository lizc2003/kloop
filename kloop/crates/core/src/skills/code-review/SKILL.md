---
name: code-review
description: Review a commit, a diff, or working-tree changes for defects — correctness, regressions, contract drift, test gaps. Use whenever the user asks to review, audit, or check over code, including a bare "review <sha>". Not for writing code, and not for re-reading edits you just made yourself.
context: fork
---

# Code review

Target: $ARGUMENTS

With no target, review the working tree against HEAD. A bare revision means that
commit against its parent. Read the whole diff first, then the surrounding code
the diff does not show — most real defects live in what the change stopped
doing, not in the lines it added.

## What counts as a finding

A finding is a defect someone should act on. Each one needs three things:

1. **Location** — `file:line`, the line the defect is on.
2. **The claim** — one sentence naming the defect, not the area it lives in.
3. **A failure scenario** — a concrete input reaching a concrete wrong output,
   traced through the code you read. "This could be risky" is not one.

If you cannot write the third, you have not finished investigating. Finish it,
or record the candidate as excluded with a reason. Do not let it evaporate.

## Not findings

Do not report these; if you investigated one, it goes in the excluded list:

- A pre-existing issue the change did not introduce or worsen.
- Anything a compiler, linter, type checker, or formatter would catch.
- A real issue on lines this change did not touch.
- A nitpick a senior engineer would not raise in review.
- A behavior change that is plainly the point of the change. **But note that
  "deliberate" and "correct" are different questions**: a change can do exactly
  what it set out to do and still break a caller's contract on the way. Check
  the consequence before excluding on intent.
- Missing tests or docs in general — unless this change made a specific
  behavior untested that its own tests claim to cover, or a rule file in the
  repository now contradicts the code.

## Every candidate gets a verdict, in writing

Keep a list of every suspicion you investigate. Before writing the report, each
one is exactly one of:

- **confirmed** — report it.
- **downgraded** — real, but smaller than it first looked. Report it at its true
  severity and say what reduced it.
- **excluded** — one of the cases above, or otherwise not a defect. One line,
  with the reason.

The report ends with the downgraded and excluded ones. That section is not
filler: "I checked X, it is fine because Y" is what tells the reader X was
covered, and it is the only thing standing between an investigated candidate and
silent disappearance. **A candidate you thought about across several tool calls
and then dropped without a line is the exact failure this skill exists to
prevent.**

## Verifying

The code already exists and already compiles. What needs verifying is *your
claim*, not the repository:

- When the claim is a regression, read the parent revision and prove the old
  behavior differed.
- Follow an error or return value to where the caller observes it — the status
  code, the message, the log — not just to the return statement.
- Grep for other producers and consumers before calling anything dead,
  unreachable, or the only path.
- Run the affected package's tests when a claim turns on runtime behavior you
  cannot read off the code.

Do **not** run the project's pre-merge gate — whole-suite tests, full lint, full
build, docs checks — as a review step. It cannot find the defect you are looking
for, it costs minutes, and a broken local toolchain then eats a paragraph of the
report. Those run separately, and when project instructions require them before
*merging*, that is the author's step, not the reviewer's.

## The report

Confirmed findings first, worst first. Then downgraded and excluded. Then one or
two lines on what you ran and what you did not.

- Lead with the defect. Bold marks what is wrong — never "no problems found".
- Severity by blast radius: breaks a caller, misleads the next person to touch
  this code, or costs only tidiness.
- Every finding carries its `file:line` and its failure scenario. A reader
  should be able to act without re-deriving your reasoning.
- Finding nothing is a legitimate outcome. Say it in a sentence and show the
  excluded list; do not pad it into a report.

## Scope

Review the target. An unrelated bug you notice in passing gets one line at the
end, not an investigation. Change nothing: this is a review, and the reader
decides what to act on.
