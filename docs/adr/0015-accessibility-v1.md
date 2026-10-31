# ADR-0015: Screen readers are a v1 non-goal

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

A CPU-drawn cell grid exposes nothing to screen readers (review finding M3). The review
offers two options: AccessKit for the focused pane's text, tabs and approval prompts, or
an explicit v1 non-goal.

## Decision

Make screen-reader support an explicit v1 non-goal, and revisit it after v0.1.
Agent states already use a glyph, a colour and a word, so they do not rely on colour.

## Consequences

Screen-reader users cannot use Pinfold v1, and in particular cannot read approval
prompts. AccessKit's memory and dependency costs are avoided until they can be measured
against the budgets.
