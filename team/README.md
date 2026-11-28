# concord-team

**See what your teammates' AIs are changing, while they change it.**

Everyone keeps their own AI tool (Claude Code, Cursor, OpenCode, ...) and their own clone of the repo. `concord-team` shares each person's work in progress with the rest of the team, directly between laptops over [Tailscale](https://tailscale.com). It runs [Concord](../README.md)'s analysis on each laptop to catch the clashes git only shows at merge time:

- **Breaking changes:** a teammate changes a function your new code calls (with your exact call sites).
- **Same lines:** you both edit the same lines.
- **Duplicates:** you both add the same helper.
- **`main` moved:** a merged PR breaks your work in progress.

Each clash comes with a **verdict**: who adapts, and why. Your AI sees it in its own chat, and you see it in the terminal.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/lucas-rus/concord/teams/team/install.sh | sh
```

or with Homebrew (newer Homebrew asks you to trust a third-party tap once):

```bash
brew trust lucas-rus/tap && brew install lucas-rus/tap/concord-team
```

Then install [Tailscale](https://tailscale.com/download) (`brew install --cask tailscale`), open it and log in.

Everyone on the team must be in the same tailnet: invite each other in the Tailscale admin console, or share your machine.

## Start a team (one person)

In your clone of the project:

```bash
concord-team init
```

This prints an invite (`ct1_...`). Send it to your teammates privately: it is the team's key.

## Join (everyone else)

In your clone of the same project:

```bash
concord-team join ct1_...
```

## Every day

```bash
concord-team setup claude   # once per clone, if you use Claude Code
concord-team up             # keep this running in a terminal tab
```

`up` shows who is working on what, every open clash with its verdict, and a live feed. Your teammates' laptops find yours automatically.

| Command | What it does |
|---|---|
| `concord-team status` | one-shot overview |
| `concord-team say ana "taking parse_link for 10 min"` | message a teammate (or `team`); their AI sees it too |
| `concord-team urgent on` | you win clashes until `urgent off` (everyone is told) |
| `concord-team doctor` | checks git, concord, Tailscale, teammates, Claude Code |
| `concord-team invite` | prints the invite again |

With `setup claude`, Claude Code gets:

- a team brief when a session starts;
- clashes and messages in its context as they happen (after its next tool call, or when you type);
- a status line showing who is online, what they are working on, and open clashes.

## Who fixes a clash

Every laptop computes the same verdict from the same shared data, so nobody has to argue:

1. **`main` always wins.** Merged work is final, so you adapt to it.
2. **Urgent beats normal.** Anyone can mark their work urgent with `concord-team urgent on`. Everyone sees it, so it is not abused quietly.
3. **Otherwise, first come, first served.** The later change adapts. Whoever was first could not have known about the later change, while the later one was told the moment it clashed (Concord timestamps when each function and file first entered someone's work).
4. A tie is decided by name, so both sides always agree.

The verdict also says *how*. For example, "give `prefix` a default so old calls still work" is often the cheapest fix for a breaking change. If a verdict changes (someone turns `urgent` on, or a fix lands), both sides are told.

## Privacy

Work in progress goes only to your teammates' laptops, over Tailscale's encrypted connections. Every request needs the team key. No server is involved unless you add the optional mailbox (`team/mailbox`, a small Vercel app for teammates who are never online at the same time).

## Always-online relay and Discord bot

Laptops sync directly over Tailscale while both are online. A relay keeps the team's latest work and messages for whoever is offline, and runs a Discord bot. It is the same program in server mode, with a `Dockerfile` here:

```bash
railway init --name concord-team && railway add -s relay
railway variable set RELAY_KEY --stdin -s relay <<< "$(openssl rand -base64 32)"
railway volume add -m /data && railway up -s relay -d && railway domain -s relay
concord-team relay https://<your-domain> <RELAY_KEY> --default   # new invites include it
```

Any Docker host works the same way (`concord-team serve` with `RELAY_KEY` set and a `/data` volume).

**Discord bot.** Create an application in the [Discord developer portal](https://discord.com/developers/applications) and set `DISCORD_APP_ID`, `DISCORD_PUBLIC_KEY` and `DISCORD_BOT_TOKEN` on the relay (and `DISCORD_GUILD_ID` for instant command updates). Set the app's *Interactions Endpoint URL* to `https://<your-domain>/discord/interactions`, then invite the bot to your server. Commands:

| Command | What it does |
|---|---|
| `/team here` | post this team's clashes in this channel, each with **I'm on it** / **Ask them to fix it** buttons |
| `/team status` | who is working on what (online or not), and open clashes |
| `/team clashes` | open clashes and who fixes each |
| `/team say <name> <message>` | message a teammate; their AI sees it too |
| `/team iam <name>` | link your Discord account to your team name |

## Optional extras

- **Discord webhook (no bot):** `concord-team init --discord <webhook-url>` posts clashes to a channel.
- **Vercel mailbox:** a serverless alternative to the relay in `team/mailbox` (messages and snapshots only, no bot).
- **Windows:** use WSL2 (Ubuntu), install Homebrew for Linux and Tailscale inside it, then follow the steps above.
