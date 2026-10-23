// The WinterTC Minimum Common API, checked from inside the runtime.
// GET /probe: which of the 59 globals exist. GET /checks: each API used for real.
const WINTERTC = [
  'AbortController', 'AbortSignal', 'Blob', 'ByteLengthQueuingStrategy', 'CompressionStream', 'CountQueuingStrategy',
  'Crypto', 'CryptoKey', 'CustomEvent', 'DecompressionStream', 'DOMException', 'ErrorEvent', 'Event', 'EventTarget',
  'File', 'FormData', 'Headers', 'MessageChannel', 'MessageEvent', 'MessagePort', 'Performance', 'PromiseRejectionEvent',
  'ReadableByteStreamController', 'ReadableStream', 'ReadableStreamBYOBReader', 'ReadableStreamBYOBRequest',
  'ReadableStreamDefaultController', 'ReadableStreamDefaultReader', 'Request', 'Response', 'SubtleCrypto',
  'TextDecoder', 'TextDecoderStream', 'TextEncoder', 'TextEncoderStream', 'TransformStream',
  'TransformStreamDefaultController', 'URL', 'URLPattern', 'URLSearchParams', 'WritableStream',
  'WritableStreamDefaultController', 'WritableStreamDefaultWriter', 'WebAssembly',
  'atob', 'btoa', 'clearInterval', 'clearTimeout', 'console', 'crypto', 'fetch', 'navigator', 'performance',
  'queueMicrotask', 'reportError', 'self', 'setInterval', 'setTimeout', 'structuredClone',
]

const checks = {
  async simienGlobal() { return typeof Simien === 'object' && /^\d+\.\d+\.\d+/.test(Simien.version) && Object.isFrozen(Simien) && typeof Deno === 'undefined'; },
  async setTimeout() { const t0 = performance.now(); await new Promise((r) => setTimeout(r, 20)); return performance.now() - t0 >= 15; },
  async structuredClone() { const v = structuredClone({ a: [1, { b: new Date(0) }] }); return v.a[1].b instanceof Date && v.a[0] === 1; },
  async structuredCloneTransfer() { const b = new ArrayBuffer(8); const c = structuredClone(b, { transfer: [b] }); return b.detached === true && b.byteLength === 0 && c.byteLength === 8; },
  async base64() { return btoa('simien') === 'c2ltaWVu' && atob('c2ltaWVu') === 'simien'; },
  async textCoding() { return new TextDecoder().decode(new TextEncoder().encode('héllo ✓')) === 'héllo ✓'; },
  async url() { const u = new URL('/a?b=1#c', 'https://simien.dev'); return u.href === 'https://simien.dev/a?b=1#c' && new URLSearchParams(u.search).get('b') === '1'; },
  async urlPattern() { return new URLPattern({ pathname: '/users/:id' }).exec('https://x.dev/users/42').pathname.groups.id === '42'; },
  async streams() {
    const rs = new ReadableStream({ start(c) { c.enqueue('a'); c.enqueue('b'); c.close(); } });
    const out = []; for await (const chunk of rs.pipeThrough(new TransformStream({ transform(ch, c) { c.enqueue(ch.toUpperCase()); } }))) out.push(chunk);
    return out.join('') === 'AB';
  },
  async compression() {
    const gz = new Blob(['simien '.repeat(100)]).stream().pipeThrough(new CompressionStream('gzip'));
    const back = await new Response(gz.pipeThrough(new DecompressionStream('gzip'))).text();
    return back === 'simien '.repeat(100);
  },
  async crypto() {
    const id = crypto.randomUUID();
    const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode('abc')));
    return /^[0-9a-f-]{36}$/.test(id) && digest[0] === 0xba && digest[31] === 0xad;
  },
  async abort() { const c = new AbortController(); c.abort(new Error('x')); return c.signal.aborted && AbortSignal.timeout(10) instanceof AbortSignal; },
  async events() { const t = new EventTarget(); let got = null; t.addEventListener('hi', (e) => (got = e.detail)); t.dispatchEvent(new CustomEvent('hi', { detail: 7 })); return got === 7; },
  async messageChannel() { const { port1, port2 } = new MessageChannel(); const p = new Promise((r) => (port2.onmessage = (e) => r(e.data))); port1.postMessage({ n: 1 }); const v = await p; port1.close(); port2.close(); return v.n === 1; },
  async responseJson() { const r = Response.json({ ok: true }, { status: 201 }); return r.status === 201 && (await r.json()).ok === true && r.headers.get('content-type').startsWith('application/json'); },
  async formData() { const f = new FormData(); f.append('a', '1'); return (await new Request('https://x.dev', { method: 'POST', body: f }).formData()).get('a') === '1'; },
  async fetchDenied() {
    try { await fetch('https://example.com/'); return false; }
    catch (e) { return /net|allow|NotCapable|denied|permission/i.test(String(e?.name) + String(e?.message)); }
  },
}

export default {
  async fetch(request) {
    const url = new URL(request.url)
    if (url.pathname === '/probe') {
      const missing = WINTERTC.filter((name) => typeof globalThis[name] === 'undefined')
      return Response.json({ total: WINTERTC.length, missing })
    }
    const results = {}
    for (const [name, check] of Object.entries(checks)) {
      try { results[name] = (await check()) ? 'ok' : 'WRONG' } catch (e) { results[name] = 'THREW ' + (e?.name ?? '') + ': ' + (e?.message ?? e) }
    }
    return Response.json(results)
  },
}
