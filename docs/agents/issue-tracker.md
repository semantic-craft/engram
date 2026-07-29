# Issue tracker: GitHub

Issues and PRDs for this repository live in GitHub Issues under
`semantic-craft/engram`. Use the `gh` CLI for issue operations.

## Conventions

- Create an issue with `gh issue create`.
- Read an issue and its discussion with `gh issue view <number> --comments`.
- List and filter issues with `gh issue list` and structured JSON output.
- Comment with `gh issue comment <number>`.
- Apply or remove labels with `gh issue edit <number>`.
- Close an issue with `gh issue close <number>`.

GitHub shares one number space across issues and pull requests. Resolve an
ambiguous reference as a pull request first, then fall back to an issue.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Skill routing

When an engineering skill says to publish to the issue tracker, create a GitHub
issue in `semantic-craft/engram`. When it asks for a relevant ticket, read the
issue body, labels, and comments before acting.
