# OrbitNet — Claude Code context

**CONTRIBUTING.md is the authority on this repository**: layout, the two grep-enforced boundaries, the GDScript
rules, the addon-sync model, where coverage belongs, and how decisions get recorded. Read it first and follow it.
This file adds what that document does not cover: how to write for a human, and the rule this repository needs
that its upstream does not.

## Writing for humans

Everything written for a human reader — PR titles, commit messages, issue and review comments, release notes,
the README, every page under `docs/`, and the header comments CONTRIBUTING.md asks you to write at length — is
**concise, bulleted and aphorism-free**. State plainly what changed and what it now does.

- No metaphor, euphemism, or oblique stand-in for the thing you mean.
- No epigrams or rhetorical inversions ("A *X* is not a *Y*", "not *X*, but *Y*").
- No teaser clause joined by a colon to the real content.
- No emphatic capitalization, and no general truth standing in for the specific change.
- Short sections, bullets over paragraphs, tables for enumerations, identifiers in backticks. Record the rule and
  its consequence; skip the narrative of how it was discovered. Bold the term a rule is about.

A reader who has not seen the diff must learn what changed from the sentence alone.

**A PR title is a release-notes line** — GitHub's generated notes quote it verbatim and `release.yml` publishes
those notes on every tag, so it is read by people who never open the diff. Form:
`<type>(<area>) <plain description> (#issue)`, type one of `feat`, `fix`, `perf`, `refactor`, `docs`, `test`,
`build`, `chore`; area is the part of the addon the change lands in (`facade`, `rollback`, `state`, `interest`,
`priority`, `clock`, `codec`, `transport`, `steam`, `netbench`, `rts`, `harness`, `ci`, …).

| Not this | This |
| --- | --- |
| A rota that only ever asks who waited longest is not a priority | `feat(priority) order the send rota by staleness times a distance weight (#12)` |
| A window measured in ticks is a different policy at every rate | `fix(lagcomp) denominate the rewind window in milliseconds rather than ticks (#31)` |
| The bench could not see the thing players complain about | `feat(netbench) measure how often a remote body's pose reaches the client (#33)` |
| An addon that names its host project is not extractable | `refactor(bench) reach the game through BenchSubject instead of naming its classes (#2)` |

## Write for a reader who has only this repository

This is a public repository consumed by projects it knows nothing about.

- Do not name another project's classes, scenes, arenas, weapons, issue numbers or docs pages. A reader cannot
  resolve any of them, and a header comment carries the reasoning rather than pointing at it.
- Where a concrete example earns its place, describe it in general terms: "a fat channel of 41 `i64` props"
  rather than the name of the class that happened to have them.
- The same applies to strings a consumer ships: default the Steam app id and lobby tag to configurable values,
  never to one title's real ones.
