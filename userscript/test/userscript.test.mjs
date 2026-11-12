// node --test userscript/test/
// 交给 usuyo 的测试要一个一次性的 usuyo 守护进程：USUYO_BIN 指向已经编好的 usuyo，
// 端口 USUYO_TEST_PORT（默认 47703，不用 47615），家目录是新建的临时目录，测完 SIGTERM 并送进废纸篓。
// 回答的形状在 fixtures/：收藏夹那三份是照真实回答的字段手写的（内容是编的）；
// toview.json、medialist.json 没有量过（取不到账号的回答），照接口笔记写。
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, existsSync } from 'node:fs';
import { createHmac, randomBytes } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { load, startSite, startListener, until, SCRIPT_FRAME } from './harness.mjs';

const BIN = process.env.USUYO_BIN;
const PORT = Number(process.env.USUYO_TEST_PORT || 47703);
const FOLDER1 = ['BV1Aa0000001', 'BV1Aa0000002', 'BV1Aa0000003', 'BV1Aa0000005'];
const FOLDER2 = ['BV1Aa0000006'];
const LATER = ['BV1Bb0000001', 'BV1Bb0000002'];

let site;
let daemon;
let home;
let owner = '';
let deliverToken = '';

async function api(method, path, body, token = owner) {
  const res = await fetch('http://127.0.0.1:' + PORT + path, {
    method,
    headers: { authorization: 'Bearer ' + token, 'content-type': 'application/json' },
    body: body && JSON.stringify(body),
    signal: AbortSignal.timeout(5000),
  });
  return { status: res.status, json: res.status === 204 ? null : await res.json() };
}

before(async () => {
  site = await startSite();
  if (!BIN) return;
  home = mkdtempSync(join(process.env.TMPDIR || tmpdir(), 'usuyo-userscript-'));
  daemon = spawn(BIN, ['daemon', '--port', String(PORT), '--first-library', join(home, 'lib')], {
    env: { ...process.env, USUYO_HOME: home },
    stdio: 'ignore',
  });
  await until(() => existsSync(join(home, 'api-token')), 15000);
  owner = readFileSync(join(home, 'api-token'), 'utf8').trim();
  await until(() => owner.length > 0);
  let created;
  for (let i = 0; i < 50 && !created; i++) {
    try {
      created = await api('POST', '/deliver-tokens', { name: 'B站油猴脚本（测试）', kind: 'extension' });
    } catch {
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  assert.equal(created.status, 200);
  deliverToken = created.json.token;
});

after(async () => {
  await site.close();
  if (daemon) {
    daemon.kill('SIGTERM');
    await new Promise((r) => daemon.once('exit', r));
  }
  if (home) spawnSync('trash', [home], { timeout: 10000 });
});

const list = (path) => site.origin + path;

/** 让「页面」自己请求收藏夹的两页和另一个收藏夹，跟真页面翻收藏夹一样。 */
async function browseFavorites(s) {
  await s.page.fetch(list('/x/v3/fav/resource/list?media_id=1001&pn=1&ps=20&platform=web'));
  await s.page.fetch(list('/x/v3/fav/resource/list?media_id=1001&pn=2&ps=20&platform=web'));
  await s.page.fetch(list('/x/v3/fav/resource/list?media_id=1002&pn=1&ps=20&platform=web'));
  await new Promise((r) => setTimeout(r, 50));
}

/** 稍后再看页自己请求的两个列表（旧接口、播放列表接口）。 */
async function browseLater(s) {
  await s.page.fetch(list('/x/v2/history/toview/web?pn=1&ps=20'));
  await s.page.fetch(list('/x/v2/medialist/resource/list?mobi_app=web&type=2&ps=20'));
  await new Promise((r) => setTimeout(r, 50));
}

const keys = (o) => Object.keys(o).sort();

/** 点交给，在浏览器的对话框里贴令牌，等到不再是「先确认…」。 */
async function handOver(s, token) {
  if (token) s.answerPrompt(token);
  s.click('交给 usuyo');
  await until(() => !/先确认/.test(s.note()));
}

/** 每个测试最后都查：令牌没进页面的存储、没出现在面板上，脚本只连了 127.0.0.1。 */
function noLeak(s, token, port) {
  const secret = token.split('.')[1];
  assert.ok(!s.panelText().includes(secret), 'secret shown in the panel');
  assert.ok(!s.hasInput(), 'an input element in the page');
  for (const v of s.pageStorage.values()) assert.ok(!v.includes(secret), 'token in page storage');
  assert.deepEqual(s.violations, []);
  for (const r of s.gmRequests) assert.equal(new URL(r.url).host, '127.0.0.1:' + port);
}

test('parses a folder answer: fields, de-duplication across pages, skipped kinds', async () => {
  const s = load({ port: 1 });
  await browseFavorites(s);
  const { favorite, later } = s.gm.get('items');
  assert.deepEqual(keys(favorite), [...FOLDER1, ...FOLDER2].sort());
  assert.deepEqual(later, {});
  // 第 3 条在两页里各出现一次，只记一条
  assert.equal(Object.keys(favorite).length, 5);
  assert.deepEqual(favorite.BV1Aa0000001, { title: '示例视频 1', up: '示例UP主甲', folder: '示例收藏夹甲', at: 1700001000 * 1000 });
  assert.equal(favorite.BV1Aa0000006.folder, '示例收藏夹乙');
  // 音频、合集是别的种类；type 为 2、attr 为 9 的是失效的：都不记，各自数出来
  assert.ok(!('BV1Aa0000004' in favorite));
  assert.match(s.panelText(), /收藏夹 5（已到底） · 稍后再看 0/);
  assert.match(s.panelText(), /跳过：不是视频的 2 条、已失效的 1 条/);
  assert.equal(s.gmRequests.length, 0, 'recording sends nothing');
  // 页面自己的那三次请求以外，脚本没向页面的站点发过东西
  assert.equal(site.seen.length, 3);
  assert.equal(s.pageStorage.size, 0, 'nothing written to page storage');
});

test('skipped entries are counted once however often the page asks again', async () => {
  const s = load({ port: 1 });
  await browseFavorites(s);
  await browseFavorites(s);
  assert.match(s.panelText(), /跳过：不是视频的 2 条、已失效的 1 条/);
});

test('parses both watch-later answer shapes on the watch-later page', async () => {
  const s = load({ port: 1, host: 'www.bilibili.com', path: '/list/watchlater' });
  await browseLater(s);
  const { favorite, later } = s.gm.get('items');
  assert.deepEqual(favorite, {});
  // toview：两条正常、一条失效；medialist：和 toview 重复一条、新增一条、一条不是视频
  assert.deepEqual(keys(later), [...LATER, 'BV1Bb0000004']);
  assert.deepEqual(later.BV1Bb0000002, { title: '示例稍后再看 2', up: '示例UP主辛', folder: '', at: 1700011000 * 1000 });
  assert.equal(later.BV1Bb0000004.up, '示例UP主壬');
  assert.match(s.panelText(), /稍后再看 3/);
  assert.match(s.panelText(), /跳过：不是视频的 1 条、已失效的 1 条/);
  assert.equal(s.gmRequests.length, 0);
});

test('the playlist interface counts as watch-later only on the watch-later page', async () => {
  const s = load({ port: 1, host: 'space.bilibili.com', path: '/42/favlist' });
  await s.page.fetch(list('/x/v2/medialist/resource/list?type=2'));
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(s.gm.get('items'), undefined);
});

test('ignores other requests and says so when Bilibili answers with an error code', async () => {
  const s = load({ port: 1 });
  await s.page.fetch(list('/x/v3/fav/folder/created/list-all?up_mid=42'));
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(s.gm.get('items'), undefined);
  await s.page.fetch(list('/x/v3/fav/resource/list?media_id=666&pn=1&ps=20'));
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(s.gm.get('items'), undefined);
  assert.match(s.panelText(), /B站回了 code -101，先别往下翻了/);
});

// 脚本装在哪个页面上都收旧接口（toview）；播放列表接口只在稍后再看页上收，所以这里只有 toview 那两条
test('delivers folders and watch-later to a throwaway usuyo daemon', { skip: !BIN && 'USUYO_BIN not set' }, async () => {
  const s = load({ port: PORT });
  await browseFavorites(s);
  await browseLater(s);
  const before = (await api('GET', '/imports')).json.length;
  const pageRequests = site.seen.length;
  await handOver(s, deliverToken);
  assert.equal(site.seen.length, pageRequests, 'delivery sends nothing to Bilibili');
  assert.match(s.note(), /交给 usuyo 了：收藏夹 5 条、稍后再看 2 条，到 usuyo 的导入页确认了才会去取/);

  // 握手只发随机数和令牌的 id，令牌只在核对过以后进 /v1/batches 的请求头
  const [hs, ...rest] = s.gmRequests;
  assert.equal(new URL(hs.url).pathname, '/handshake');
  assert.deepEqual(Object.keys(JSON.parse(hs.data)).sort(), ['client', 'nonce']);
  assert.equal(JSON.parse(hs.data).client, deliverToken.split('.')[0]);
  assert.ok(!JSON.stringify(hs).includes(deliverToken.split('.')[1]));
  assert.deepEqual(rest.map((r) => new URL(r.url).pathname), ['/v1/batches', '/v1/batches', '/v1/batches']);

  const imports = (await api('GET', '/imports')).json;
  assert.equal(imports.length, before + 3);
  const byTitle = Object.fromEntries(imports.map((i) => [i.title, i]));
  const day = new Date().toLocaleDateString('sv');
  const expect = [
    ['B站收藏夹「示例收藏夹甲」 ' + day, FOLDER1],
    ['B站收藏夹「示例收藏夹乙」 ' + day, FOLDER2],
    ['B站稍后再看 ' + day, LATER],
  ];
  for (const [name, bvids] of expect) {
    const imp = byTitle[name];
    assert.ok(imp, 'batch ' + name);
    assert.equal(imp.kind, 'delivered');
    const entries = (await api('GET', '/imports/' + imp.id + '/entries')).json;
    assert.deepEqual(entries.map((e) => e.url).sort(), bvids.map((b) => 'https://www.bilibili.com/video/' + b).sort());
    for (const e of entries) {
      assert.equal(e.action, 'favorite');
      assert.match(e.title, /^示例/);
      assert.match(e.actedAt, /^2023-11-1\d/);
    }
  }
  // 交出去的从脚本里去掉
  assert.deepEqual(s.gm.get('items'), { favorite: {}, later: {} });
  assert.equal(s.gm.get('deliverToken'), deliverToken);
  assert.match(s.panelText(), /令牌 [0-9a-f]{6}… 存在脚本管理器里/);
  noLeak(s, deliverToken, PORT);
});

test('a listener that cannot prove itself gets nothing', async (t) => {
  const fake = await startListener(() => [200, { proof: randomBytes(32).toString('hex') }]);
  t.after(() => fake.close());
  const token = randomBytes(8).toString('hex') + '.' + randomBytes(32).toString('hex');
  const s = load({ port: fake.port });
  await browseFavorites(s);
  await handOver(s, token);
  assert.match(s.note(), /没能证明自己是 usuyo.*令牌没有发出去，什么都没交/);
  assert.deepEqual(fake.seen.map((r) => r.url), ['/handshake']);
  const secret = token.split('.')[1];
  for (const r of fake.seen) {
    assert.ok(!JSON.stringify(r.headers).includes(secret) && !r.body.includes(secret));
    assert.equal(r.headers.authorization, undefined);
  }
  assert.equal(Object.keys(s.gm.get('items').favorite).length, 5, 'entries stay');
  noLeak(s, token, fake.port);
});

test('a peer that answers the handshake with an error status gets nothing', async (t) => {
  const fake = await startListener(() => [500, {}]);
  t.after(() => fake.close());
  const token = randomBytes(8).toString('hex') + '.' + randomBytes(32).toString('hex');
  const s = load({ port: fake.port });
  await browseFavorites(s);
  await handOver(s, token);
  assert.match(s.note(), /没能证明自己是 usuyo/);
  assert.deepEqual(fake.seen.map((r) => r.url), ['/handshake']);
  noLeak(s, token, fake.port);
});

test('closed port says usuyo is not running', async () => {
  const gone = await startListener(() => [500, {}]);
  const port = gone.port;
  await gone.close();
  const token = randomBytes(8).toString('hex') + '.' + randomBytes(32).toString('hex');
  const s = load({ port });
  await browseFavorites(s);
  await handOver(s, token);
  assert.match(s.note(), /usuyo 没有开着（本机 \d+ 端口没有回应），什么都没交/);
  assert.equal(s.gmRequests.length, 1);
  noLeak(s, token, port);
});

test('a refused token stops the delivery', async (t) => {
  const token = randomBytes(8).toString('hex') + '.' + randomBytes(32).toString('hex');
  // 能答对握手、却不认这个令牌的对面（例如令牌在握手之后被撤掉）
  const fake = await startListener((req, body) => {
    if (req.url === '/handshake') {
      const { nonce } = JSON.parse(body);
      const proof = createHmac('sha256', token).update(`usuyo-daemon-proof/v1\n127.0.0.1:${fake.port}\n${nonce}`).digest('hex');
      return [200, { proof }];
    }
    return [401, { code: 'unauthorized' }];
  });
  t.after(() => fake.close());
  const s = load({ port: fake.port });
  await browseFavorites(s);
  await handOver(s, token);
  assert.match(s.note(), /usuyo 不认这个令牌（可能撤掉了），剩下的没有收下/);
  assert.deepEqual(fake.seen.map((r) => r.url), ['/handshake', '/v1/batches'], 'stops after the first refusal');
  assert.equal(Object.keys(s.gm.get('items').favorite).length, 5, 'entries stay');
  noLeak(s, token, fake.port);
  s.click('忘掉令牌');
  assert.equal(s.gm.get('deliverToken'), '');
});

test('a token revoked in usuyo is never sent to it', { skip: !BIN && 'USUYO_BIN not set' }, async () => {
  const created = await api('POST', '/deliver-tokens', { name: '撤掉的', kind: 'extension' });
  const token = created.json.token;
  assert.equal((await api('DELETE', '/deliver-tokens/' + created.json.id)).status, 204);
  const s = load({ port: PORT });
  await browseFavorites(s);
  await handOver(s, token);
  assert.match(s.note(), /也可能这个令牌在 usuyo 里撤掉了/);
  assert.deepEqual(s.gmRequests.map((r) => new URL(r.url).pathname), ['/handshake']);
  noLeak(s, token, PORT);
});

test('a malformed token is not kept', async () => {
  const s = load({ port: 1 });
  await browseFavorites(s);
  s.answerPrompt('not-a-token');
  s.click('交给 usuyo');
  assert.match(s.note(), /这不像 usuyo 的投递令牌/);
  assert.equal(s.gm.get('deliverToken'), undefined);
  assert.equal(s.gmRequests.length, 0);
});

test('nothing noted yet: no prompt, no request', async () => {
  const s = load({ port: 1 });
  s.click('交给 usuyo');
  assert.match(s.note(), /还没有记下的条目/);
  assert.equal(s.prompted.length, 0);
  assert.equal(s.gmRequests.length, 0);
});

// 脚本管理器替脚本带上的 Origin（见 README 的「脚本管理器带什么 Origin」）：扩展的来源能进，页面的来源不能
for (const origin of ['chrome-extension://abcdefghijklmnop', 'moz-extension://0b1e5a8c-1111-4222-8333-944445555666', 'safari-web-extension://0B1E5A8C-1111-4222-8333-944445555666']) {
  test('delivers with Origin ' + origin.split(':')[0], { skip: !BIN && 'USUYO_BIN not set' }, async () => {
    const s = load({ port: PORT, origin });
    await browseFavorites(s);
    await handOver(s, deliverToken);
    assert.match(s.note(), /交给 usuyo 了：收藏夹 5 条、稍后再看 0 条/);
    noLeak(s, deliverToken, PORT);
  });
}

test('a page Origin is turned away before any token is sent', { skip: !BIN && 'USUYO_BIN not set' }, async () => {
  const s = load({ port: PORT, origin: 'https://www.bilibili.com' });
  await browseFavorites(s);
  await handOver(s, deliverToken);
  assert.match(s.note(), /usuyo 没让这个脚本管理器的请求进来，什么都没交/);
  assert.deepEqual(s.gmRequests.map((r) => new URL(r.url).pathname), ['/handshake']);
  noLeak(s, deliverToken, PORT);
});

test('the token can be entered and forgotten from the script manager menu', async () => {
  const token = randomBytes(8).toString('hex') + '.' + randomBytes(32).toString('hex');
  const s = load({ port: 1 });
  assert.deepEqual(s.menuLabels(), ['填 usuyo 令牌', '忘掉 usuyo 令牌']);
  await browseFavorites(s);
  // 对话框没弹出来或被关掉（prompt 回 null）
  s.click('交给 usuyo');
  assert.match(s.note(), /没拿到令牌.*脚本管理器的菜单里点「填 usuyo 令牌」/);
  assert.equal(s.gmRequests.length, 0);
  s.answerPrompt('  ' + token + '\n');
  s.menu('填 usuyo 令牌');
  assert.equal(s.gm.get('deliverToken'), token, 'kept, trimmed');
  // 面板上只有 id 的前 6 位
  assert.ok(s.panelText().includes('令牌 ' + token.slice(0, 6) + '…'));
  assert.ok(!s.panelText().includes(token.slice(6, 16)) && !s.panelText().includes(token.slice(17, 25)));
  s.menu('忘掉 usuyo 令牌');
  assert.equal(s.gm.get('deliverToken'), '');
  assert.ok(!s.hasInput());
});

// 页面和脚本在同一个环境里时，页面能改掉的内置方法：换成会记账的间谍
function plant(page, secret) {
  const hits = [];
  const has = String.prototype.indexOf;
  const values = Object.values;
  let inside = false;
  // 只算脚本自己直接调的：脚本管理器那一侧（测试里是 harness 的 GM_xmlhttpRequest 和 node:http）不算
  const fromScript = () => {
    const st = new Error().stack;
    // 栈：Error、fromScript、间谍自己，再下一帧才是调它的
    const own = Reflect.apply(has, st, ['\n    at ']);
    const spyFrame = Reflect.apply(has, st, ['\n    at ', own + 1]);
    const caller = Reflect.apply(has, st, ['\n    at ', spyFrame + 1]);
    const next = Reflect.apply(has, st, ['\n', caller + 1]);
    const at = Reflect.apply(has, st, [SCRIPT_FRAME, caller]);
    return caller >= 0 && at >= 0 && (next < 0 || at < next);
  };
  const carries = (v, depth = 0) => {
    if (typeof v === 'string') return Reflect.apply(has, v, [secret]) >= 0;
    if (v && typeof v === 'object' && depth < 2) {
      try {
        return values(v).some((x) => carries(x, depth + 1));
      } catch {
        return false;
      }
    }
    return false;
  };
  const restores = [];
  const spy = (obj, name, label) => {
    const orig = obj[name];
    obj[name] = function (...args) {
      if (!inside) {
        inside = true;
        try {
          if (fromScript() && (carries(this) || args.some((a) => carries(a)))) hits.push(label);
        } finally {
          inside = false;
        }
      }
      return new.target ? Reflect.construct(orig, args, new.target) : Reflect.apply(orig, this, args);
    };
    restores.push(() => (obj[name] = orig));
  };
  for (const n of ['exec', 'test']) spy(RegExp.prototype, n, 'RegExp.' + n);
  for (const n of ['trim', 'indexOf', 'slice', 'charCodeAt', 'split', 'substring']) spy(String.prototype, n, 'String.' + n);
  spy(TextEncoder.prototype, 'encode', 'TextEncoder.encode');
  spy(JSON, 'stringify', 'JSON.stringify');
  for (const n of ['call', 'apply', 'bind']) spy(Function.prototype, n, 'Function.' + n);
  const promptCalls = [];
  const pagePrompt = page.prompt;
  page.prompt = (...a) => {
    promptCalls.push(a);
    return null;
  };
  restores.push(() => (page.prompt = pagePrompt));
  return { hits, promptCalls, restore: () => restores.reverse().forEach((r) => r()) };
}

test('page-patched builtins never see the token', { skip: !BIN && 'USUYO_BIN not set' }, async () => {
  const s = load({ port: PORT });
  await browseFavorites(s);
  const secret = deliverToken.split('.')[1];
  const spies = plant(s.page, secret);
  try {
    s.answerPrompt(' ' + deliverToken + ' ');
    s.click('交给 usuyo');
    await until(() => !/先确认|令牌存着了/.test(s.note()));
  } finally {
    spies.restore();
  }
  assert.match(s.note(), /交给 usuyo 了：收藏夹 5 条、稍后再看 0 条/);
  assert.deepEqual(spies.hits, [], 'a patched builtin saw the token');
  assert.deepEqual(spies.promptCalls, [], 'the page prompt was used');
  assert.equal(s.prompted.length, 1, 'the prompt captured at start was used');
  assert.ok(!s.hasInput());
  noLeak(s, deliverToken, PORT);
});
