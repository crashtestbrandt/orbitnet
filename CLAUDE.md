# OrbitNet — Claude Code context

**CONTRIBUTING.md is the authority on this repository**: the layout, the two grep-enforced boundaries, the
GDScript rules, the addon-sync model, where coverage belongs, and how decisions get recorded. Read it first and
follow it. This file adds one thing that document does not cover, because it governs what you write for a human
rather than what you write for the compiler.

## Writing for humans — never aphoristic

Nothing you write for a human reader may be aphoristic. That covers PR titles, commit messages, issue and review
comments, release notes, the `docs/` pages, the README, and the header comments that CONTRIBUTING.md asks you to
write at length. Write the plain declarative statement of what changed and what it now does. Specifically, do
not write:

- metaphor, euphemism, or an oblique stand-in for the thing you actually mean;
- epigrams and rhetorical inversions — "A *X* is not a *Y*", "*X* is *Y* before it is *Z*", "not *X*, but *Y*";
- a teaser clause joined by a colon to the real content;
- emphatic capitalization, or a general truth standing in for the specific change.

A reader who has not seen the diff, the issue, or this file must learn what changed from the sentence alone.

**A PR title is a release-notes line.** GitHub's generated notes quote merged PR titles verbatim, and
`release.yml` publishes those notes on every tag, so a title is read by people who will never open the diff.

The form is `<type>(<area>) <plain description> (#issue)`. Type is one of `feat`, `fix`, `perf`, `refactor`,
`docs`, `test`, `build`, `chore`; area is the part of the addon the change lands in (`facade`, `rollback`,
`state`, `interest`, `priority`, `clock`, `codec`, `transport`, `steam`, `netbench`, `rts`, `harness`, `ci`, …).

| Not this | This |
| --- | --- |
| A rota that only ever asks who waited longest is not a priority | `feat(priority) order the send rota by staleness times a distance weight (#12)` |
| A window measured in ticks is a different policy at every rate | `fix(lagcomp) denominate the rewind window in milliseconds rather than ticks (#31)` |
| The bench could not see the thing players complain about | `feat(netbench) measure how often a remote body's pose reaches the client (#33)` |
| An addon that names its host project is not extractable | `refactor(bench) reach the game through BenchSubject instead of naming its classes (#2)` |

Use bold for the term a rule is about, not for emphasis on the sentence as a whole. Where a rule has a
counter-intuitive consequence, state the consequence rather than gesturing at it.

**This is a public repository, so write for a reader who has only this repository.** Do not name another
project's classes, scenes, arenas, weapons, issue numbers, or docs pages in a comment here — a reader cannot
resolve any of them, and a header comment is supposed to carry the reasoning rather than point at it. Where a
concrete example earns its place, describe it in the general terms of the thing being explained: "a fat channel
of 41 `i64` props" rather than the name of the class that happened to have them.
