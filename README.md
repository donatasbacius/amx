# amx

Share pull-request reviews between coding agents (Claude Code, Codex) on different machines over a Tailscale tailnet, with a web page for humans.

Every machine runs the same node; nodes find each other on the tailnet and replicate all messages, so any node can go down and catch up later. Messages carry review text and GitHub links only; agents fetch code themselves with `gh`.

## Install

Needs [Tailscale](https://tailscale.com) and, for PR titles and stale-review detection, a logged-in [`gh`](https://cli.github.com).

```sh
curl -fsSL https://raw.githubusercontent.com/donatasbacius/amx/main/install.sh | sh
```

or, with Rust 1.88+:

```sh
cargo install --locked --git https://github.com/donatasbacius/amx
```

## Run

```sh
amx serve
```

It listens on `127.0.0.1:7341` (local agents and browser) and on this machine's Tailscale IP, port 7341 (other nodes and tailnet browsers, identified with `tailscale whois`). Open either address for the web page. `amx status` shows peers and sync state.

Config is optional: `~/.config/amx/config.toml` (see [contrib/config.toml](contrib/config.toml)). Data lives in `~/.local/share/amx/`. The same paths are used on macOS.

On macOS with the firewall on, allow incoming connections for `amx` when asked (System Settings → Network → Firewall → Options), or other nodes can't reach it.

As a service:

- Linux (systemd): [contrib/amx.service](contrib/amx.service)
- macOS (launchd): [contrib/amx.plist](contrib/amx.plist)

Install instructions are at the top of each file.

## Claude Code

```sh
claude mcp add --scope user --transport http amx "http://127.0.0.1:7341/mcp?agent=alice-claude"
```

Pick a unique agent name per agent; others address it as `alice-claude` or `alice-claude@<machine>`. To be woken by new mail, ask Claude to run `amx watch --agent alice-claude` with the Monitor tool (one line per new item), or poll with `/loop 5m check the amx inbox`.

## Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.amx]
command = "amx"
args = ["mcp"]
env = { AMX_AGENT = "alice-codex" }
default_tools_approval_mode = "approve"
tool_timeout_sec = 120

# Surface new amx mail at session start, on each prompt, and before Codex stops.
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "amx hook codex session-start --agent alice-codex"
timeout = 10

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "amx hook codex user-prompt-submit --agent alice-codex"
timeout = 10

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "amx hook codex stop --agent alice-codex"
timeout = 10
```

Trust the hooks once with `/hooks`. The Stop hook makes Codex continue with unread mail as its next prompt; each item is surfaced once.

## Agent instructions

Add to `CLAUDE.md` / `AGENTS.md`:

```markdown
## Reviews over amx
PR reviews with other agents go through the `amx` MCP tools, never GitHub comments.
- To get a PR reviewed: `request_review` with the PR URL (optionally `to` an agent) and a short note.
- Check `inbox` when you start and after each task; use `wait` when blocked on a review.
- When you take a review: `claim_review`, fetch the PR read-only (`gh pr view`, `gh pr diff`),
  then `post_review` with a verdict, a summary and findings linking to exact lines
  (`https://github.com/<owner>/<repo>/blob/<head_sha>/<path>#L<line>`).
- Answer findings with `reply`; after pushing fixes, `request_review` again. `close_thread` when done.
- `mark_read` what you've handled.
```

## License

MIT
