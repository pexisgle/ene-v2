// npm install --no-save --package-lock=false @extism/extism@2.0.0-rc13
// node examples/plugins/openrouter-wasm/test.cjs target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const createPlugin = require('@extism/extism');

(async () => {
    const wasm = readFileSync(process.argv[2]);
    const text = 'data: {"choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]}\n\n';
    // The broker still has an open response after delivering the terminal event.
    for (const { chunks, done = false } of [
        { chunks: [text + 'data: [DONE]\n\n'] },
        { chunks: [text + 'data: [DO', 'NE]\r\n\r\n'] },
        { chunks: [text + 'data: [DONE]\n\n'], done: true },
    ]) {
        const events = [];
        const calls = [];
        const plugin = await createPlugin({ wasm: [{ data: wasm }] }, {
            useWasi: false,
            functions: {
                'extism:host/user': {
                    ene_service(cp, offset) {
                        const request = cp.read(offset).json();
                        calls.push(request.type);
                        if (request.type === 'http_close') {
                            assert.equal(done, false, 'EOF already released the handle');
                            assert.equal(request.handle, 'stream');
                            return cp.store(JSON.stringify({ Ok: null }));
                        }
                        assert.ok(['http_open', 'http_read'].includes(request.type));
                        assert.ok(chunks.length, 'read after [DONE]: the open response would block');
                        return cp.store(JSON.stringify({ Ok: {
                            handle: 'stream', status: 200, headers: {}, done,
                            data_base64: Buffer.from(chunks.shift()).toString('base64'),
                        } }));
                    },
                    ene_emit(cp, offset) { events.push(cp.read(offset).json()); },
                },
            },
        });
        try {
            await plugin.call('ene_invoke', JSON.stringify({ type: 'generate', request: {
                endpoint: 'https://openrouter.ai/api/v1', options: {}, model: 'fixture',
                preamble: '', messages: [{ role: 'user', text: 'hello' }], max_tokens: 32,
            } }));
            assert.equal(chunks.length, 0);
            assert.equal(calls[0], 'http_open');
            assert.equal(calls.filter(c => c === 'http_close').length, done ? 0 : 1);
            if (!done) assert.equal(calls.at(-1), 'http_close');
            assert.deepEqual(events, [
                { type: 'delta', text: 'hello' }, { type: 'finished', reason: 'stop' },
            ]);
        } finally {
            await plugin.close();
        }
    }
    console.log('PASS: whole/split [DONE] closes without waiting for EOF; EOF is not closed twice');
})().catch(error => { console.error(error); process.exitCode = 1; });
