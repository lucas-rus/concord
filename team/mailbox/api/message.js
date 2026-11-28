// POST /api/message?project=   body JSON {from, to, text}; to = a member or "team"
//   Messages to "team" are also forwarded to Discord/Slack when a webhook is set.
// GET  /api/message?project=&user=&since=   -> messages to the user or the team
import { randomBytes } from 'node:crypto';
import { put } from '@vercel/blob';
import { authed, listAll, param, readBody, readJson, stamp } from './_lib.js';

const NAME = /^[a-z0-9][a-z0-9_-]{0,39}$/;

async function forward(text) {
  const jobs = [];
  if (process.env.DISCORD_WEBHOOK_URL)
    jobs.push(fetch(process.env.DISCORD_WEBHOOK_URL, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ content: text.slice(0, 1900), allowed_mentions: { parse: [] } }),
    }));
  if (process.env.SLACK_WEBHOOK_URL)
    jobs.push(fetch(process.env.SLACK_WEBHOOK_URL, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text }),
    }));
  await Promise.allSettled(jobs);
}

export default async function handler(req, res) {
  if (!authed(req, res)) return;
  const project = param(req, res, 'project');
  if (!project) return;
  const root = `teams/${project}/msg`;

  if (req.method === 'POST') {
    let m;
    try {
      m = JSON.parse((await readBody(req)).toString('utf8'));
    } catch {
      return res.status(400).json({ error: 'bad json' });
    }
    const to = String(m.to || '');
    if (!NAME.test(String(m.from || '')) || !(to === 'team' || NAME.test(to)) || !m.text)
      return res.status(400).json({ error: 'from, to and text are required' });
    const now = Date.now();
    const msg = { from: m.from, to, text: String(m.text).slice(0, 4000), kind: String(m.kind || 'note'), at: now };
    await put(`${root}/${to}/${now}-${randomBytes(4).toString('hex')}.json`, JSON.stringify(msg), {
      access: 'private', addRandomSuffix: false, contentType: 'application/json',
    });
    if (to === 'team') await forward(`**${msg.from}**: ${msg.text}`);
    return res.status(200).json({ ok: true, at: now });
  }

  const user = param(req, res, 'user');
  if (!user) return;
  const since = Number(req.query.since || 0);
  const blobs = [...(await listAll(`${root}/${user}/`)), ...(await listAll(`${root}/team/`))].filter(
    (b) => stamp(b.pathname) > since,
  );
  const msgs = (await Promise.all(blobs.map((b) => readJson(b.pathname)))).filter(Boolean);
  msgs.sort((a, b) => a.at - b.at);
  return res.status(200).json({ messages: msgs, now: Date.now() });
}
