---
name: release-check
description: Verify that a release is ready: changelog, version, build, tests, and artifacts.
allowed_tools: Read, Grep, Glob, Bash, Git
---

Prepare a release-readiness report.

- Read the changelog and compare it with merged changes since the last tag.
- Verify the package version is consistent across manifests and lockfiles.
- Run the documented formatting, lint, and test commands.
- Inspect the release build command and any packaging scripts.
- Check for uncommitted changes, missing migrations, and TODOs in user-facing
  code.
- Produce a checklist with pass/fail results and exact commands. Do not tag,
  publish, or push anything without explicit approval.
