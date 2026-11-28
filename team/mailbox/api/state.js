// GET /api/state?project=   -> every member's latest snapshot metadata
import { authed, listAll, param, readJson, stamp } from './_lib.js';

export default async function handler(req, res) {
  if (!authed(req, res)) return;
  const project = param(req, res, 'project');
  if (!project) return;
  const latest = new Map();
  for (const b of await listAll(`teams/${project}/meta/`)) {
    const user = b.pathname.split('/')[3];
    if (!latest.has(user) || stamp(b.pathname) > stamp(latest.get(user).pathname)) latest.set(user, b);
  }
  const users = {};
  await Promise.all(
    [...latest].map(async ([user, b]) => {
      const m = await readJson(b.pathname);
      if (m) users[user] = m;
    }),
  );
  return res.status(200).json({ users, now: Date.now() });
}
