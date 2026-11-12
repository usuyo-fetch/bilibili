// 离线载入油猴脚本：假的 GM_*、unsafeWindow、document，页面的列表请求打到本机的假站点。
// 脚本的 GM_xmlhttpRequest 只放行 127.0.0.1，别的主机直接记成违规。
import { readFileSync } from 'node:fs';
import { createServer, request } from 'node:http';
import { webcrypto } from 'node:crypto';

// 脚本经下面 load() 里的 new Function 跑，栈里它的帧都写着这一段
export const SCRIPT_FRAME = 'eval at load (';
const SOURCE = readFileSync(new URL('../usuyo-bilibili-saved.user.js', import.meta.url), 'utf8');

// ---------- 一点点 DOM ----------

class Element {
  constructor(tag) {
    this.tagName = tag;
    this.children = [];
    this.style = {};
    this.listeners = {};
    this.text = '';
  }
  get textContent() {
    return this.text + this.children.map((c) => c.textContent).join(' ');
  }
  set textContent(v) {
    this.text = String(v);
    this.children = [];
  }
  append(...nodes) {
    this.children.push(...nodes);
  }
  appendChild(node) {
    this.children.push(node);
    return node;
  }
  addEventListener(type, fn) {
    (this.listeners[type] ||= []).push(fn);
  }
  click() {
    for (const fn of this.listeners.click || []) fn({ type: 'click' });
  }
  find(pred) {
    if (pred(this)) return this;
    for (const c of this.children) {
      const hit = c.find(pred);
      if (hit) return hit;
    }
    return null;
  }
  attachShadow() {
    this.shadow = new Element('#shadow-root');
    return this.shadow;
  }
}


// ---------- 假的B站：只答收藏夹、稍后再看的三个列表接口，回答是 fixtures/ 里手写的 ----------

const fixture = (name) => readFileSync(new URL('./fixtures/' + name, import.meta.url), 'utf8');

const ANSWERS = [
  [/^\/x\/v3\/fav\/resource\/list$/, (q) => (q.get('media_id') === '666' ? 'error.json' : q.get('media_id') === '1002' ? 'fav-folder2.json' : q.get('pn') === '2' ? 'fav-folder1-page2.json' : 'fav-folder1-page1.json')],
  [/^\/x\/v2\/history\/toview(\/web)?$/, () => 'toview.json'],
  [/^\/x\/v2\/medialist\/resource\/list$/, () => 'medialist.json'],
];

export async function startSite() {
  const seen = [];
  const server = createServer((req, res) => {
    seen.push(req.url);
    const url = new URL(req.url, 'http://x');
    const hit = ANSWERS.find(([re]) => re.test(url.pathname));
    res.setHeader('content-type', 'application/json');
    if (!hit) {
      res.writeHead(404);
      res.end('{}');
      return;
    }
    res.end(fixture(hit[1](url.searchParams)));
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return { origin: 'http://127.0.0.1:' + server.address().port, seen, close: () => new Promise((r) => server.close(r)) };
}

// ---------- 假的本机监听者（不是 usuyo，或者假装认得令牌） ----------

export async function startListener(answer) {
  const seen = [];
  const server = createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      seen.push({ method: req.method, url: req.url, headers: req.headers, body });
      const [status, json] = answer(req, body);
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(JSON.stringify(json));
    });
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return { port: server.address().port, seen, close: () => new Promise((r) => server.close(r)) };
}

// ---------- 载入脚本 ----------

export function load({ port, path = '/42/favlist', host = 'space.bilibili.com', origin }) {
  const marker = 'const PORT = 47615;';
  if (!SOURCE.includes(marker)) throw new Error('port line not found in the script');
  const source = SOURCE.replace(marker, 'const PORT = ' + port + ';');

  const gm = new Map();
  const gmRequests = [];
  const violations = [];
  const pageStorage = new Map();
  let tick = () => {};
  const menu = {};
  // 浏览器自己的 prompt：测试排好下一次回什么；脚本载入时会拿住这一个
  const promptAnswers = [];
  const prompted = [];

  const page = {
    location: { href: 'https://' + host + path, pathname: path },
    localStorage: {
      getItem: (k) => (pageStorage.has(k) ? pageStorage.get(k) : null),
      setItem: (k, v) => pageStorage.set(k, String(v)),
    },
    prompt: (message) => {
      prompted.push(message);
      return promptAnswers.length ? promptAnswers.shift() : null;
    },
    fetch: (input, init) => fetch(input, init),
    XMLHttpRequest: class {
      open() {}
      send() {}
    },
  };

  const body = new Element('body');
  const document = {
    body,
    createElement: (tag) => new Element(tag),
  };

  function GM_xmlhttpRequest(o) {
    const url = new URL(o.url);
    gmRequests.push({ method: o.method, url: o.url, headers: { ...o.headers }, data: o.data });
    if (url.hostname !== '127.0.0.1') {
      violations.push(o.url);
      queueMicrotask(() => o.onerror && o.onerror({}));
      return;
    }
    // 脚本管理器自己加的 Origin（脚本设不了它）
    const headers = origin ? { ...o.headers, Origin: origin } : o.headers;
    const req = request(
      { host: '127.0.0.1', port: url.port, path: url.pathname + url.search, method: o.method, headers, timeout: o.timeout },
      (res) => {
        let text = '';
        res.setEncoding('utf8');
        res.on('data', (c) => (text += c));
        res.on('end', () => o.onload({ status: res.statusCode, responseText: text }));
      },
    );
    req.on('timeout', () => {
      req.destroy();
      o.ontimeout();
    });
    req.on('error', () => o.onerror({}));
    if (o.data != null) req.write(o.data);
    req.end();
  }

  const names = ['unsafeWindow', 'document', 'GM_getValue', 'GM_setValue', 'GM_xmlhttpRequest', 'GM_registerMenuCommand', 'Element', 'crypto', 'setInterval'];
  const run = new Function(...names, source);
  run(
    page,
    document,
    (k, d) => (gm.has(k) ? structuredClone(gm.get(k)) : d),
    (k, v) => gm.set(k, structuredClone(v)),
    GM_xmlhttpRequest,
    (label, fn) => {
      menu[label] = fn;
    },
    Element,
    webcrypto,
    (fn) => {
      tick = fn;
      return 0;
    },
  );
  tick();

  const panel = () => body.children[0].shadow;
  const button = (label) => panel().find((e) => e.tagName === 'button' && e.text === label);
  return {
    page,
    gm,
    gmRequests,
    violations,
    pageStorage,
    tick,
    panelText: () => panel().textContent,
    note: () => panel().find((e) => e.text !== undefined && /note/.test(e.className || '')).text,
    click: (label) => button(label).click(),
    answerPrompt: (value) => promptAnswers.push(value),
    prompted,
    menu: (label) => menu[label](),
    menuLabels: () => Object.keys(menu),
    hasInput: () => !!body.find((e) => e.tagName === 'input' || (e.shadow && e.shadow.find((x) => x.tagName === 'input'))),
  };
}

/** 等到 `cond()` 为真，最多 `ms` 毫秒。 */
export async function until(cond, ms = 8000) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await new Promise((r) => setTimeout(r, 20));
  }
}
