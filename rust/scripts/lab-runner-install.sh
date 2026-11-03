#!/bin/sh
# Install a lab runner on a durable Mac (docs/arch/hygiene.md, HY-5): a
# GitHub Actions runner of its own folder, registered to the repository with
# the label `lab`, run by a per-user LaunchAgent of its own label in the
# operator's desktop session, with the job hooks of lab-runner-hook.sh, so
# every job holds the Mac's durable lease while it runs.
#
# Run it on the Mac, from the runner folder it creates, with the hook script
# beside it and a registration token on standard input:
#
#   gh api -X POST repos/<owner>/<repo>/actions/runners/registration-token --jq .token \
#     | ssh <mac> 'sh ~/actions-runner-lab/lab-runner-install.sh <owner>/<repo> <version> <sha256>'
#
# <version> and <sha256> are the runner release's and its osx-arm64
# archive's, as the release page publishes them; the archive is refused when
# its SHA-256 differs. The token reaches config.sh through its environment
# (ACTIONS_RUNNER_INPUT_TOKEN), never its arguments, and is not written down.
#
# Another runner's folder and LaunchAgent, such as ~/actions-runner and
# com.github.actions.runner, are left as they are. The runner's name derives
# from the Mac's hardware UUID, so it names neither the Mac nor its owner.
set -eu

repo="${1:?usage: lab-runner-install.sh <owner/repo> <version> <sha256>}"
version="${2:?version}"
sha256="${3:?sha256}"
dir="$(cd "$(dirname "$0")" && pwd)"
label=com.github.actions.runner.lab
plist="$HOME/Library/LaunchAgents/$label.plist"
logs="$HOME/Library/Logs/actions-runner-lab"

IFS= read -r token
[ -n "$token" ] || { echo "no registration token on standard input" >&2; exit 1; }
[ -f "$dir/lab-runner-hook.sh" ] || { echo "lab-runner-hook.sh is not beside this script" >&2; exit 1; }
if [ -f "$dir/.runner" ]; then
    echo "$dir already holds a configured runner; remove it with config.sh remove first" >&2
    exit 1
fi

archive="actions-runner-osx-arm64-$version.tar.gz"
cd "$dir"
curl -fsSL -o "$archive" "https://github.com/actions/runner/releases/download/v$version/$archive"
actual="$(shasum -a 256 "$archive" | cut -d' ' -f1)"
if [ "$actual" != "$sha256" ]; then
    echo "$archive: SHA-256 $actual, not the published $sha256" >&2
    rm -f "$archive"
    exit 1
fi
tar -xzf "$archive"
rm -f "$archive"

mkdir -p hooks "$logs"
cp lab-runner-hook.sh hooks/lab-runner-hook.sh
chmod 755 hooks/lab-runner-hook.sh
# The runner prints a hook's path in the job's public log before any step
# can mask it, so the runner is given links outside the home folder, in a
# folder of this account's that survives a restart.
links=/var/tmp/lab-runner-hooks
mkdir -p "$links"
chmod 755 "$links"
[ -O "$links" ] || { echo "$links belongs to another account" >&2; exit 1; }
ln -sf "$dir/hooks/lab-runner-hook.sh" "$links/job-started.sh"
ln -sf "$dir/hooks/lab-runner-hook.sh" "$links/job-completed.sh"

name="lab-$(ioreg -rd1 -c IOPlatformExpertDevice | awk -F'"' '/IOPlatformUUID/ {print $4}' | shasum -a 256 | cut -c1-6)"
path="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

ACTIONS_RUNNER_INPUT_TOKEN="$token" PATH="$path" ./config.sh --unattended \
    --url "https://github.com/$repo" --name "$name" --labels lab --work _work
token=""

# The hooks, and the PATH a job and its hooks start with.
{
    echo "ACTIONS_RUNNER_HOOK_JOB_STARTED=$links/job-started.sh"
    echo "ACTIONS_RUNNER_HOOK_JOB_COMPLETED=$links/job-completed.sh"
} >>.env
printf '%s\n' "$path" >.path

cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$label</string>
	<key>ProgramArguments</key>
	<array>
		<string>$dir/run.sh</string>
	</array>
	<key>WorkingDirectory</key>
	<string>$dir</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>ACTIONS_RUNNER_SVC</key>
		<string>1</string>
		<key>HOME</key>
		<string>$HOME</string>
		<key>PATH</key>
		<string>$path</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>SessionCreate</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>StandardOutPath</key>
	<string>$logs/stdout.log</string>
	<key>StandardErrorPath</key>
	<string>$logs/stderr.log</string>
</dict>
</plist>
EOF
plutil -lint "$plist" >/dev/null
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "installed runner $name in $dir as $label"
