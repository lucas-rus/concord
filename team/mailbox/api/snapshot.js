// POST /api/snapshot?project=&user=   body: git bundle, header x-concord-meta: base64 JSON
// GET  /api/snapshot?project=&user=&sha=   -> the bundle
import { del, get, put } from '@vercel/blob';
import { authed, listAll, param, readBody, stamp } from './_lib.js';

const SHA = /^[0-9a-f]{40}$/;

export default async function handler(req, res) {
  if (!authed(req, res)) return;
  const project = param(req, res, 'project');
  if (!project) return;
  const user = param(req, res, 'user');
  if (!user) return;
  const root = `teams/${project}`;

  if (req.method === 'GET') {
    const sha = String(req.query.sha || '');
    if (!SHA.test(sha)) return res.status(400).json({ error: 'bad sha' });
    const r = await get(`${root}/bundles/${user}/${sha}.bundle`, { access: 'private', useCache: false });
    if (!r || r.statusCode !== 200) return res.status(404).json({ error: 'no such snapshot' });
    res.setHeader('content-type', 'application/octet-stream');
    return res.status(200).send(Buffer.from(await new Response(r.stream).arrayBuffer()));
  }
  if (req.method !== 'POST') return res.status(405).json({ error: 'method' });

  let meta;
  try {
    meta = JSON.parse(Buffer.from(String(req.headers['x-concord-meta'] || ''), 'base64').toString('utf8'));
  } catch {
    return res.status(400).json({ error: 'bad meta' });
  }
  if (!SHA.test(meta.sha || '') || !SHA.test(meta.base || '')) return res.status(400).json({ error: 'bad sha/base' });
  const bundle = await readBody(req);
  if (bundle.length === 0) return res.status(400).json({ error: 'empty bundle' });

  const now = Date.now();
  await put(`${root}/bundles/${user}/${meta.sha}.bundle`, bundle, {
    access: 'private', addRandomSuffix: false, allowOverwrite: true, contentType: 'application/octet-stream',
  });
  const record = {
    user,
    sha: meta.sha,
    base: meta.base,
    branch: String(meta.branch || '').slice(0, 200),
    files: (Array.isArray(meta.files) ? meta.files : []).slice(0, 200).map(String),
    tool: String(meta.tool || '').slice(0, 40),
    at: now,
  };
  await put(`${root}/meta/${user}/${now}.json`, JSON.stringify(record), {
    access: 'private', addRandomSuffix: false, contentType: 'application/json',
  });

  // Keep only this member's latest snapshot.
  const old = [
    ...(await listAll(`${root}/meta/${user}/`)).filter((b) => stamp(b.pathname) < now),
    ...(await listAll(`${root}/bundles/${user}/`)).filter((b) => !b.pathname.endsWith(`/${meta.sha}.bundle`)),
  ];
  if (old.length) await del(old.map((b) => b.url));
  return res.status(200).json({ ok: true, at: now });
}
