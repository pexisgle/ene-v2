// npm install --no-save --package-lock=false @extism/extism@2.0.0-rc13
// node examples/plugins/openrouter-wasm/test.cjs target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const createPlugin = require('@extism/extism');

(async () => {
    const wasm = readFileSync(process.argv[2]);
    const text = 'data: {"choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]}\n\n';
    const unicode = Buffer.from(text.replace('hello', 'こんにちは'));
    const split = unicode.indexOf(Buffer.from('こ')) + 1;
    const delta = { type: 'delta', text: 'hello' };
    const finished = { type: 'finished', reason: 'stop' };
    // The broker still has an open response after delivering the terminal event.
    for (const { chunks = [], done = false, response, error, models = false,
        expected = [delta, finished] } of [
        { chunks: [text + 'data: [DONE]\n\n'] },
        { chunks: [text + 'data: [DO', 'NE]\r\n\r\n'] },
        { chunks: [text + 'data: [DONE]\n\n'], done: true },
        { chunks: [text.repeat(128) + 'data: [DONE]\n\n'], expected: [...Array(128).fill(delta), finished] },
        { chunks: [text + 'data: {"choices":', '[{"delta":{"content":"again"}}]}\r\n\r\ndata: [DONE]\n'],
            expected: [delta, { type: 'delta', text: 'again' }, finished] },
        { chunks: [unicode.subarray(0, split), unicode.subarray(split), 'data: [DONE]\n'],
            expected: [{ type: 'delta', text: 'こんにちは' }, finished] },
        { chunks: ['{"data":[{"id":"fixture",', '"context_length":4096}]}'], done: true, models: true },
        { response: JSON.stringify({ Err: { code: 'denied', message: 'destination is not allowed' } }),
            error: /denied: destination is not allowed/ },
        { chunks: [':' + 'x'.repeat(1024 * 1024) + '\n'], error: /SSE line limit/ },
        { chunks: [':' + 'x'.repeat(1024 * 1024)], error: /SSE line limit/ },
        { response: JSON.stringify({ Ok: { handle: 'stream' } }), error: /missing field/ },
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
                        if (request.type === 'http_open') {
                            assert.equal(request.request.method, models ? 'GET' : 'POST');
                            if (!models) assert.deepEqual(JSON.parse(Buffer.from(request.request.body_base64, 'base64')), {
                                model: 'fixture', stream: true, max_tokens: 32,
                                messages: [{ role: 'system', content: 'Be helpful' }, { role: 'user', content: 'hello' }],
                            });
                        }
                        if (response !== undefined) return cp.store(response);
                        if (request.type === 'http_close') {
                            assert.equal(done, false, 'EOF already released the handle');
                            assert.equal(request.handle, 'stream');
                            return cp.store(JSON.stringify({ Ok: null }));
                        }
                        assert.ok(['http_open', 'http_read'].includes(request.type));
                        assert.ok(chunks.length, 'read after [DONE]: the open response would block');
                        const data_base64 = Buffer.from(chunks.shift()).toString('base64');
                        return cp.store(JSON.stringify({ Ok: {
                            handle: 'stream', status: 200, headers: {}, done: done && chunks.length === 0,
                            data_base64,
                        } }));
                    },
                    ene_emit(cp, offset) { events.push(cp.read(offset).json()); },
                },
            },
        });
        try {
            const invoke = () => plugin.call('ene_invoke', JSON.stringify(models
                ? { type: 'list_models', endpoint: 'https://openrouter.ai/api/v1', options: {} }
                : { type: 'generate', request: {
                endpoint: 'https://openrouter.ai/api/v1', options: {}, model: 'fixture',
                preamble: 'Be helpful', messages: [{ role: 'user', text: 'hello' }], max_tokens: 32,
            } }));
            if (error) {
                await assert.rejects(invoke, error);
                assert.deepEqual(calls, ['http_open']);
                assert.deepEqual(events, []);
                continue;
            }
            await invoke();
            assert.equal(chunks.length, 0);
            assert.equal(calls[0], 'http_open');
            assert.equal(calls.filter(c => c === 'http_close').length, done ? 0 : 1);
            if (!done) assert.equal(calls.at(-1), 'http_close');
            if (models) {
                assert.equal(events.length, 1);
                assert.equal(events[0].type, 'models');
                assert.equal(events[0].models.length, 1);
                assert.equal(events[0].models[0].id, 'fixture');
                assert.equal(events[0].models[0].name, 'fixture');
                assert.equal(events[0].models[0].metadata.contextWindow, '4096');
            } else {
                assert.deepEqual(events, expected);
            }
        } finally {
            await plugin.close();
        }
    }
    console.log('PASS: stream completion, typed host responses and host error details');
})().catch(error => { console.error(error); process.exitCode = 1; });
