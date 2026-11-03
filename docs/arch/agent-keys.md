# Agent keys (AK)

## Why

- Claude Code and Codex run inside durable-machine sessions, for example as the agents under test in an evaluation row.
- Both must reach the in-house LiteLLM gateway, and the owner wants each machine's usage tracked on its own (DC-24).
- The gateway's address is local configuration of each machine. No in-house endpoint appears in this repository, in an image, or as a default in code (DC-32).
- A **stable key per machine per agent** gives that tracking: the gateway attributes every request to the key, and the key belongs to one agent on one Mac.

## AK-1: Key files

- Each Mac holds its own keys in two files that the owner creates, under the instance's configuration folder, `~/.config/machine-service/` by default (a second instance for tests names its own with `MACHINE_SERVICE_CONFIG_DIR`):

| File | Content | Agent |
| --- | --- | --- |
| `~/.config/machine-service/agents/claude-code.env` | `ANTHROPIC_API_KEY=<key>` | Claude Code |
| `~/.config/machine-service/agents/codex.env` | `LITELLM_CODEX_KEY=<key>` | Codex |

- The directory `~/.config/machine-service/agents/` must be mode 700 and each file mode 600, all owned by `wezzard`.
- Each file must contain exactly its one variable assignment. Blank lines and lines starting with `#` are allowed. Any other line makes the file invalid.
- A file that is missing is reported as `absent`. A file or directory with group or other permission bits is reported as `unsafe-mode` and is not loaded. Both are reported in the machine's `agents` capability ([MM-3](machines.md#mm-3-capability-report)) and never refuse an acquisition.
- machine-service never creates, rotates or copies a key. The owner issues the keys in LiteLLM, one per machine per agent, and writes the files on each Mac.
- The files are separate from the virtual-machine credential packs in `~/.config/vm-credentials/`, which keep serving virtual machines unchanged.

## AK-2: Gateway endpoints are local configuration

- The base URLs that Claude Code and Codex use are injected into a machine from local configuration. They are never part of a VM image or of a durable machine's software baseline, and machine-service has no default for them (DC-32).
- On a durable machine the operator writes them once, beside the key files, in `~/.config/machine-service/agents/endpoints.json`:

```json
{
  "claude-code": { "base_url": "http://<gateway-host>" },
  "codex": { "base_url": "http://<gateway-host>/v1" }
}
```

- The file holds no secret. It must be owned by `wezzard` and must not be writable by group or other, because it decides where an agent sends its key.
- Each `base_url` is used exactly as written. The service appends no path.
- An agent is loaded only when both its key file and its endpoint are present and valid. A key without an endpoint is not loaded, because Claude Code or Codex would then send the key to its vendor's public endpoint. The machine's `agents` capability reports the endpoint's state beside the key's ([MM-3](machines.md#mm-3-capability-report)).
- On a virtual machine the endpoints arrive at lease time from host-local configuration, the credential pack's `env.extra` in `~/.config/vm-credentials/<pack>/`, as they do today ([AK-6](#ak-6-virtual-machines)).
- Documents and tests name an endpoint only as a placeholder, such as `http://<gateway-host>`.

## AK-3: Claude Code

- When a durable lease is acquired with `env: "default"` and `claude-code.env` is loaded, the session environment carries:
  - `ANTHROPIC_BASE_URL`, set to the `claude-code` base URL from `endpoints.json`.
  - `ANTHROPIC_API_KEY`, set to the key from `claude-code.env`.
- The session environment never carries `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN`. The service removes both from the captured login environment, together with any other credential variable from the owner's shell ([DM-4](durable-machine.md#dm-4-exec)), so Claude Code authenticates only with the machine's own key.
- The service does not change Claude Code's configuration directory. The owner's settings, plugins and MCP servers in `~/.claude` stay available to a session.

## AK-4: Codex

- When `codex.env` and the `codex` endpoint are loaded, the session environment carries `LITELLM_CODEX_KEY` and `CODEX_HOME`, which points at the workspace's `codex/` folder.
- At acquisition the service writes `codex/config.toml`, which selects a custom model provider at the configured gateway:

```toml
model_provider = "machine-service-litellm"

[model_providers.machine-service-litellm]
name = "LiteLLM (machine-service)"
base_url = "http://<gateway-host>/v1"
env_key = "LITELLM_CODEX_KEY"
wire_api = "responses"
```

- `base_url` is the `codex` base URL from `endpoints.json`, as written. The file names the key's variable through `env_key` and never contains the key.
- The file does not pin a model. The caller names the model on Codex's command line, as the evaluation runner already does for its pinned models.
- A per-session `CODEX_HOME` keeps the owner's `~/.codex/config.toml` untouched, because no service may write into a user's own agent files. It also keeps Codex's session logs inside the workspace.
- A session therefore does not see the owner's Codex plugins, skills or MCP servers from `~/.codex`. A caller that needs them stages them into the session's `CODEX_HOME`.

## AK-5: Never printed, logged or returned

- The service reads each key file at acquisition and holds the value in memory only. It passes the value to a command only through that command's environment.
- The lease record, the service log, every response and every error carry the key's state (`loaded`, `absent`, `unsafe-mode` or `invalid`) and the file's path, never its value.
- The service writes no key into the workspace, the hygiene record or any file it creates.
- Before returning exec output, the service replaces every occurrence of a loaded key's value with `[redacted]`. A session's own command may still write its environment into a file, which the service cannot prevent. This replacement keeps a key out of the responses that clients log.
- A command through the relay agent, whether machine-service's own exec or a relay client's ([DM-4](durable-machine.md#dm-4-exec)), has its output redacted by the launcher before the relay agent sees it, so no relay message, no relay agent record and no client of the broker receives a key (DC-221). Release still purges the lease's records ([RP-14](relay-protocol.md#rp-14-records-belong-to-their-lease), DC-173), which keep the commands' output.

## AK-6: Virtual machines

- Virtual machines keep the credential packs of [US3](../user-stories/agent.md#us3--credentials-arrive-automatically). A pack is host-local configuration: its `env.extra` carries the gateway endpoints and keys, and the service injects it into the clone at lease time.
- Images stay free of endpoints and keys, as pilot-images requires: an image is built without any gateway, and a pack is never written into a base.
- Agent key files and `endpoints.json` apply only to durable machines.
