// Run with node --test tests/remote_ui.mjs (Node 22+ and Chrome).
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import test from 'node:test';

const chrome = process.env.CHROME_BIN || [
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/usr/bin/google-chrome', '/usr/bin/chromium',
].find(existsSync);

test('remote conversation is rich, safe and visible on phone screens', { skip: !chrome, timeout: 30000 }, async () => {
  const profile = mkdtempSync(join(tmpdir(), 'bhai-remote-ui-'));
  const child = spawn(chrome, ['--headless=new', '--remote-debugging-port=0', '--no-first-run',
    '--no-default-browser-check', '--user-data-dir=' + profile, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
  let socket;
  const exited = new Promise(resolve => child.once('close', resolve));
  try {
    const address = await new Promise((resolve, reject) => {
      let output = '';
      const timer = setTimeout(() => reject(new Error('Chrome startup timed out')), 10000);
      child.once('error', reject);
      child.stderr.on('data', data => {
        output += data;
        const match = /DevTools listening on (ws:\/\/[^\s]+)/.exec(output);
        if (match) { clearTimeout(timer); resolve(match[1]); }
      });
    });
    socket = new WebSocket(address);
    await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
    let next = 0, session;
    const pending = new Map(), errors = [];
    socket.onmessage = event => {
      const data = JSON.parse(event.data);
      if (data.method === 'Runtime.exceptionThrown') errors.push(data.params.exceptionDetails.text);
      if (!data.id) return;
      const waiter = pending.get(data.id); pending.delete(data.id);
      if (data.error) waiter.reject(new Error(JSON.stringify(data.error))); else waiter.resolve(data.result);
    };
    const call = (method, params = {}) => new Promise((resolve, reject) => {
      const id = ++next; pending.set(id, { resolve, reject });
      socket.send(JSON.stringify({ id, method, params, ...(session ? { sessionId: session } : {}) }));
    });
    const { targetId } = await call('Target.createTarget', { url: 'about:blank' });
    session = (await call('Target.attachToTarget', { targetId, flatten: true })).sessionId;
    await call('Runtime.enable');
    const evaluate = async expression => {
      const result = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
      if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description || result.exceptionDetails.text);
      return result.result.value;
    };
    await call('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await call('Page.navigate', { url: pathToFileURL(fileURLToPath(new URL('../assets/remote.html', import.meta.url))).href });
    for (let i = 0; i < 100; i++) {
      if (await evaluate('typeof snapshot === "function"')) break;
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    const transcript = [
      { kind: 'user', data: 'Make the mobile chat readable.' },
      { kind: 'commentary', data: 'I will check the layout and preserve **approvals**.' },
      { kind: 'command', data: { tool: 'bash', summary: 'cargo test --locked' } },
      { kind: 'output', data: '221 tests passed' },
      { kind: 'assistant', data: '# Mobile chat\n\nReadable **messages**, *emphasis* and `inline code`.\n\n- Conversation first\n- Tools stay accessible\n\n```rust\nlet ready = true;\n```\n\n[Docs](https://example.com)\n\n<img src=x onerror=alert(1)> [bad](javascript:alert)\n\n> Keep the key private.' },
    ];
    transcript[4].data += '\n\n| Check | Result |\n| --- | --- |\n| **Mobile** | Ready |';
    const state = { model: 'gpt-test', mode: 'ask', working: true, queued: [] };
    const data = { title: 'Mobile UI', state, transcript, children: [] };
    await evaluate(`updateSessions([{ id: 'test-session', title: 'Mobile UI', project: '/project', connected: true }]); snapshot(${JSON.stringify(data)});`);
    assert.equal(await evaluate('document.body.dataset.view'), 'chat');
    assert.equal(await evaluate('$("controls").hidden'), false);
    await evaluate('status("Connected · updated 5:00:00 PM")');
    assert.equal(await evaluate('$("status").textContent'), 'Connected');
    assert.equal(await evaluate('$("transcript").querySelector("table tbody strong").textContent'), 'Mobile');
    assert.equal(await evaluate('$("transcript").querySelector("strong").textContent'), 'approvals');
    assert.equal(await evaluate('$("transcript").querySelector(".command pre").textContent'), 'cargo test --locked');
    assert.equal(await evaluate('$("transcript").querySelector(".code-block code").textContent'), 'let ready = true;');
    assert.equal(await evaluate('$("transcript").querySelectorAll("img, script, a[href^=javascript]").length'), 0);
    assert.equal(await evaluate('$("transcript").querySelector("a").rel'), 'noopener noreferrer');
    assert.equal(await evaluate('$("transcript").querySelector(".output details").open'), false);
    await evaluate('window.firstEntry = $("transcript").firstChild; $("transcript").querySelector(".output details").open = true');
    data.transcript.push({ kind: 'assistant', data: 'One more streaming update.' });
    await evaluate(`snapshot(${JSON.stringify(data)})`);
    assert.equal(await evaluate('window.firstEntry === $("transcript").firstChild'), true);
    assert.equal(await evaluate('$("transcript").querySelector(".output details").open'), true);
    for (const width of [320, 390, 768, 1280]) {
      await call('Emulation.setDeviceMetricsOverride', { width, height: 844, deviceScaleFactor: 1, mobile: true });
      await evaluate('new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))');
      const geometry = await evaluate(`(() => {
        const chat = $('transcript').getBoundingClientRect(), prompt = $('prompt').getBoundingClientRect();
        return { height: chat.height, bottom: prompt.bottom, viewport: visualViewport.height,
          width: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth };
      })()`);
      assert.ok(geometry.height > 250, JSON.stringify({ width, geometry }));
      assert.ok(geometry.bottom <= geometry.viewport, JSON.stringify({ width, geometry }));
      assert.ok(geometry.scroll <= geometry.width, JSON.stringify({ width, geometry }));
    }
    await call('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await evaluate('new Promise(resolve => requestAnimationFrame(resolve))');
    if (process.env.BHAI_UI_SCREENSHOT) {
      await evaluate('$("transcript").scrollTop = 0');
      const image = await call('Page.captureScreenshot', { format: 'png' });
      writeFileSync(process.env.BHAI_UI_SCREENSHOT, Buffer.from(image.data, 'base64'));
    }
    await call('Emulation.setDeviceMetricsOverride', { width: 390, height: 500, deviceScaleFactor: 1, mobile: true });
    await evaluate('new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))');
    assert.ok(await evaluate('$("prompt").getBoundingClientRect().bottom <= visualViewport.height'));
    assert.ok(await evaluate('$("transcript").clientHeight > 80'));
    await call('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await evaluate('new Promise(resolve => requestAnimationFrame(resolve))');
    data.state.pending = { id: 7, tool: 'bash', command: 'cargo test' };
    await evaluate(`snapshot(${JSON.stringify(data)})`);
    assert.equal(await evaluate('$("approval").hidden'), false);
    assert.ok(await evaluate('$("transcript").clientHeight > 100'));
    await evaluate(`window.sent = []; window.originalRequest = request;
      request = async (path, method, body) => { sent.push({ path, method, body }); return {}; };
      token = 'synthetic-test-token'; $('prompt').value = 'Hello from the phone';
      $('prompt-form').onsubmit(new Event('submit', { cancelable: true }));
      new Promise(resolve => setTimeout(resolve, 0));`);
    assert.equal(await evaluate('sent[0].path'), '/sessions/test-session/prompt');
    assert.equal(await evaluate('sent[0].body.text'), 'Hello from the phone');
    assert.equal(await evaluate('$("prompt").value'), '');
    await evaluate('$("approval-buttons").firstChild.onclick()');
    assert.equal(await evaluate('sent[1].path'), '/sessions/test-session/approve');
    assert.equal(await evaluate('sent[1].body.id'), 7);
    await evaluate('$("stop-turn").onclick()');
    assert.equal(await evaluate('sent[2].path'), '/sessions/test-session/interrupt');
    await evaluate('request = window.originalRequest; token = ""');
    await evaluate('$("view-tools").click()');
    assert.equal(await evaluate('getComputedStyle($("tools")).display === "none"'), false);
    assert.equal(await evaluate('getComputedStyle($("chat-panel")).display'), 'none');
    await evaluate('$("view-connection").click()');
    assert.equal(await evaluate('getComputedStyle($("pairing")).display === "none"'), false);
    await evaluate('$("view-chat").click()');
    const many = Array.from({ length: 40 }, (_, i) => ({ kind: 'assistant', data: 'Message ' + i + '\n\nA readable conversation.' }));
    await evaluate(`entries($('transcript'), ${JSON.stringify(many)}); $('follow').checked = false; $('transcript').scrollTop = 0;`);
    many.push({ kind: 'assistant', data: 'New answer' });
    await evaluate(`entries($('transcript'), ${JSON.stringify(many)})`);
    assert.equal(await evaluate('$("transcript").scrollTop'), 0);
    await evaluate('$("follow").checked = true; $("follow").onchange()');
    assert.ok(await evaluate('$("transcript").scrollTop > 0'));
    await evaluate('entries($("transcript"), [])');
    assert.match(await evaluate('$("transcript").textContent'), /Start a conversation/);
    assert.deepEqual(errors, []);
  } finally {
    socket?.close(); child.kill('SIGTERM'); await exited;
    rmSync(profile, { recursive: true, force: true });
  }
});
