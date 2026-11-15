#!/usr/bin/python3 -I
"""The forced command of the relay acceptance's restricted key (DC-307, DC-343).

A client VM's host control reaches a target Mac only through this command,
which `relay-acceptance-matrix.sh up` authorizes with `restrict` beside the
host script in the host folder. It runs only that folder's
relay-acceptance-host.sh with one of the subcommands the suite needs, its
arguments passed as a list, never through a shell.
"""
import os
import re
import shlex
import sys

HOST_DIR = os.path.dirname(os.path.realpath(__file__))
SCRIPT = os.path.join(HOST_DIR, "relay-acceptance-host.sh")
ALLOWED = {"exposure", "restart", "rig-ctl", "rig-start", "rig-stop", "rig-status", "status"}
SAFE = re.compile(r"^[A-Za-z0-9._:,{}\[\]\" -]{0,2048}$")


def refuse(why):
    sys.stderr.write("host-control: refused: %s\n" % why)
    sys.exit(126)


try:
    words = shlex.split(os.environ.get("SSH_ORIGINAL_COMMAND", ""))
except ValueError as error:
    refuse("unparsable command (%s)" % error)
if len(words) < 2 or os.path.realpath(words[0]) != SCRIPT:
    refuse("only %s may run" % SCRIPT)
if words[1] not in ALLOWED:
    refuse("subcommand %r is not allowed" % words[1])
if any(not SAFE.match(word) for word in words[2:]):
    refuse("an argument has characters outside the allowed set")
os.environ["RELAY_HOST_DIR"] = HOST_DIR
os.execv("/bin/sh", ["/bin/sh", SCRIPT] + words[1:])
