// Additive v1 metrics are optional when reading an older server publication.
export interface Point { t: string; answered: number; queued: number; miners: number; answering?: number; requests?: number; prompt_tokens?: number }
export interface History { interval_seconds: 60; end: string; answered: number[]; queued: number[]; miners: number[]; answering?: number[]; requests?: number[]; prompt_tokens?: number[] }
export interface Totals { miners: number; queued_questions: number; answered_last_24h: number; answered_last_hour?: number; answered_per_second: number; answering_questions?: number; average_prompt_tokens?: number | null }
export interface Shared { version: 1; epoch: string; seq: number; updated_at: string; totals: Totals }
export interface Snapshot extends Shared { live: Point[]; history: History }
export interface Tick extends Shared { second: Point; minute: Point | null }

type ObjectValue = Record<string, unknown>;
const object = (v: unknown): v is ObjectValue => typeof v === 'object' && v !== null && !Array.isArray(v);
const integer = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const number = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v) && v >= 0;
const utc = (v: unknown): v is string => typeof v === 'string' && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(v)
  && Number.isFinite(Date.parse(v)) && new Date(v).toISOString().replace('.000Z', 'Z') === v;
const totals = (v: unknown): v is Totals => object(v) && integer(v.miners) && integer(v.queued_questions)
  && (v.answered_last_hour === undefined || integer(v.answered_last_hour))
  && integer(v.answered_last_24h) && number(v.answered_per_second)
  && (v.answering_questions === undefined || integer(v.answering_questions))
  && (v.average_prompt_tokens === undefined || v.average_prompt_tokens === null || number(v.average_prompt_tokens));
const shared = (v: unknown): v is Shared => object(v) && v.version === 1 && typeof v.epoch === 'string'
  && /^[\da-f]{8}-[\da-f]{4}-[\da-f]{4}-[\da-f]{4}-[\da-f]{12}$/i.test(v.epoch)
  && integer(v.seq) && utc(v.updated_at) && totals(v.totals);
function promptCounts(v: ObjectValue): boolean {
  return (v.requests === undefined && v.prompt_tokens === undefined)
    || (integer(v.requests) && integer(v.prompt_tokens) && (v.requests > 0 || v.prompt_tokens === 0));
}
const point = (v: unknown): v is Point => object(v) && utc(v.t) && integer(v.answered) && integer(v.queued) && integer(v.miners)
  && (v.answering === undefined || integer(v.answering)) && promptCounts(v);
function history(v: unknown): v is History {
  if (!object(v) || v.interval_seconds !== 60 || !utc(v.end) || Date.parse(v.end) % 60000 !== 0) return false;
  const arrays = [v.answered, v.queued, v.miners];
  if (!arrays.every(a => Array.isArray(a) && a.length <= 1440 && a.every(integer))) return false;
  const length = (v.answered as number[]).length;
  if (!arrays.every(a => (a as number[]).length === length)) return false;
  for (const key of ['answering', 'requests', 'prompt_tokens']) {
    const values = v[key];
    if (values !== undefined && (!Array.isArray(values) || values.length !== length || !values.every(integer))) return false;
  }
  if ((v.requests === undefined) !== (v.prompt_tokens === undefined)) return false;
  return v.requests === undefined || (v.requests as number[]).every((requests, i) => requests > 0 || (v.prompt_tokens as number[])[i] === 0);
}
export function parseSnapshot(v: unknown): Snapshot | null {
  if (!shared(v) || !object(v) || !history(v.history) || !Array.isArray(v.live) || v.live.length > 300 || !v.live.every(point)) return null;
  const points = v.live as Point[];
  if (points.some((p, i) => Date.parse(p.t) >= Date.parse(v.updated_at as string)
    || (i > 0 && Date.parse(p.t) - Date.parse(points[i - 1].t) !== 1000))) return null;
  if (Date.parse(v.history.end) + 60000 > Date.parse(v.updated_at)) return null;
  return v as unknown as Snapshot;
}
export function parseTick(v: unknown): Tick | null {
  if (!shared(v) || !object(v) || !point(v.second) || !(v.minute === null || point(v.minute))) return null;
  if (Date.parse(v.second.t) + 1000 > Date.parse(v.updated_at)) return null;
  if (v.minute !== null && (Date.parse(v.minute.t) % 60000 !== 0 || Date.parse(v.minute.t) + 60000 !== Date.parse(v.second.t) + 1000)) return null;
  return v as unknown as Tick;
}
export function parseRest(v: unknown): Snapshot | null {
  if (!object(v)) return null;
  return parseSnapshot({ version: v.version, epoch: v.epoch, seq: v.seq, updated_at: v.updated_at,
    live: v.live, history: v.history, totals: {
    miners: v.miners, queued_questions: v.queued_questions,
    answered_last_24h: v.answered_last_24h, answered_per_second: v.answered_per_second,
    ...(v.answered_last_hour === undefined ? {} : { answered_last_hour: v.answered_last_hour }),
    ...(v.answering_questions === undefined ? {} : { answering_questions: v.answering_questions }),
    ...(v.average_prompt_tokens === undefined ? {} : { average_prompt_tokens: v.average_prompt_tokens }),
  } });
}
