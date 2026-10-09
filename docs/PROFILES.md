# Launch profiles

A launch profile is a named way to start an agent that OmniSession already supports. It can add arguments, set or remove environment variables, or run another program in place of the agent's own command.

Use a profile to point an agent at a local gateway, to start it through a wrapper, or to keep two setups of one agent apart. A profile does what a shell alias or a small launcher script does. Instead of a script, the setup is one entry in a file, and the entry appears in `omni` and `omni fork` next to the built-in agents.

A profile changes how an agent starts. It does not change where sessions come from. A profile for Claude Code starts Claude Code, reads Claude Code sessions, imports like Claude Code, and forks like Claude Code.

The profile file lives on your machine, in the OmniSession state directory. It is not part of any repository, so settings such as the address of a gateway or a token stay local.

## Add a profile

1. Create `~/.omnisession/profiles.toml`. If you set `OMNISESSION_HOME`, the file is `$OMNISESSION_HOME/profiles.toml`.
2. Run `chmod 600 ~/.omnisession/profiles.toml`. OmniSession refuses a file that other users can write, because a profile chooses the program that `omni` runs.
3. Add one table for each profile, as in the examples below.
4. Run `omni profiles`. It lists every profile and says whether each one can start.
5. Run `omni` and pick a session. The profile is a row on the target page. You can also run `omni resume <session> --in <profile>`.

## Example: Claude Code through a local gateway

```toml
[profiles.claude-gateway]
label = "Claude Code (local gateway)"
agent = "claude"
args = ["--settings", "${HOME}/.local/share/gateway/claude-settings.json"]
unset = ["ANTHROPIC_API_KEY"]

[profiles.claude-gateway.env]
ANTHROPIC_BASE_URL = "${GATEWAY_URL:-http://127.0.0.1:11435}"
ANTHROPIC_AUTH_TOKEN = "${GATEWAY_TOKEN:-local}"
ANTHROPIC_MODEL = "${GATEWAY_MODEL:-gateway/default}"
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1"
```

The profile starts `claude` with the `--settings` argument. It removes `ANTHROPIC_API_KEY` from the environment and sets the other four variables. Each `${...}` takes its value from the environment that you run `omni` in, so you can change the gateway address for one run without editing the file.

## Example: Codex through a local gateway

```toml
[profiles.codex-gateway]
label = "Codex (local gateway)"
agent = "codex"
args = [
  "--profile", "gateway",
  "-c", "model_providers.gateway.base_url=\"${GATEWAY_URL:-http://127.0.0.1:11435}/v1\"",
]
```

Codex reads the `gateway` profile from its own configuration. The `-c` argument sets the address for this launch only.

## Example: a wrapper program

```toml
[profiles.codex-wrapped]
agent = "codex"
program = "${HOME}/bin/codex-wrapper"
args = ["--record"]
```

The profile runs `codex-wrapper` in place of `codex`. The wrapper receives `--record`, then the arguments that `omni` builds for the agent, such as `resume <session>`. The agent itself does not have to be installed. The wrapper starts it.

## The fields of a profile

| Field | Required | Meaning |
| --- | --- | --- |
| `agent` | Yes | The built-in agent that the profile starts: `claude`, `codex`, `opencode`, `grok`, `hermes`, `antigravity`, `pi`, `omp`, or `cursor-cli`. Any name that `--in` accepts for these agents works too. |
| `label` | No | The name that the target page shows. The default is the profile name. |
| `program` | No | A program to run in place of the agent's own command. Give a path, or a command name that `PATH` can find. `omni` finds the file once, with its own `PATH`, and runs that file. A `PATH` that the profile sets in `env` does not change which file runs. A command that names an agent, such as `claude`, resolves as `omni` resolves that agent: the `OMNI_CLAUDE_BIN` override applies, and the provider shim is skipped. |
| `args` | No | Arguments that go before the arguments that `omni` builds for the agent. |
| `env` | No | A table of variables to set. |
| `unset` | No | A list of variables to remove. A variable cannot be in both `env` and `unset`. |

The name of a profile is the part after `profiles.`. It uses lowercase letters, digits, `.`, `_`, and `-`, starts with a letter or digit, and has at most 64 characters. It cannot be the name or alias of a built-in agent, such as `claude` or `agy`. A label has at most 64 characters and no control characters. A variable name has at most 128 characters. A file holds at most 64 profiles and at most 256 KiB.

## Values that use the environment

A value in `program`, `args`, or `env` can read the environment that `omni` runs in:

| Text | Result |
| --- | --- |
| `${NAME}` | The value of `NAME`. If `NAME` is not set, the profile cannot start, and the error says which variable is missing. |
| `${NAME:-default}` | The value of `NAME`. If `NAME` is not set or is empty, `default`. A default ends at the first `}`. |
| `$$` | A literal `$`. |

Any other `$` stays as written.

Put secrets in your shell environment and read them with `${NAME}`. Do not write a credential into the file. The file is plain text, and `omni` cannot hide a credential that you write into it.

OmniSession hides a value that a launch read from a variable whose name suggests a credential. The name contains `key`, `token`, `secret`, `pass`, `pwd`, `auth`, `credential`, `cookie`, or `bearer`. The value is hidden wherever it appears in the launch command, including inside an argument such as `"--api-key", "${GATEWAY_TOKEN}"`. A value shorter than 6 characters is not hidden, because it would also match other text. A variable with another name, such as `${BUILD_ID}`, is shown. A default that the profile gives is not a value that was read, so it is shown.

## How a launch is built

For one launch, the command has these parts, in this order:

1. The program: the agent's own command, or the `program` of the profile.
2. The permission mode flags, when you choose a mode with `--mode` or on the target page.
3. The `args` of the profile.
4. The arguments that `omni` builds for the agent, such as `--resume <session>` or `--fork-session`.

Before the program starts, `omni` removes the variables in `unset`, then sets the variables in `env`.

`omni resume --dry-run` and `--json` show the result. The command appears as arguments of `env`. A value that looks like a credential, or that came from a credential-like variable (see above), is replaced by a placeholder. The program receives the real value.

```sh
omni resume <session> --in claude-gateway --dry-run
```

## What a profile applies to

- `omni resume --in <profile>`, `omni fork --in <profile>`, and `omni switch <profile>`.
- The rows of the target page and the `NEW SESSION` row in `omni` and `omni fork`.

A profile does not apply to commands that a [provider shim](../README.md#provider-shims) routes, such as `claude --continue`. Those start the agent itself.

A cross-agent import still uses the installed agent to write the new session. The profile applies when the agent starts for you.

## Permission modes

The [permission mode](../README.md#permission-modes) flags come from the agent that the profile starts. OmniSession adds a mode flag only when the program that runs lists that flag in `--help`. When a profile sets `program`, that program is the one that is asked. An older program steps down to the nearest mode it has, and the warning names the profile.

## Check a profile

`omni profiles` prints the file, then each profile with a summary and its state:

```text
claude-gateway  Claude Code (local gateway)
    agent claude; 1 argument(s); sets ANTHROPIC_AUTH_TOKEN, ANTHROPIC_BASE_URL, ANTHROPIC_MODEL, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC; unsets ANTHROPIC_API_KEY
    ready
codex-wrapped  codex-wrapped
    agent codex; program ${HOME}/bin/codex-wrapper; 1 argument(s)
    not ready: the program `${HOME}/bin/codex-wrapper` was not found or is not executable
```

A profile whose agent is not installed, or whose program is missing, does not appear on the target page. A profile that uses `${NAME}` for a variable that is not set does appear, also when the variable is in `program`. Picking it stops the run with the name of the missing variable, and `omni profiles` gives the same reason. The summary lists variable names and never their values. `omni --json profiles` prints the same information as JSON.

## Limits and safety

- A profile cannot set or remove a variable that starts with `OMNI_`. Those variables steer OmniSession.
- A profile cannot set or remove a variable that decides where sessions are: `HOME`, `OMNISESSION_HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`, `HERMES_HOME`, `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR`, `PI_CONFIG_DIR`, `PI_PROFILE`, `OMP_SESSION_DIR`, `OMP_PROFILE`, `CURSOR_AGENT_HOME`, `CURSOR_CONFIG_DIR`, `CURSOR_IDE_HOME`, `ANTIGRAVITY_CLI_HOME`, `ANTIGRAVITY_IDE_HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `APPDATA`, and `LOCALAPPDATA`. `omni` reads and imports sessions with its own environment. A changed variable would send the agent to a different store than the one `omni` wrote to. Other variables, such as `PATH` and `ANTHROPIC_BASE_URL`, are free.
- A profile can run any program that you can run, with the permissions that you have. Treat the profile file like a shell startup file. On Linux and macOS, OmniSession refuses a file that group or other users can write, a file that another user owns, and a file in a directory that other users can write. A directory with the sticky bit, such as `/tmp`, is accepted. For a symbolic link, the directory of the link and the directory of its target both count.
- A file that does not parse, or a profile that fails a check, never stops the built-in agents. `omni` prints a warning, and the built-in agents keep working. A profile name that cannot be read shows the same error when you pass it to `--in`. For a syntax error, the message gives the line and the column and never quotes the line, because the line may hold a credential.
- If a value uses `${NAME}` and `NAME` is not set, or the `program` of the profile does not exist or is not executable, the run stops before it imports or writes anything. Nothing needs a rollback.
- Windows reads the same file. Permission checks on the file apply on Linux and macOS only.
