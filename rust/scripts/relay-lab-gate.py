#!/usr/bin/python3 -I
"""The lab runners' gate on a target host of the relay acceptance (TL-7).

A lab runner reaches a target host, a VM host or the other durable Mac, over
SSH with a key of its own, whose `authorized_keys` entry is

    restrict,command="/usr/bin/python3 -I <this file>" ssh-ed25519 ... relay-lab

so the key runs this gate and nothing else: no shell, no forwarding, no
terminal. The gate keeps the lab to one folder beside it, `host/`, which is
RELAY_HOST_DIR of scripts/relay-acceptance-host.sh, and allows three things:

    deploy                         replace host/ with the gzipped tar on
                                   standard input, after `clean` of what an
                                   earlier run left there
    relay-acceptance-host.sh ARGS  run the host script there, as the suite's
                                   --host-shell does (the path is ignored)
    clean                          the host script's `clean`, and remove host/

The deployed host.env gets PILOT_REPO from the installed instance's
LaunchAgent, read here at run time, so the lab's instance finds the images
the installed one does without the runner knowing where they lie.
"""

import os
import plistlib
import shlex
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
HOST = os.path.join(HERE, "host")
SCRIPT = "relay-acceptance-host.sh"
INSTALLED = os.path.expanduser("~/Library/LaunchAgents/com.wezzard.machine-service.plist")


def host_script(args):
    script = os.path.join(HOST, SCRIPT)
    if not os.path.isfile(script):
        sys.exit("relay-lab gate: nothing is deployed")
    env = dict(os.environ, RELAY_HOST_DIR=HOST)
    return subprocess.call(["/bin/sh", script, *args], env=env)


def clean():
    if os.path.isfile(os.path.join(HOST, SCRIPT)):
        host_script(["clean"])
    shutil.rmtree(HOST, ignore_errors=True)


def pilot_repo():
    try:
        with open(INSTALLED, "rb") as handle:
            return plistlib.load(handle).get("EnvironmentVariables", {}).get("PILOT_REPO")
    except (OSError, plistlib.InvalidFileException):
        return None


def deploy():
    clean()
    staging = HOST + ".new"
    shutil.rmtree(staging, ignore_errors=True)
    os.makedirs(staging, mode=0o700)
    # bsdtar refuses absolute paths and `..` without -P.
    status = subprocess.call(["/usr/bin/tar", "-xzf", "-", "-C", staging], stdin=sys.stdin.buffer)
    if status != 0:
        shutil.rmtree(staging, ignore_errors=True)
        sys.exit(f"relay-lab gate: the deployment did not unpack ({status})")
    repo = pilot_repo()
    if repo:
        with open(os.path.join(staging, "host.env"), "a") as env:
            env.write(f"PILOT_REPO={shlex.quote(repo)}\n")
    os.rename(staging, HOST)
    print("deployed")
    return 0


def main():
    words = shlex.split(os.environ.get("SSH_ORIGINAL_COMMAND", ""))
    if words == ["deploy"]:
        return deploy()
    if words == ["clean"]:
        clean()
        print("cleaned")
        return 0
    if words and os.path.basename(words[0]) == SCRIPT:
        return host_script(words[1:])
    sys.exit("relay-lab gate: allowed: deploy | clean | relay-acceptance-host.sh <args>")


if __name__ == "__main__":
    sys.exit(main())
