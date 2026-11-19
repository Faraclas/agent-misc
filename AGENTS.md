# Repository Agent Instructions (`agent-misc`)

This repository hosts several independent, agent-agnostic projects. Each project lives in its own top-level folder and
is self-contained: its code, `docs/`, `issues/`, `packaging/`, and its own `AGENTS.md`.

- Before changing anything inside a project folder, read and follow that folder's `AGENTS.md`
  (for example [`utcp-rs/AGENTS.md`](./utcp-rs/AGENTS.md)).
- Keep project-specific files out of the repository root. The root holds only the project index (`README.md`),
  `LICENSE`, this file, shared lint config, and `.github/workflows/`.
- Each project has its own workflow file, `.github/workflows/<project>.yml`, filtered with `paths: <project>/**`, and
  its own release tags, `<project>-vX.Y.Z`.
- Adding a project means adding its folder, its `AGENTS.md`, its workflow (if any), and a `README.md` index entry.
