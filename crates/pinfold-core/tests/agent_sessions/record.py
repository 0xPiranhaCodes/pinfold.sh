#!/usr/bin/env python3
"""Records a shell session with OSC 133 marks as raw PTY bytes with timing.

usage: record.py OUT.rec DIR STEP...

Runs `zsh -d -i` in DIR on a 24x80 terminal, with a prompt that sends OSC 133 A, B, C and
D;status as shell integration would. Each STEP is `expect:TEXT`, `send:TEXT` (Python
escapes such as \\r are read) or `sleep:SECONDS`. Each output read is written as
`DELAY_MS LENGTH\\n` and then LENGTH raw bytes.

The recording is then scrubbed: session IDs become zeros, and it fails if a home path, user
name, host name, email address or API key is left.
"""

import codecs, fcntl, os, pty, re, select, socket, struct, sys, termios, time

ROWS, COLUMNS = 24, 80
RC = r"""
PS1='$ '
precmd() { local s=$?; [[ -n $ran ]] && print -n "\e]133;D;$s\a"; print -n "\e]133;A\a" }
preexec() { ran=1; print -n "\e]133;C\a" }
zle-line-init() { print -n "\e]133;B\a" }
zle -N zle-line-init
"""
# Replies to the queries TUIs wait on at start: cursor position, background colour, and
# primary device attributes, which also ends any query a terminal does not answer.
QUERIES = (
    (b"\x1b[6n", b"\x1b[1;1R"),
    (b"\x1b]11;?", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (b"\x1b[c", b"\x1b[?62c"),
)


def main():
    out, directory, steps = sys.argv[1], sys.argv[2], sys.argv[3:]
    rc = os.path.join(directory, ".zshrc")
    with open(rc, "w") as file:
        file.write(RC)
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(directory)
        # A bare environment, so no token or name from this one reaches the recording.
        keep = {name: os.environ[name] for name in ("PATH", "HOME", "USER") if name in os.environ}
        os.execvpe("zsh", ["zsh", "-d", "-i"], keep | {"LANG": "en_US.UTF-8", "TERM": "xterm-256color", "ZDOTDIR": directory})
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))
    seen, last = b"", time.monotonic()
    with open(out, "wb", buffering=0) as record:

        def pump(timeout):
            nonlocal seen, last
            if not select.select([fd], [], [], timeout)[0]:
                return True
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                return False
            if not chunk:
                return False
            now = time.monotonic()
            record.write(b"%d %d\n" % (round((now - last) * 1000), len(chunk)) + chunk)
            last = now
            seen += chunk
            for query, reply in QUERIES:
                os.write(fd, reply * chunk.count(query))
            return True

        for step in steps:
            kind, _, value = step.partition(":")
            if kind == "expect":
                # TUIs often move the cursor instead of printing a space, so a space in the
                # pattern also matches control sequences.
                space = rb"(?:\x1b\[[0-?]*[ -/]*[@-~]| )+"
                pattern = re.compile(space.join(re.escape(word.encode()) for word in value.split(" ")))
                deadline = time.monotonic() + 180
                while not (found := pattern.search(seen)):
                    if time.monotonic() > deadline or not pump(0.2):
                        sys.exit(f"timed out on {step!r}; tail: {seen[-400:]!r}")
                seen = seen[found.end() :]
            elif kind == "send":
                os.write(fd, codecs.decode(value, "unicode_escape").encode())
            elif kind == "sleep":
                end = time.monotonic() + float(value)
                while time.monotonic() < end:
                    pump(0.05)
        while select.select([fd], [], [], 2)[0] and pump(0):
            pass
        os.kill(pid, 9)
    os.unlink(rc)
    scrub(out)


def scrub(path):
    data = open(path, "rb").read()
    chunks, at = [], 0
    while at < len(data):
        end = data.index(b"\n", at)
        delay, length = map(int, data[at:end].split())
        chunk = data[end + 1 : end + 1 + length]
        chunk = re.sub(rb"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", b"00000000-0000-0000-0000-000000000000", chunk)
        chunks.append(b"%d %d\n" % (delay, len(chunk)) + chunk)
        at = end + 1 + length
    data = b"".join(chunks)
    open(path, "wb").write(data)
    names = {os.environ.get("HOME"), os.environ.get("USER"), socket.gethostname().split(".")[0]}
    for name in filter(None, names):
        if name.encode() in data:
            sys.exit(f"{path} holds {name!r}: re-record without it")
    for pattern in (rb"/Users/", rb"/home/", rb"\bsk-[\w-]{8}", rb"[\w.+-]+@[\w-]+\.[\w.]+"):
        if re.search(pattern, data):
            sys.exit(f"{path} matches {pattern!r}: re-record without it")


main()
