# Rewind an agent's changes

Every agent PinFold launches, with `pinfold agent <profile>` or `agent.launch`, gets a checkpoint
of its working directory before it starts, and more when its status becomes ✓ done and when it
exits. Rewind shows what the agent changed since a checkpoint and puts files back as they were,
sandboxed or not, inside a git repository or outside one
([ADR-0050](../adr/0050-rewind-an-agents-changes.md)). Checkpoints are git commits in a private
store under PinFold's state directory, never in your repository. Rewind needs `git`; the
[`[rewind]` keys](../configuration.md#checkpoints) set when checkpoints are taken and how long
they are kept.

## Open the changes

Either way opens the diff viewer beside the agent's pane, on what changed since the run's launch
checkpoint.

- **The palette.** With the agent's pane focused, press Ctrl+Shift+P and choose **Rewind Agent
  Changes**. It is offered only when the focused pane runs an agent.
- **`pinfold rewind`**, in any PinFold pane: the run in that pane, else in the agent pane you
  focused last.

  ```sh
  pinfold rewind                    # beside this pane, since the launch checkpoint
  pinfold rewind 3 --tab            # the agent in pane 3, in a new tab
  pinfold rewind --checkpoint 2     # since the run's checkpoint 2
  ```

  `--down` splits below instead of to the right.

Opening the viewer takes a `view` checkpoint of the directory as it is now, and the viewer shows
the difference between the two. A file marked `later`, in yellow, changed after the agent
stopped, probably by you. Nested repositories, which checkpoints skip, are listed at the end as
`nested repository, not covered`. Staging, reverting and sending a hunk are off in this mode. `q`
quits and deletes the view checkpoint, unless a restore used it.

## Undo

- `u` undoes the selected file, `later` or not.
- `U` undoes every file not marked `later`. Undo a `later` file with `u`, one at a time.

Neither changes a file on its own. Each asks PinFold, which shows a prompt in the window, outside
every pane, so no program's output can draw or answer it:

```text
── PinFold rewind ───────────────────────────────────────────────────────────────
 ◆ needs you  Undo changes in /Users/me/work/pinfold since 12 min ago (launch)?
asked by the tool in pane 3
restore 1 · delete 1 the agent added · bring back 1 it deleted · 1 changed later
the agent is still running, and may change these files again
  restore src/main.rs
  delete notes.txt
  bring back README.md
and 2 more
 y undo once · n/Esc keep the files · the 9 rows above are this prompt
```

The counts and paths come from the checkpoint store, not from the agent. A path in red changed
later. `y` undoes once; `n` or Esc keeps the files. A prompt not answered within 60 s closes, and
a `y` never becomes a lasting grant. The window then flashes `✓ done · undid 3 files, 1 skipped`,
or `✕ error · <why>`. Before it changes anything, the restore takes a `restore` checkpoint, so a
rewind can itself be rewound with `pinfold rewind --checkpoint <n>`.

Only you can start a restore: from the viewer, which is a tool pane, or from a terminal outside
every PinFold pane. An agent pane or a plain shell pane asking `checkpoint.restore` is refused
before any prompt, and with no PinFold window open every restore is refused.

## The strip

When a run has no checkpoint, the line under its pane says why, in grey:
`no checkpoint · took over wait_s`, `git not found`, `the take failed`, `too many runs` or `busy`.
The agent still runs; there is nothing to rewind to until a later checkpoint of the run succeeds.

## What a restore never touches

- The agent's commits, branches and anything else in `.git`. Undo those with git.
- Anything outside the working directory, such as the agent's writes to `~/.bashrc`.
- Nested repositories and submodules below the working directory.
- What the tree's `.gitignore` files ignore, unless `include_ignored = true`. Files named `.env*`
  are covered even when ignored, except inside an ignored directory.
- Files over `max_file_mib`, files with more than one hard link, fifos, sockets and devices.
- PinFold's own state directory, when the agent runs in `~`.
- A file that changed since the viewer showed it: it is skipped and reported.
- Anything a checkpoint did not cover. A checkpoint cut short by `max_files` or `max_new_mib`
  is incomplete, and a restore from it deletes nothing.

A restore never follows a link: it writes each file as a new file and renames it over the name,
so a link the agent planted is replaced, not written through.

Rewind is a convenience, not a backup. An agent that is not sandboxed runs as you and can change
or delete its own checkpoints. Runs from before the core last restarted cannot be reached, and
checkpoints are pruned after `keep_days`, past `keep_per_run` and over `max_store_mib`.
[Known issues](../beta/known-issues.md#rewind) lists the rest.
