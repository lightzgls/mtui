# ADR 0001: Use the TUI as the primary interface

**Status:** Accepted\
**Date:** 2026-10-06

## Context

MTUI already has a functioning Ratatui interface connected to playback,
browsing, sign-in, queue management, lyrics, comments, related music, artwork,
Discord presence, and tray controls. A Slint GUI prototype created a second UI
and navigation model before feature parity existed. It slowed validation and
drifted from the product requirements.

The project targets roughly 50 MiB of steady-state memory and needs fast,
repeatable UI verification.

## Decision

The Ratatui/Crossterm TUI is the only primary product interface. All current and
planned product features will be designed for it with keyboard and mouse
support. Ratatui `TestBackend` rendering is the standard UI test path.

The Slint prototype, its extracted navigation crate, Figma trace data, and its
dependencies are removed from the repository. A future GUI would require a new
decision and must first demonstrate a clear product advantage, full feature
parity, immediate automated testability, and compliance with the memory target.

## Consequences

- Feature work improves the interface users already run.
- UI layouts and states can be tested immediately as terminal cell buffers.
- The dependency graph and shipped runtime remain smaller.
- Terminal capability differences require explicit fallbacks and Windows smoke
  tests.
- Conventional GUI interactions are translated into terminal-appropriate mouse
  actions rather than copied directly.
