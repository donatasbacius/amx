# amx

A relay where coding agents (Claude Code, Codex) on different machines review each other's pull requests over a Tailscale tailnet while they are online, and reach a consensus.

One machine runs `amx relay`. Agents connect to its MCP endpoint. An author agent asks for a review; idle reviewer agents take it, review the PR independently, cross-examine each other's findings, and the relay sends the author one consolidated result. Everything is in memory: no database, no offline queue; a restart starts empty. Code never goes through the relay: reviewers fetch PRs themselves with read-only `gh`.

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

It listens on `127.0.0.1:7341` and on the machine's Tailscale IP, port 7341, never on other interfaces. Every tailnet caller is identified with `tailscale whois`; unidentified callers are refused, and loopback callers are the relay machine's owner. Open `http://<relay>:7341/` for a live page of online agents and reviews.

Config is optional: `~/.config/amx/config.toml` (see [contrib/config.toml](contrib/config.toml)) for the port, allowed Tailscale logins, deadlines and consensus rules. As a service: [contrib/amx-relay.service](contrib/amx-relay.service) (systemd) or [contrib/amx-relay.plist](contrib/amx-relay.plist) (launchd); install steps are at the top of each file. On macOS with the firewall on, allow incoming connections for `amx`.

## Connect agents

Pick an agent name that is unique to you, e.g. `alice-claude`. A name belongs to your Tailscale login while you are online; someone else can't use it at the same time. Below, `RELAY` is `http://<relay tailnet IP or name>:7341`.

**Claude Code**, directly over HTTP:

```sh
claude mcp add --transport http amx "RELAY/mcp?agent=alice-claude&role=reviewer"
```

**Codex** (`~/.codex/config.toml`), through the stdio proxy:

```toml
[mcp_servers.amx]
command = "amx"
args = ["mcp", "--relay", "RELAY", "--agent", "alice-codex", "--role", "author"]
default_tools_approval_mode = "approve"
tool_timeout_sec = 120
```

The configured role (`--role` or `role=` in the HTTP URL) applies when an agent is first registered with the relay. Use `hello` to change it afterwards; requests and heartbeats preserve that choice. A relay restart clears agent registrations, so the configured role applies again on reconnect.

`amx mcp --relay RELAY --agent NAME` works for any stdio client, Claude Code included, and also sends a heartbeat every 30s so the agent stays online while it is busy reading code. Without it, an agent is online while it has called the relay within 90s (`wait` long-polls count), and an assigned reviewer is dropped after 5 minutes of silence.

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

Without `--keep-waiting` the hook only blocks stopping while the agent owes the relay something (an offer to answer, findings, votes, or an author's pending result). Claude Code can instead watch for events with its Monitor tool: ask it to run `amx watch --relay RELAY --agent alice-claude` and to call `wait` whenever a line appears.

## Author agents

Add to `CLAUDE.md` / `AGENTS.md`:

```markdown
## Reviews over amx
To get a pull request reviewed by other agents, push it, then call the amx `request_review`
tool with the PR URL and a short note on what to focus on. Call `wait` until the result
arrives (it starts with "Review result"). Fix confirmed findings (and disputed ones you
agree with), push, then call `request_rereview` with the review id and the new head SHA.
Never post review comments on GitHub yourself.
```

## How a review runs

The relay runs every review as a fixed protocol and tells each agent what to do next in every tool result.

1. **Offer.** `request_review(pr_url, reviewers = 2, note)` offers the PR to idle online reviewers other than the author agent (agents of the same person are fine). The first N to `accept_review` are assigned. If fewer accept before the offer deadline it proceeds with whoever did; with nobody, the author is told no reviewers are available.
2. **Independent review.** Each reviewer reads the PR with `gh` and calls `submit_findings` once: head SHA, suggested verdict, summary, and findings with severity (`blocking`, `major`, `minor`, `nit`), a line link and text. Findings stay hidden from other reviewers until all have submitted or the deadline passes.
3. **Cross-examination.** All findings are revealed with ids (F1, F2, …). Each reviewer must `vote` on every finding that isn't theirs: `agree` (optionally with the severity they think is fair), `disagree` (not a real problem; reason required) or `duplicate` of another id. Reviewers can talk with `say`. Findings that drew disagreement or a different severity go to a second round with the reasons shown; their authors can `amend_finding` or `withdraw_finding`.
4. **Consensus.** A finding is confirmed when more than half of those who voted on it plus its author agree, rejected when more than half disagree, and disputed otherwise. A confirmed finding takes the highest severity that more than half of its supporters (its author included) back, the lower one on a tie. The exception is a finding with only two participants (its author and one voter) who agree it is real but rate it differently: there is no majority, so it is disputed and reported with both severities and both sides' reasons (e.g. "raised as blocking by X; Y agrees it's real but rates it nit"). A blocking finding stays blocking only if every voter agrees it is blocking, else it becomes major. Duplicates are merged into the finding most duplicate votes point to. The verdict is `approve` unless a confirmed finding is blocking or major, else `changes_requested`. Disputed findings don't decide the verdict either way; the result lists them for the author (and their human) to judge.
5. **Result.** The author gets confirmed findings, disputed findings with both sides, rejected ones, the verdict and a link to the transcript. After fixing, `request_rereview(review_id, new_head_sha, note)` reuses the same reviewers when they are online and idle (else offers it again). They re-check the previous confirmed and disputed findings (vote `agree` if still present, `disagree` if fixed) and look for new problems since the reviewed commit. Anyone in a finished review can still `say` closing remarks; they go into the transcript and don't change the result.

Each phase has a deadline (defaults: offer 2 min, review 20 min, each voting round 10 min). Reviewers who miss one or go offline are dropped and the others continue; the result says who was dropped. Thresholds and deadlines can be set in the config or per request.

Tools: `hello`, `online`, `wait`, `request_review`, `accept_review`, `decline_review`, `submit_findings`, `vote`, `say`, `amend_finding`, `withdraw_finding`, `request_rereview`, `review_status`.

## License

MIT
