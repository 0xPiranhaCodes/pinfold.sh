# Shell integration

No scripts ship yet. OSC 133 and OSC 7 scripts for bash, zsh and fish are planned.

Without OSC 7, PinFold reads a pane's directory from its shell process, so `pinfold agent` and
session restore work with a stock shell. A shell that sends OSC 7 overrides that
([ADR-0040](../docs/adr/0040-pane-directory-falls-back-to-its-shell.md)).
