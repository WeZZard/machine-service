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
    clean                          release every lease the lab instance holds,
                                   the host script's `clean`, and remove host/

The deployed host.env gets PILOT_REPO from the installed instance's
LaunchAgent, read here at run time, so the lab's instance finds the images
the installed one does without the runner knowing where they lie.
"""

import fcntl
import json
import os
import plistlib
import shlex
import shutil
import subprocess
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
HOST = os.path.join(HERE, "host")
SCRIPT = "relay-acceptance-host.sh"
INSTALLED = os.path.expanduser("~/Library/LaunchAgents/com.wezzard.machine-service.plist")
# The host-wide durable lease lock every instance on the Mac shares (DM-2).
LOCK = "/var/tmp/com.wezzard.machine-service/durable-machine.lock"


def host_script(args):
    script = os.path.join(HOST, SCRIPT)
    if not os.path.isfile(script):
        sys.exit("relay-lab gate: nothing is deployed")
    env = dict(os.environ, RELAY_HOST_DIR=HOST)
    return subprocess.call(["/bin/sh", script, *args], env=env)


def lab_port():
    try:
        with open(os.path.join(HOST, "host.env")) as env:
            for line in env:
                if line.startswith("RELAY_HOST_PORT="):
                    return line.split("=", 1)[1].strip()
    except OSError:
        pass
    return None


def release_leases():
    """Release what the lab instance still holds, over loopback, so that its
    VMs are deleted and its durable lease frees the Mac before it stops."""
    port = lab_port()
    if not port:
        return
    base = f"http://127.0.0.1:{port}"
    try:
        with urllib.request.urlopen(f"{base}/machines", timeout=30) as answer:
            records = json.load(answer).get("machines") or []
        # The service answers its machines keyed by name.
        if isinstance(records, dict):
            records = [dict(record, name=record.get("name", name)) for name, record in records.items()]
    except Exception as error:
        print(f"relay-lab gate: the lab instance did not list its leases: {error}")
        return
    for record in records:
        lease = record.get("lease_id")
        if not lease or record.get("state") in ("released", "available", "offer"):
            continue
        body = json.dumps({"lease_id": lease, "reason": "relay lab cleanup"}).encode()
        request = urllib.request.Request(f"{base}/machines/{record['name']}/release", data=body,
                                         headers={"content-type": "application/json"}, method="POST")
        try:
            urllib.request.urlopen(request, timeout=900).read()
            print("relay-lab gate: released a lease the lab instance held")
        except Exception as error:
            print(f"relay-lab gate: a release failed: {error}")


def free_own_lock():
    """A durable lease of the lab instance that outlived its state can never
    be released by anyone, so the lock entry naming the lab's own removed
    state folder is cleared; any other holder is left alone."""
    try:
        fd = os.open(LOCK, os.O_RDWR | os.O_NOFOLLOW)
    except OSError:
        return
    with os.fdopen(fd, "r+") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        try:
            holder = json.load(handle).get("holder")
        except ValueError:
            return
        state = (holder or {}).get("state_dir", "")
        if state.startswith(HOST + "/") and not os.path.exists(state):
            handle.seek(0)
            handle.truncate()
            json.dump({"holder": None}, handle)
            print("relay-lab gate: freed the Mac from a lease of the removed lab instance")


def clean():
    if os.path.isfile(os.path.join(HOST, SCRIPT)):
        try:
            release_leases()
        except Exception as error:
            print(f"relay-lab gate: releasing the lab's leases failed: {error}")
        host_script(["clean"])
    shutil.rmtree(HOST, ignore_errors=True)
    free_own_lock()


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
