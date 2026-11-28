// Shared helpers of the Concord Teams mailbox (prototype).
//
// The mailbox only stores and forwards: each member's latest work-in-progress
// snapshot (a git bundle plus a little metadata) and messages. It never runs
// git or analyses code; every laptop runs Concord's check itself.
import { timingSafeEqual } from 'node:crypto';
import { get, list } from '@vercel/blob';

const NAME = /^[a-z0-9][a-z0-9_-]{0,39}$/;

/** Constant-time check of the team token (`Authorization: Bearer <token>`). */
export function authed(req, res) {
  const want = Buffer.from(process.env.TEAM_TOKEN || '');
  const got = Buffer.from((req.headers.authorization || '').replace(/^Bearer\s+/i, ''));
  if (want.length === 0 || want.length !== got.length || !timingSafeEqual(want, got)) {
    res.status(401).json({ error: 'unauthorized' });
    return false;
  }
  return true;
}

/** A project or user name from the query, or null after answering 400. */
export function param(req, res, key) {
  const v = String(req.query[key] || '');
  if (!NAME.test(v)) {
    res.status(400).json({ error: `bad or missing ${key}` });
    return null;
  }
  return v;
}

export async function readBody(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  return Buffer.concat(chunks);
}

export async function listAll(prefix) {
  const out = [];
  let cursor;
  do {
    const r = await list({ prefix, cursor, limit: 1000 });
    out.push(...r.blobs);
    cursor = r.hasMore ? r.cursor : undefined;
  } while (cursor);
  return out;
}

export async function readText(pathname) {
  const r = await get(pathname, { access: 'private', useCache: false });
  if (!r || r.statusCode !== 200) return null;
  return await new Response(r.stream).text();
}

export async function readJson(pathname) {
  const t = await readText(pathname);
  return t === null ? null : JSON.parse(t);
}

/** Leading millisecond timestamp of a pathname's last segment. */
export function stamp(pathname) {
  return Number((pathname.split('/').pop() || '').split(/[-.]/)[0]) || 0;
}
