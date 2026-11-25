# telagent

A Telegram bridge for a local [opencode](https://opencode.ai) server.

It lets you:

- bind a Telegram chat to one local opencode instance,
- receive a message when a session finishes or fails,
- reply to that notification to send feedback back into the session,
- approve or deny opencode permission requests from Telegram,
- answer opencode questions from Telegram.

Both the opencode V1 and V2 HTTP APIs are supported behind an `OpenCodeBackend`
trait. Set `opencode.backend = "v1"` or `"v2"`. The V1 adapter is the tested
default for the currently installed opencode 1.15.10.

## How it works

```text
Telegram
   | long polling
   v
telagent (Rust + teloxide)
   | HTTP + SSE
   v
local opencode server (127.0.0.1:4096)
   |
   +-- redb state: bindings, sessions, pending requests, outbox
```

telagent runs as a user service. It does not open a public port and does not
need port forwarding.

## Requirements

- opencode V1, for example `1.15.10` or newer.
- A local opencode server reachable at `http://127.0.0.1:4096`.
- Home Manager on NixOS if you want the provided module.
- A Telegram bot token from [@BotFather](https://t.me/BotFather).

Home Manager has `programs.opencode.web.enable`, which creates a user service
that runs `opencode serve`. Use that as the persistent backend:

```nix
programs.opencode = {
  enable = true;

  web = {
    enable = true;
    extraArgs = [
      "--hostname" "127.0.0.1"
      "--port" "4096"
    ];
  };
};
```

The option is named `web`, but the service runs `opencode serve`; it does not
open a browser.

To use the TUI with that server:

```bash
opencode attach http://127.0.0.1:4096
```

Stock `opencode` starts its own private server and will not be monitored. Use
`opencode attach`, or add a shell wrapper in your Nix config:

```nix
programs.bash.initExtra = ''
  opencode() {
    case "''${1:-}" in
      completion|acp|mcp|attach|run|debug|providers|agent|upgrade|uninstall|serve|web|models|stats|export|import|github|pr|session|plugin|db)
        command opencode "$@"
        ;;
      *)
        command opencode attach http://127.0.0.1:4096 "$@"
        ;;
    esac
  }
'';
```

## Home Manager

Add telagent as a flake input, then import the module:

```nix
{
  inputs.telagent.url = "github:serephus/telagent";
}
```

Example den aspect or Home Manager module:

```nix
homeManager = { pkgs, osConfig, ... }: {
  imports = [ inputs.telagent.homeManagerModules.default ];

  services.telagent = {
    enable = true;

    opencode.url = "http://127.0.0.1:4096";
    # opencode.passwordFile = osConfig.vaultix.secrets.opencode-server.path;

    telegram.botTokenFile =
      osConfig.vaultix.secrets.telagent-bot.path;
    telegram.allowedUserIds = [ 123456789 ];
    # telegram.proxy = "socks5h://127.0.0.1:7890";

    notify = {
      onIdle = true;
      onError = true;
      permissions = true;
      questions = true;
      includeLastMessage = true;
      childSessions = false;

      paste.enabled = false;
    };
  };
};
```

The module writes `~/.config/telagent/config.toml` and creates
`systemd.user.services.telagent`.

The Telegram token is read from `telegram.botTokenFile` at runtime. It is never
copied into the Nix store.

## Pairing

1. Generate a one-time code:

   ```bash
   telagent pair
   ```

2. Send the printed code to your bot:

   ```text
   /bind ABCD-EFGH
   ```

The code expires after `security.pairingTtlSecs` seconds and can only be used
once.

If `telegram.allowedUserIds` is non-empty, only those Telegram users can bind
and interact with the bot. An empty list relies on the pairing code alone.

## Telegram commands

```text
/bind CODE
/unbind
/new [DIRECTORY]
/compact [ID]
/sessions
/use ID
/cancel
/status
/mute [ID]
/unmute [ID]
/abort [ID]
/help
```

Reply to any notification to send feedback to that session. If no reply target
is set, the active session from `/use` is used.

Commands such as `/compact`, `/mute`, and `/abort` also use the replied
notification's session when sent as a reply. `/cancel` clears the active
session selection; `/status` shows it with a `*` and reports pending requests.

`/new` creates a session and makes it active. Missing directories are created.
Without a directory it uses the active session's directory, then
`opencode.directory`, then the server default. Relative paths resolve against
the active or configured directory, and `~/` expands to your home directory.

`/compact` compacts the active session with its current model.

Notifications also include inline buttons:

- `Reply` selects the session for the next message.
- `Mute` suppresses completion notifications for that session.
- `Sessions` lists recent sessions.

Completion notifications include the model and context usage when opencode
reports it.

## Permissions and questions

`permission.asked` becomes a Telegram message with:

- Allow once,
- Always,
- Deny.

`question.asked` becomes a Telegram message with option buttons when there is a
single question. For custom or multi-question requests, reply to the message
with text. For multiple questions, send one answer per line.

## Configuration

See `examples/config.toml`. The service uses
`~/.local/state/telagent/state.redb` unless `storage.path` is set.

Telegram connectivity can use a proxy:

```toml
[telegram]
proxy = "socks5h://127.0.0.1:7890"
```

or export `TELEGRAM_PROXY` in the service environment.

Notable settings:

```toml
[notify]
include_last_message = true
max_telegram_chars = 3900
child_sessions = false

[notify.paste]
enabled = false
provider = "none" # none | paste-rs | zero-zero | custom
url = ""
threshold_chars = 12000
```

Full messages are sent when they fit. Longer messages are split into numbered
Telegram chunks. If paste upload is enabled and the message exceeds
`threshold_chars`, telagent uploads it and sends the URL instead. Paste is
opt-in because it sends code and possibly secrets to a third party.

## Security

- The opencode server should bind to `127.0.0.1`.
- If it listens beyond loopback, set `OPENCODE_SERVER_PASSWORD` and configure
  `opencode.passwordFile`.
- Pairing codes are hashed and expire.
- Only bound chats can send feedback or approve permissions.
- Permission approval from Telegram is security-sensitive; restrict
  `telegram.allowedUserIds`.
- Do not enable paste upload if the agent may print secrets.

## Development

```bash
cargo fmt
cargo clippy
cargo test
nix build 'path:.#default' --no-link
```

`Cargo.lock` must be tracked for a normal flake build (`nix build .#default`).
The `path:.` form is useful while the tree is still dirty.

## V2

Set:

```toml
[opencode]
backend = "v2"
url = "http://127.0.0.1:4096"
# password_file = "/run/secrets/opencode-server-password"
```

The V2 adapter uses `/api/event`, `/api/session`, `/api/session/active`,
`/api/session/{id}/message`, `/api/session/{id}/prompt`,
`/api/session/{id}/interrupt`, `/api/session/{id}/compact`, permission replies,
and question replies. Questions are mapped onto the Telegram question flow.

Automatic V2 service discovery is available:

```toml
[opencode]
backend = "v2"
auto_discover = true
# service_file = "/home/me/.local/state/opencode/service.json"
```

Discovery reads `url` or `host`/`hostname` + `port`, and supports `headers`,
`token`, or `password` auth fields. An explicit `opencode.url` is still used if
discovery fails.
