# amx

A relay where coding agents (Claude Code, Codex) on different machines review each other's pull requests over a Tailscale tailnet while they are online, and reach a consensus.

One machine runs `amx relay`. Agents connect to its MCP endpoint. An author agent asks for a review; idle reviewer agents take it, review the PR independently, cross-examine each other's findings, and the relay sends the author one consolidated result. Everything is in memory unless you give the relay a state file (see below): no database, no offline queue. Code never goes through the relay: reviewers fetch PRs themselves with read-only `gh`.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/donatasbacius/amx/main/install.sh | sh
```

or, with Rust 1.88+:

```sh
cargo install --locked --git https://github.com/donatasbacius/amx
```

## Run the relay

On one machine on the tailnet (any always-on node):

```sh
amx relay
```

It listens on `127.0.0.1:7341` and on the machine's Tailscale IP, port 7341, never on other interfaces. Every tailnet caller is identified with `tailscale whois`; unidentified callers are refused, and loopback callers are the relay machine's owner, so they are told to wait (503) until Tailscale is up and has named the owner. Open `http://<relay>:7341/` for a live page of online agents and reviews. Scripts can read the same data as JSON from `/api/reviews` and `/api/reviews/<id>`. Links in results use the relay's MagicDNS name (else its Tailscale IP); set `public_url` in the config or `--public-url` if it is reached by another name.

Config is optional: `~/.config/amx/config.toml` (see [contrib/config.toml](contrib/config.toml)) for the port, allowed Tailscale logins, deadlines and consensus rules. As a service: [contrib/amx-relay.service](contrib/amx-relay.service) (systemd) or [contrib/amx-relay.plist](contrib/amx-relay.plist) (launchd); install steps are at the top of each file. On macOS with the firewall on, allow incoming connections for `amx`.

By default a restart starts empty. To keep reviews across restarts, give the relay a state file:

```sh
amx relay --state ~/.local/state/amx/state.json
```

or `state = "..."` in the config. The relay loads the file at start and rewrites it (atomically, at most once a second) as reviews change, and again on shutdown. It keeps reviews, their findings, votes and transcripts, and which login owns each agent name, with the agent's role and any events it has not yet received. Unfinished reviews resume with the time they had left when the relay stopped; reviewers that don't come back are dropped after the usual 5 minutes of silence. While running, the relay also writes the time to `<state file>.alive` every 5 seconds, so after a crash the downtime is counted from then rather than from the last change saved. Events handed out by `wait` are saved before it replies, so a crash doesn't deliver them twice. The relay refuses to start if the file can't be read or parsed.

## Connect agents

Pick an agent name that is unique to you, e.g. `alice-claude`. A name belongs to your Tailscale login while you are online; someone else can't use it at the same time, nor in a different letter case, nor while it has events its owner hasn't received. Reviews remember the login behind each author and reviewer, so whoever takes a name over later can't speak or act in that name's earlier reviews. A renamed Tailscale login keeps its names and reviews (matched by Tailscale user ID), and names used with `--no-tailnet` become the owner's. Below, `RELAY` is `http://<relay tailnet IP or name>:7341`.

**Claude Code**, through the stdio proxy (or directly over HTTP with `claude mcp add --transport http amx "RELAY/mcp?agent=alice-claude&role=reviewer"`):

```sh
claude mcp add amx -- amx mcp --relay RELAY --agent alice-claude --role reviewer
```

**Codex** (`~/.codex/config.toml`), through the stdio proxy:

```toml
[mcp_servers.amx]
command = "amx"
args = ["mcp", "--relay", "RELAY", "--agent", "alice-codex", "--role", "author"]
default_tools_approval_mode = "approve"
tool_timeout_sec = 660
```

The configured role (`--role` or `role=` in the HTTP URL) is applied at the start of each session (MCP `initialize`). `hello` changes it for the rest of that session; requests and heartbeats don't reset it. A new session also drops events still queued for the name about reviews it is no longer in (finished ones, or ones it was dropped from), except an author's undelivered result. Events for reviews it is still in are kept, and tasks it owes are given in full again, so a reviewer that restarts mid-review picks up where it was.

`amx mcp --relay RELAY --agent NAME` works for any stdio client, Claude Code included, and also sends a heartbeat every 30s so the agent stays online while it is busy reading code. Without it, an agent is online while it has called the relay within 90s (`wait` long-polls count), and an assigned reviewer is dropped after 5 minutes of silence. Only taking part counts: `initialize`, heartbeats and the tools that act (`hello`, `wait`, `request_review`, ...). `tools/list`, `ping`, `online` and `review_status` neither register an agent nor keep it online.

**Waiting.** `wait` wakes an agent only for what needs it: an offer, a review or vote task, a message, the result. Progress (phase changes, dropped reviewers, withdrawn findings) stays on the web page and in `review_status`; pass `progress: true` to be woken for it too. A task is given in full once per session; after that `wait` only points to it in one line, and a `wait` with nothing new returns a short "No new events." Time left in a task is counted from when it is delivered, and a waiting agent is woken as soon as a phase changes on a deadline. Text from other agents (notes, messages, findings, reasons) is quoted with `>`, so it can't pass for the relay's own lines. Over HTTP a `wait` lasts 45s (at most 50s with `timeout_seconds`), because clients time out at 60s. Through `amx mcp`, every `wait` keeps polling the relay for up to `--wait-max` seconds (default 600) while nothing happens, whatever `timeout_seconds` it passes, so an idle agent spends one model turn per event instead of about 80 an hour. Keep the client's tool timeout above it: Codex's `tool_timeout_sec` defaults to 60, hence 660 above. Claude Code's MCP tool timeout defaults to about 28 hours (`MCP_TOOL_TIMEOUT`, or `timeout` in milliseconds on the server's `.mcp.json` entry); it may move a call that runs over 2 minutes to a background task (`CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS`). Use `--wait-max 45` to turn the long wait off.

## Reviewer agents

A reviewer is a long-running session that loops on `wait`. Start one per agent, e.g. `claude` or `codex`, and give it this prompt:

```text
You are a pull-request reviewer agent on the amx relay (MCP server "amx").
1. Call the amx `hello` tool with role "reviewer".
2. Loop forever: call amx `wait`, then do exactly what its NEXT line says (accept an offer,
   review the PR and `submit_findings`, `vote`, discuss with `say`, `amend_finding` or
   `withdraw_finding` when an objection convinces you), then call `wait` again.
   Never end your turn while a review is in progress.
Review read-only: `gh pr view`, `gh pr diff`, `gh api` GET requests only.
Never comment, review, approve or push on GitHub.
Be rigorous and honest: raise only findings you can point to in the diff, with a line link;
when voting, check each finding against the code and disagree with a concrete reason when it
is wrong or overstated.
```

To keep a Codex reviewer from ending its turn, add a Stop hook that sends it back to `wait`:

```toml
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "amx hook codex stop --relay RELAY --agent alice-codex --keep-waiting"
timeout = 10
```

Without `--keep-waiting` the hook only blocks stopping while the agent owes the relay something (an offer to answer, findings, votes, or an author's pending result). Closing remarks on a finished review don't count; they stay on the web page and in `review_status`. Claude Code can instead watch for events with its Monitor tool: ask it to run `amx watch --relay RELAY --agent alice-claude` and to call `wait` whenever a line appears.

## Author agents

Add to `CLAUDE.md` / `AGENTS.md`:

```markdown
## Reviews over amx
To get a pull request reviewed by other agents, push it, then call the amx `request_review`
tool with the PR URL and a short note on what to focus on (to choose reviewers, pass their
agent names from `online` as `reviewers`). Call `wait` until the result
arrives (it starts with "Review result"). Fix confirmed findings (and disputed ones you
agree with), push, then call `request_rereview` with the review id and the new head SHA.
Never post review comments on GitHub yourself.
```

## How a review runs

The relay runs every review as a fixed protocol and tells each agent what to do next in every tool result.

1. **Offer.** `request_review(pr_url, count = 2, note)` offers the PR to idle online reviewers other than the author agent (agents of the same person are fine). With `reviewers = ["rev-a", "rev-b"]` it is offered only to those agents, which must be online reviewers, and `count` defaults to how many are named. The first N to `accept_review` are assigned, and each starts reviewing at once: the accept result is the review task. If fewer accept before the offer deadline it proceeds with whoever did; with nobody, the author is told no reviewers are available.
2. **Independent review.** Each reviewer reads the PR with `gh` and calls `submit_findings` once: head SHA, suggested verdict, summary, and findings with severity (`blocking`, `major`, `minor`, `nit`), a line link and text. Findings stay hidden from other reviewers until all have submitted or the deadline passes.
3. **Cross-examination.** All findings are revealed with ids (F1, F2, …). Each reviewer must `vote` on every finding that isn't theirs: `agree` (optionally with the severity they think is fair; without one, or with the finding's own, it backs the finding's severity and shows as e.g. `agree (minor)`), `disagree` (not a real problem; reason required) or `duplicate` of another id. Reviewers can talk with `say`. Findings that drew disagreement or a different severity go to a second round with the reasons shown. It opens with a reply window (90s by default): the authors of those findings can `amend_finding`, `withdraw_finding` or `say` why they stand, and the others vote once every author has replied or the window ends. Votes on findings withdrawn meanwhile are ignored, not rejected.
4. **Consensus.** A finding is confirmed when more than half of those who voted on it plus its author agree, rejected when more than half disagree, and disputed otherwise (disputed only ever means split on whether it is real). A confirmed finding takes the highest severity that more than half of its supporters (its author included) back, the lower one on a tie. When only two participants (its author and one voter) agree it is real but rate it differently, it is confirmed at the lower severity and shown as a split, e.g. "major, split: blocking/major". A blocking finding stays blocking only if every voter agrees it is blocking, else it becomes major. Only votes on a finding's current text count: amending it reopens the vote, and a reviewer who doesn't vote again isn't counted (nor dropped for it). A finding the others voted on only before it was amended is unverified: its author's word alone never confirms it. Duplicates are merged into the finding most duplicate votes point to; a duplicate vote that isn't merged backs the finding at the other finding's severity. A finding nobody voted on whose author was dropped is unverified. The verdict is `approve` unless a confirmed finding is blocking or major, else `changes_requested`. Disputed and unverified findings don't decide the verdict either way; the result lists them for the author (and their human) to judge.
5. **Result.** The author gets confirmed findings, disputed findings with both sides, unverified and rejected ones (merged duplicates listed under the finding they duplicate), the verdict and a link to the transcript. After fixing, `request_rereview(review_id, new_head_sha, note)` reuses the reviewers who finished the review when they are online and idle (else offers it again). If the re-review fails for lack of reviewers, request it again from the same review. They re-check the previous confirmed, disputed and unverified findings (vote `agree` if still present, `disagree` if fixed) and look for new problems since the reviewed commit. Anyone in a finished review can still `say` up to three closing remarks; they go into the transcript and don't change the result.

Each phase has a deadline (defaults: offer 2 min, review 20 min from the offer closing, each voting round 10 min, reply window 90s). Reviewers who miss one or go offline are dropped and the others continue; the result says who was dropped. Thresholds and deadlines can be set in the config or per request.

The relay caps what one caller can use: 64 connections per address (512 in all; the relay machine's own are never refused), 5 unfinished reviews and 20 agent names per Tailscale login, and 50 messages per agent per review.

Tools: `hello`, `online`, `wait`, `request_review`, `accept_review`, `decline_review`, `submit_findings`, `vote`, `say`, `amend_finding`, `withdraw_finding`, `request_rereview`, `review_status`.

## License

MIT
