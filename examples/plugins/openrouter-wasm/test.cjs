// npm install --no-save --package-lock=false @extism/extism@2.0.0-rc13
// node examples/plugins/openrouter-wasm/test.cjs target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const createPlugin = require('@extism/extism');

const endpoint = 'https://openrouter.ai/api/v1';
const done = 'data: [DONE]\n\n';
const delta = { type: 'delta', text: 'hello' };
const finished = { type: 'finished', reason: 'stop' };
const toolFinished = { type: 'finished', reason: 'tool_calls' };
const sse = choices => `data: ${JSON.stringify({ choices })}\n\n`;
const choice = (delta, finish_reason = null, index = 0) => ({ index, delta, finish_reason });
const call = (index = 0, id = 'call-1', name = 'weather', arguments = '{}') => ({
    ...(index === undefined ? {} : { index }), id, type: 'function', function: { name, arguments },
});
const toolEvent = (id = 'call-1', name = 'weather', arguments = {}) => ({ type: 'tool_call', id, name, arguments });
const tools = (calls, reason = null, index = 0) => sse([choice({ tool_calls: calls }, reason, index)]);
const brokerChunks = data => {
    const bytes = Buffer.from(data);
    return Array.from({ length: Math.ceil(bytes.length / 65536) }, (_, index) => bytes.subarray(index * 65536, (index + 1) * 65536));
};
const text = sse([choice({ content: 'hello' }, 'stop')]);
const complete = tools([call()], 'tool_calls');
const fragmented = [
    tools([call(0, 'call-1', 'weather', '')]),
    tools([{ index: 0, function: { arguments: '{"city":' } }]),
    tools([{ index: 0, function: { arguments: '"Tokyo"}' } }], 'tool_calls'),
];
const unicode = Buffer.from(text.replace('hello', 'こんにちは'));
const unicodeSplit = unicode.indexOf(Buffer.from('こ')) + 1;
const unicodeTool = Buffer.from(tools([call(0, 'call-1', 'weather', '{"city":"東京"}')], 'tool_calls') + done);
const unicodeToolSplit = unicodeTool.indexOf(Buffer.from('東')) + 1;
const request = {
    endpoint, options: {}, model: 'fixture', preamble: 'Be helpful',
    messages: [{ kind: 'text', role: 'user', text: 'hello' }], max_tokens: 32,
};
const payload = {
    model: 'fixture', stream: true, max_tokens: 32,
    messages: [{ role: 'system', content: 'Be helpful' }, { role: 'user', content: 'hello' }],
};
const historyRequest = {
    ...request, endpoint: endpoint + '/', tools: [{ name: 'weather', description: 'Get weather',
        parameters: { type: 'object', properties: { city: { type: 'string' } } } }],
    messages: [...request.messages,
        { kind: 'tool_calls', calls: [{ id: 'old-1', name: 'weather', arguments: { city: '東京' } },
            { id: 'old-2', name: 'clock', arguments: {} }] },
        { kind: 'tool_result', call_id: 'old-1', name: 'weather', content: '晴れ' },
        { kind: 'tool_result', call_id: 'old-2', name: 'clock', content: 'noon' },
        { kind: 'text', role: 'assistant', text: 'It is sunny' },
    ],
};
const historyPayload = {
    ...payload,
    messages: [...payload.messages, { role: 'assistant', tool_calls: [
        { id: 'old-1', type: 'function', function: { name: 'weather', arguments: '{"city":"東京"}' } },
        { id: 'old-2', type: 'function', function: { name: 'clock', arguments: '{}' } },
    ] }, { role: 'tool', tool_call_id: 'old-1', content: '晴れ' },
    { role: 'tool', tool_call_id: 'old-2', content: 'noon' },
    { role: 'assistant', content: 'It is sunny' }],
    tools: [{ type: 'function', function: { name: 'weather', description: 'Get weather',
        parameters: { type: 'object', properties: { city: { type: 'string' } } } } }],
};
const cases = [
    { name: 'DONE closes a live response', chunks: [text + done] },
    { name: 'fragmented arguments', chunks: [fragmented.join('') + done],
        expected: [toolEvent('call-1', 'weather', { city: 'Tokyo' }), toolFinished] },
    { name: 'fragmented arguments across broker reads', chunks: [...fragmented, done],
        beforeRead: [[], [], []], expected: [toolEvent('call-1', 'weather', { city: 'Tokyo' }), toolFinished] },
    { name: 'split DONE with CRLF', chunks: [text + 'data: [DO', 'NE]\r\n\r\n'], beforeRead: [[delta]] },
    { name: 'EOF releases response before DONE', chunks: [{ data: text + done, done: true }], closes: 0 },
    { name: 'many text events in one chunk', chunks: [text.repeat(128) + done],
        expected: [...Array(128).fill(delta), finished] },
    { name: 'split JSON and immediate text', chunks: [text + 'data: {"choices":',
        '[{"index":0,"delta":{"content":"again"}}]}\r\n\r\ndata: [DONE]\n'],
        beforeRead: [[delta]], expected: [delta, { type: 'delta', text: 'again' }, finished] },
    { name: 'UTF8 split inside codepoint', chunks: [unicode.subarray(0, unicodeSplit), unicode.subarray(unicodeSplit), done],
        beforeRead: [[], [{ type: 'delta', text: 'こんにちは' }]],
        expected: [{ type: 'delta', text: 'こんにちは' }, finished] },
    { name: 'request tools and complete tool history', request: historyRequest, payload: historyPayload, chunks: [text + done] },
    { name: 'unindexed text choices retain streaming compatibility', chunks: [sse([
        { delta: { content: 'hello' }, finish_reason: 'length' },
        { delta: { content: 'again' }, finish_reason: 'stop' },
    ]) + done], expected: [delta, { type: 'delta', text: 'again' }, finished] },
    { name: 'text retains latest finish reason', chunks: [sse([choice({ content: 'hello' }, 'length')]) + text + done],
        expected: [delta, delta, finished] },
    { name: 'DONE ignores unread trailing events', chunks: [text + done + 'data: {oops}\n'] },
    { name: 'text EOF without finish reason preserves incomplete', chunks: [{ data: sse([choice({ content: 'hello' })]), done: true }],
        closes: 0, expected: [delta, { type: 'finished', reason: 'incomplete' }] },
    { name: 'unterminated text SSE line is not dispatched', chunks: [{ data: text.trimEnd(), done: true }], closes: 0,
        expected: [{ type: 'finished', reason: 'incomplete' }] },
    { name: 'finish reason waits for trailing fragments', chunks: [
        tools([call(900000, '', 'wea', '{"city":')], 'tool_calls'),
        tools([{ index: 900000, id: 'late-id', function: { name: 'ther', arguments: '"東京"}' } }]), done,
    ], beforeRead: [[], []], expected: [toolEvent('late-id', 'weather', { city: '東京' }), toolFinished] },
    { name: 'parallel sparse calls are assembled independently', chunks: [
        tools([call(90, 'b', 'cl', '{"zone":'), call(3, 'a', 'wea', '{"city":')]),
        tools([{ index: 3, function: { name: 'ther', arguments: '"Tokyo"}' } },
            { index: 90, id: 'b', function: { name: 'ock', arguments: '"UTC"}' } }], 'tool_calls'), done,
    ], beforeRead: [[], []], expected: [toolEvent('a', 'weather', { city: 'Tokyo' }), toolEvent('b', 'clock', { zone: 'UTC' }), toolFinished] },
    { name: 'tool arguments preserve split UTF8 bytes', chunks: [unicodeTool.subarray(0, unicodeToolSplit), unicodeTool.subarray(unicodeToolSplit)],
        beforeRead: [[]], expected: [toolEvent('call-1', 'weather', { city: '東京' }), toolFinished] },
    { name: 'index-only start accepts late metadata', chunks: [tools([{ index: 7 }]),
        tools([{ index: 7, function: { arguments: ' {} ' } }]),
        tools([{ index: 7, id: 'call-1', function: { name: 'weather' } }], 'tool_calls'), done,
    ], beforeRead: [[], [], []], expected: [toolEvent(), toolFinished] },
    { name: 'same tool index in separate choices', chunks: [sse([
        choice({ tool_calls: [call(0, 'a', 'weather', '{}')] }, 'tool_calls', 5),
        choice({ tool_calls: [call(0, 'b', 'clock', '{"zone":"UTC"}')] }, 'tool_calls', 100),
    ]) + done], expected: [toolEvent('a'), toolEvent('b', 'clock', { zone: 'UTC' }), toolFinished] },
    { name: 'unindexed complete compatibility', chunks: [tools([
        { id: 'a', function: { name: 'weather', arguments: '{}' } },
        { id: 'b', function: { name: 'clock', arguments: '{"zone":"UTC"}' } },
    ], 'tool_calls') + done], expected: [toolEvent('a'), toolEvent('b', 'clock', { zone: 'UTC' }), toolFinished] },
    { name: 'unindexed IDs z then a preserve arrival order', chunks: [tools([
        { id: 'z', function: { name: 'weather', arguments: '{}' } },
        { id: 'a', function: { name: 'clock', arguments: '{}' } },
    ], 'tool_calls') + done], expected: [toolEvent('z'), toolEvent('a', 'clock'), toolFinished] },
    { name: 'unindexed IDs z then a preserve arrival order across reads', chunks: [tools([
        { id: 'z', function: { name: 'weather', arguments: '{}' } },
    ]), tools([{ id: 'a', function: { name: 'clock', arguments: '{}' } }], 'tool_calls'), done],
        beforeRead: [[], []], expected: [toolEvent('z'), toolEvent('a', 'clock'), toolFinished] },
    { name: 'mixed calls keep indexed order then unindexed arrival order', chunks: [tools([
        { id: 'z', function: { name: 'weather', arguments: '{}' } }, call(9, 'i9'),
        { id: 'a', function: { name: 'clock', arguments: '{}' } }, call(2, 'i2'),
    ], 'tool_calls') + done], expected: [toolEvent('i2'), toolEvent('i9'), toolEvent('z'), toolEvent('a', 'clock'), toolFinished] },
    { name: 'literal empty object is valid at EOF', chunks: [{ data: complete, done: true }], closes: 0,
        expected: [toolEvent(), toolFinished] },
    { name: 'DONE terminates complete calls without a finish reason', chunks: [tools([call()]) + done],
        expected: [toolEvent(), { type: 'finished', reason: 'incomplete' }] },
    { name: 'text is visible while calls remain pending', chunks: [
        sse([choice({ content: 'hello', tool_calls: [call(0, 'call-1', 'weather', '{"city":')] })]),
        tools([{ index: 0, function: { arguments: '"Tokyo"}' } }], 'tool_calls'), done,
    ], beforeRead: [[delta], [delta]], expected: [delta, toolEvent('call-1', 'weather', { city: 'Tokyo' }), toolFinished] },
    { name: 'model listing payload and metadata', models: true, chunks: ['{"data":[{"id":"fixture",',
        { data: '"context_length":4096,"top_provider":{"max_completion_tokens":512}}]}', done: true }], closes: 0,
        checkEvents(events) {
            assert.equal(events.length, 1);
            assert.equal(events[0].type, 'models');
            assert.equal(events[0].models.length, 1);
            assert.equal(events[0].models[0].id, 'fixture');
            assert.equal(events[0].models[0].name, 'fixture');
            assert.equal(events[0].models[0].metadata.contextWindow, '4096');
            assert.equal(events[0].models[0].metadata.maxOutputTokens, '512');
        } },
    { name: 'valid empty model listing', models: true, chunks: [{ data: '{"data":[]}', done: true }], closes: 0,
        expected: [{ type: 'models', models: [] }] },
    { name: 'R1 missing kind never opens HTTP', request: { ...request, messages: [{ role: 'user', text: 'hello' }] },
        chunks: [], error: /missing field.*kind/, phases: [], closes: 0 },
    { name: 'typed open failure', openResponse: { Err: { code: 'denied', message: 'destination is not allowed' } },
        error: /denied: destination is not allowed/, closes: 0, phases: ['http_open'] },
    { name: 'malformed open response cannot establish ownership', openResponse: { Ok: { handle: 'stream' } },
        error: /missing field/, closes: 0, phases: ['http_open'] },
    { name: 'typed read failure closes original handle', chunks: ['', { response: { Err: { code: 'read_failed', message: 'broken stream' } } }],
        error: /read_failed: broken stream/, phases: ['http_open', 'http_read', 'http_close'] },
    { name: 'malformed read response closes original handle', chunks: ['', { response: { Ok: { handle: 'stream' } } }],
        error: /missing field/, phases: ['http_open', 'http_read', 'http_close'] },
    { name: 'changed live handle closes original only', chunks: ['', { data: '', handle: 'changed' }],
        error: /changed response handle/, phases: ['http_open', 'http_read', 'http_close'] },
    { name: 'HTTP status error closes live handle', chunks: [{ data: '', status: 503 }], error: /HTTP failure/ },
    { name: 'HTTP status error at EOF does not close', chunks: [{ data: '', status: 503, done: true }],
        error: /HTTP failure/, closes: 0 },
    { name: 'bad base64 closes live handle', chunks: [{ base64: '@@@' }], error: /Invalid|invalid/ },
    { name: 'bad UTF8 closes live handle', chunks: [Buffer.from([0xff, 10])], error: /utf-8|UTF-8/ },
    { name: 'malformed JSON closes live handle', chunks: ['data: {oops}\n'], error: /key must be a string/ },
    { name: 'upstream error after text keeps delta and closes', chunks: [text, 'data: {"error":{"message":"no"}}\n'],
        error: /upstream failure/, expected: [delta], beforeRead: [[delta]] },
    { name: 'missing choices closes live handle', chunks: ['data: {}\n'], error: /missing choices/ },
    { name: 'text is retained before malformed calls in the same event', chunks: [sse([choice({ content: 'hello', tool_calls: {} })])],
        error: /invalid tool calls/, expected: [delta] },
    { name: 'late invalid sibling prevents earlier complete invocation', chunks: [complete,
        tools([call(1, 'invalid', 'clock', '[]')]), done], beforeRead: [[], []], error: /must be an object/ },
    { name: 'late argument bytes after finish reason are not discarded', chunks: [complete,
        tools([{ index: 0, function: { arguments: 'x' } }]), done], beforeRead: [[], []], error: /trailing characters/ },
    { name: 'invalid argument in another choice prevents all invocations', chunks: [sse([
        choice({ tool_calls: [call(0, 'valid')] }, 'tool_calls', 0),
        choice({ tool_calls: [call(0, 'invalid', 'clock', '[]')] }, 'tool_calls', 1),
    ]) + done], error: /must be an object/ },
    { name: 'changed EOF handle is never closed again', chunks: ['', { data: '', handle: 'changed', done: true }],
        error: /changed response handle/, closes: 0 },
    { name: 'SSE completed line limit', chunks: [':' + 'x'.repeat(1024 * 1024) + '\n'], error: /SSE line limit/ },
    { name: 'SSE retained line limit', chunks: [':' + 'x'.repeat(1024 * 1024)], error: /SSE line limit/ },
    { name: 'response byte limit', chunks: ['x'.repeat(8 * 1024 * 1024 + 1)], error: /response limit/ },
    { name: 'HTTP header count limit', chunks: [{ data: '', headers: Object.fromEntries(Array.from({ length: 65 }, (_, i) => ['x' + i, 'a'])) }],
        error: /HTTP metadata limit/ },
    { name: 'HTTP header byte limit', chunks: [{ data: '', headers: { x: 'x'.repeat(65536) } }], error: /HTTP metadata limit/ },
    { name: 'HTTP handle byte limit', chunks: [{ data: '', handle: 'x'.repeat(1025) }], closeHandle: 'x'.repeat(1025),
        error: /HTTP metadata limit/ },
    { name: 'close failure is reported after successful decode', chunks: [text + done], closeError: true,
        error: /close_failed: close failed/, expected: [delta, finished] },
    { name: 'close failure never masks first parse error', chunks: ['data: {oops}\n'], closeError: true,
        error: /key must be a string/ },
    { name: 'close failure never masks first read error', chunks: ['', { response: { Err: { code: 'read_failed', message: 'broken stream' } } }],
        closeError: true, error: /read_failed: broken stream/ },
    { name: 'missing call id at terminal', chunks: [tools([{ index: 0, function: { name: 'weather', arguments: '{}' } }], 'tool_calls') + done],
        error: /incomplete tool call/ },
    { name: 'missing call name at terminal', chunks: [tools([{ index: 0, id: 'a', function: { arguments: '{}' } }], 'tool_calls') + done],
        error: /incomplete tool call/ },
    { name: 'missing arguments at terminal', chunks: [tools([{ index: 0, id: 'a', function: { name: 'weather' } }], 'tool_calls') + done],
        error: /EOF while parsing/ },
    { name: 'complete call at EOF still needs finish reason', chunks: [{ data: tools([call()]), done: true }], closes: 0,
        error: /without finish reason/ },
    { name: 'incomplete call at EOF without reason fails', chunks: [{ data: tools([call(0, 'a', 'weather', '{')]), done: true }], closes: 0,
        error: /without finish reason/ },
    { name: 'unterminated continuation after reason fails at EOF', chunks: [{ data: complete + 'data: {', done: true }], closes: 0,
        error: /incomplete tool SSE line/ },
    { name: 'unindexed continuation is ambiguous', chunks: [tools([call(0, 'a', 'weather', '{')]),
        tools([{ function: { arguments: '}' } }], 'tool_calls')], error: /ambiguous unindexed/ },
    { name: 'unindexed partial arguments are rejected', chunks: [tools([{ id: 'a', function: { name: 'weather', arguments: '{' } }], 'tool_calls')],
        error: /EOF while parsing/ },
    { name: 'conflicting id for one index', chunks: [tools([call(0, 'a')]), tools([{ index: 0, id: 'b' }], 'tool_calls')],
        error: /conflicting tool id/ },
    { name: 'duplicate final ids invalidate all siblings', chunks: [tools([call(0, 'a'), call(1, 'a')], 'tool_calls') + done],
        error: /duplicate tool id/ },
    { name: 'duplicate unindexed call ids', chunks: [tools([{ id: 'a', function: { name: 'weather', arguments: '{}' } },
        { id: 'a', function: { name: 'weather', arguments: '{}' } }], 'tool_calls') + done], error: /duplicate unindexed tool call/ },
    { name: 'duplicate tool index within event', chunks: [tools([call(), call()], 'tool_calls') + done], error: /duplicate tool index/ },
    { name: 'duplicate choice index within event', chunks: [sse([choice({}), choice({})])], error: /duplicate choice index/ },
    { name: 'missing choice index amid parallel calls', chunks: [sse([{ delta: { tool_calls: [call()] } }, { delta: {} }])],
        error: /ambiguous choice index/ },
    { name: 'missing choice index after indexed call', chunks: [tools([call()], null, 5), sse([{ delta: {}, finish_reason: 'tool_calls' }])],
        error: /ambiguous choice index/ },
    { name: 'conflicting finish reason', chunks: [complete, sse([choice({}, 'stop')])], error: /conflicting finish reason/ },
    { name: 'choice count limit within event', chunks: [sse(Array.from({ length: 33 }, (_, i) => choice({}, null, i)))], error: /choice limit/ },
    { name: 'choice count limit across events', chunks: [Array.from({ length: 33 }, (_, i) => sse([choice({}, null, i)])).join('')],
        error: /choice limit/ },
    { name: 'bounded parallel calls remain supported at capacity', chunks: [tools(
        Array.from({ length: 64 }, (_, i) => call(i, 'c' + i)), 'tool_calls') + done],
        expected: [...Array.from({ length: 64 }, (_, i) => toolEvent('c' + i)), toolFinished] },
    { name: 'bounded indexed choices remain supported at capacity', chunks: [sse(Array.from({ length: 32 }, (_, i) =>
        choice({ tool_calls: [call(0, 'c' + i)] }, 'tool_calls', i))) + done],
        expected: [...Array.from({ length: 32 }, (_, i) => toolEvent('c' + i)), toolFinished] },
    { name: '65 calls within event emit no calls', chunks: [tools(Array.from({ length: 65 }, (_, i) => call(i, 'c' + i)), 'tool_calls')],
        error: /tool call limit/ },
    { name: '65 calls across choices emit no calls', chunks: [sse([0, 1].map(i => choice({ tool_calls:
        Array.from({ length: 32 + i }, (_, j) => call(j, `${i}-${j}`)) }, 'tool_calls', i)))], error: /tool call limit/ },
    { name: '65 calls across events emit no calls', chunks: [tools(Array.from({ length: 64 }, (_, i) => call(i, 'c' + i))),
        tools([call(64, 'c64')], 'tool_calls')], beforeRead: [[]], error: /tool call limit/ },
    { name: 'exact 256 byte tool metadata is valid', chunks: [tools([call(0, 'i'.repeat(256), 'n'.repeat(256))], 'tool_calls') + done],
        expected: [toolEvent('i'.repeat(256), 'n'.repeat(256)), toolFinished] },
    { name: 'exact 256 byte fragmented UTF8 tool metadata is valid', chunks: [tools([call(0, 'é'.repeat(128), 'é'.repeat(100), '')]),
        tools([{ index: 0, function: { name: 'é'.repeat(28), arguments: '{}' } }], 'tool_calls') + done],
        beforeRead: [[]], expected: [toolEvent('é'.repeat(128), 'é'.repeat(128)), toolFinished] },
    { name: '257 byte id invalidates valid first sibling', chunks: [complete,
        tools([call(1, 'i'.repeat(257))], 'tool_calls')], beforeRead: [[]], error: /tool metadata limit/ },
    { name: '257 byte fragmented name invalidates valid first sibling', chunks: [tools([call(), call(1, 'a', 'n'.repeat(250), '')]),
        tools([{ index: 1, function: { name: 'n'.repeat(7), arguments: '{}' } }], 'tool_calls')],
        beforeRead: [[]], error: /tool metadata limit/ },
    { name: 'exact 128 byte finish reason is valid before calls', chunks: [tools([call()], 'r'.repeat(128)) + done],
        expected: [toolEvent(), { type: 'finished', reason: 'r'.repeat(128) }] },
    { name: 'exact 128 byte UTF8 finish reason is valid', chunks: [text.replace('stop', 'é'.repeat(64)) + done],
        expected: [delta, { type: 'finished', reason: 'é'.repeat(64) }] },
    { name: '129 byte finish reason emits no calls', chunks: [tools([call()], 'r'.repeat(129)) + done], error: /finish reason limit/ },
    { name: '129 byte finish reason after valid first choice emits no calls', chunks: [complete,
        sse([choice({ tool_calls: [call(0, 'second')] }, 'r'.repeat(129), 1)]) + done],
        beforeRead: [[]], error: /finish reason limit/ },
    { name: '129 byte text finish reason is invalid', chunks: [sse([choice({}, 'r'.repeat(129))])], error: /finish reason limit/ },
    { name: 'malformed model JSON at EOF never closes', models: true, chunks: [{ data: '{', done: true }], closes: 0,
        error: /EOF while parsing/ },
    { name: 'missing models at EOF never closes', models: true, chunks: [{ data: '{}', done: true }], closes: 0,
        error: /missing models/ },
    { name: '32769 models exceed native count bound', models: true, chunks: [{ data: JSON.stringify({ data: Array(32769).fill({ id: 'x' }) }), done: true }],
        closes: 0, error: /model count limit/ },
    { name: '32768 models still obey final event byte bound', models: true, chunks: [{ data: JSON.stringify({ data: Array(32768).fill({ id: 'x' }) }), done: true }],
        closes: 0, error: /model event limit/ },
    { name: 'model metadata above 1024 bytes remains valid', models: true, chunks: [{ data: JSON.stringify({ data: [{ id: 'i'.repeat(1025), name: 'n'.repeat(1025) }] }), done: true }],
        closes: 0, checkEvents(events) {
            assert.equal(events.length, 1);
            assert.equal(events[0].type, 'models');
            assert.equal(events[0].models.length, 1);
            assert.equal(events[0].models[0].id, 'i'.repeat(1025));
            assert.equal(events[0].models[0].name, 'n'.repeat(1025));
        } },
    { name: 'valid catalogue preserves every model', models: true, chunks: [{ data: JSON.stringify({ data:
        Array.from({ length: 1000 }, (_, i) => ({ id: 'model-' + i })) }), done: true }], closes: 0,
        checkEvents(events) {
            assert.equal(events.length, 1);
            assert.equal(events[0].type, 'models');
            assert.deepEqual(events[0].models.map(model => model.id), Array.from({ length: 1000 }, (_, i) => 'model-' + i));
            assert.deepEqual(events[0].models.map(model => model.name), Array.from({ length: 1000 }, (_, i) => 'model-' + i));
        } },
    { name: 'oversized catalogue fails instead of truncating', models: true, chunks: [{ data: JSON.stringify({ data:
        Array.from({ length: 4097 }, (_, i) => ({ id: 'model-' + i })) }), done: true }],
        closes: 0, error: /model event limit/ },
    { name: 'model retained bytes across reads are bounded', models: true,
        chunks: ['x'.repeat(4 * 1024 * 1024), { data: 'x'.repeat(4 * 1024 * 1024 + 1), done: true }],
        closes: 0, error: /response limit/ },
];

for (const [label, value] of [['empty', ''], ['malformed', '{oops}'], ['truncated', '{"city":'],
    ['scalar', '1'], ['string', '"text"'], ['array', '[]'], ['null', 'null'], ['whitespace', ' ']]) {
    cases.push({ name: `${label} arguments invalidate every sibling`, chunks: [tools([
        call(0, 'valid', 'weather', '{}'), call(1, 'invalid', 'clock', value),
    ], 'tool_calls') + done], error: /EOF while parsing|key must be a string|expected value|must be an object/ });
}
for (const [label, value] of [['negative', -1], ['fractional', 0.5], ['string', '0'], ['null', null]]) {
    cases.push({ name: `${label} tool index`, chunks: [tools([call(value)], 'tool_calls')], error: /invalid tool index/ });
    cases.push({ name: `${label} choice index`, chunks: [sse([choice({}, 'stop', value)])], error: /invalid choice index/ });
}
for (const [label, value] of [['tool calls object', { tool_calls: {} }], ['tool entry scalar', { tool_calls: [1] }],
    ['tool function scalar', { tool_calls: [{ index: 0, function: 1 }] }],
    ['tool arguments object', { tool_calls: [{ index: 0, function: { arguments: {} } }] }],
    ['tool name scalar', { tool_calls: [{ index: 0, function: { name: 1 } }] }],
    ['tool id scalar', { tool_calls: [{ index: 0, id: 1 }] }],
    ['tool type mismatch', { tool_calls: [{ index: 0, type: 'other' }] }]]) {
    cases.push({ name: label, chunks: [sse([choice(value, 'tool_calls')])], error: /invalid tool/ });
}
const eventLimit = 128 * 1024;
const exactArguments = JSON.stringify({ data: 'x'.repeat(eventLimit - Buffer.byteLength(JSON.stringify(toolEvent('second', 'weather', { data: '' })))) });
const exactEvent = toolEvent('second', 'weather', JSON.parse(exactArguments));
assert.equal(Buffer.byteLength(JSON.stringify(exactEvent)), eventLimit);
cases.push({ name: 'exact 128 KiB serialized tool event is valid', chunks: brokerChunks(
    tools([call(0, 'second', 'weather', exactArguments)], 'tool_calls') + done), expected: [exactEvent, toolFinished] });
const oversizedArguments = JSON.stringify({ data: JSON.parse(exactArguments).data + 'x' });
assert.equal(Buffer.byteLength(JSON.stringify(toolEvent('second', 'weather', JSON.parse(oversizedArguments)))), eventLimit + 1);
const oversizedToolChunks = [complete, ...brokerChunks(tools([call(1, 'second', 'weather', oversizedArguments)], 'tool_calls') + done)];
cases.push({ name: 'one byte oversized tool event invalidates valid first sibling', chunks: oversizedToolChunks,
    beforeRead: oversizedToolChunks.slice(1).map(() => []), error: /tool event limit/ });
cases.push({ name: 'escaped oversized tool event invalidates valid first sibling', chunks: brokerChunks(tools([
    call(), call(1, 'second', 'weather', JSON.stringify({ data: '\u0000'.repeat(Math.ceil(eventLimit / 6)) })),
], 'tool_calls') + done), error: /tool event limit/ });
const utf8Arguments = { data: 'é'.repeat(Math.floor((eventLimit - Buffer.byteLength(JSON.stringify(toolEvent('second', 'weather', { data: '' })))) / 2)) };
utf8Arguments.data += 'x'.repeat(eventLimit - Buffer.byteLength(JSON.stringify(toolEvent('second', 'weather', utf8Arguments))));
assert.equal(Buffer.byteLength(JSON.stringify(toolEvent('second', 'weather', utf8Arguments))), eventLimit);
cases.push({ name: 'UTF8 tool event at byte bound is valid', chunks: brokerChunks(
    tools([call(0, 'second', 'weather', JSON.stringify(utf8Arguments))], 'tool_calls') + done),
    expected: [toolEvent('second', 'weather', utf8Arguments), toolFinished] });
const whitespaceArguments = ' '.repeat(8 * 1024 * 1024 - 256) + '{}';
const whitespaceChunks = Array.from({ length: Math.ceil(whitespaceArguments.length / 600000) }, (_, index) => {
    const arguments = whitespaceArguments.slice(index * 600000, (index + 1) * 600000);
    return tools([index === 0 ? call(0, 'call-1', 'weather', arguments) : { index: 0, function: { arguments } }]);
});
cases.push({ name: 'one call retains valid whitespace arguments near 8 MiB', chunks: [...whitespaceChunks,
    sse([choice({}, 'tool_calls')]) + done], beforeRead: whitespaceChunks.map(() => []), expected: [toolEvent(), toolFinished] });
const modelAtLimit = { id: 'i', name: 'n', metadata: { source: 'provider', scope: 'model' } };
modelAtLimit.name = 'n'.repeat(eventLimit - Buffer.byteLength(JSON.stringify({ type: 'models', models: [modelAtLimit] })) + 1);
const modelEventAtLimit = { type: 'models', models: [modelAtLimit] };
assert.equal(Buffer.byteLength(JSON.stringify(modelEventAtLimit)), eventLimit);
cases.push({ name: 'exact 128 KiB serialized model event is valid', models: true, chunks:
    brokerChunks(JSON.stringify({ data: [{ id: modelAtLimit.id, name: modelAtLimit.name }] }))
        .map((data, index, chunks) => ({ data, done: index === chunks.length - 1 })), closes: 0,
    expected: [modelEventAtLimit] });
cases.push({ name: 'one byte oversized model event fails without truncation', models: true, chunks:
    brokerChunks(JSON.stringify({ data: [{ id: modelAtLimit.id, name: modelAtLimit.name + 'n' }] }))
        .map((data, index, chunks) => ({ data, done: index === chunks.length - 1 })), closes: 0,
    error: /model event limit/ });
const exactDeltaText = 'x'.repeat(eventLimit - Buffer.byteLength(JSON.stringify({ type: 'delta', text: '' })));
assert.equal(Buffer.byteLength(JSON.stringify({ type: 'delta', text: exactDeltaText })), eventLimit);
cases.push({ name: 'exact 128 KiB serialized delta event is valid', chunks: brokerChunks(
    sse([choice({ content: exactDeltaText }, 'stop')]) + done), expected: [{ type: 'delta', text: exactDeltaText }, finished] });
cases.push({ name: 'one byte oversized delta event closes live handle before calls', chunks: [complete,
    ...brokerChunks(sse([choice({ content: exactDeltaText + 'x' })]))], error: /delta event limit/ });
cases.push({ name: 'one byte oversized delta event at EOF is not emitted', chunks:
    brokerChunks(sse([choice({ content: exactDeltaText + 'x' })]))
        .map((data, index, chunks) => ({ data, done: index === chunks.length - 1 })), closes: 0, error: /delta event limit/ });
const largeArguments = JSON.stringify({ data: 'x'.repeat(1024 * 1024 - 100) });
cases.push({ name: 'aggregate retained arguments are bounded', chunks: Array.from({ length: 9 }, (_, index) => [
    tools([call(index, 'c' + index, 'weather', largeArguments.slice(0, 600000))]),
    tools([{ index, function: { arguments: largeArguments.slice(600000) } }]),
]).flat(), error: /pending tool bytes limit/, allowUnused: true });

(async () => {
    const wasm = readFileSync(process.argv[2]);
    const selected = process.env.OPENROUTER_CASE ? cases.filter(test => test.name.includes(process.env.OPENROUTER_CASE)) : cases;
    assert.ok(selected.length, 'case filter must select at least one fixture');
    for (const test of selected) {
        const events = [];
        const phases = [];
        const chunks = [...(test.chunks || [])];
        let reads = 0;
        let eof = false;
        const plugin = await createPlugin({ wasm: [{ data: wasm }] }, {
            useWasi: false,
            functions: {
                'extism:host/user': {
                    ene_service(cp, offset) {
                        const service = cp.read(offset).json();
                        phases.push(service.type);
                        if (service.type === 'http_open') {
                            assert.equal(service.request.method, test.models ? 'GET' : 'POST');
                            assert.equal(service.request.url, endpoint + (test.models ? '/models?providers=openai' : '/chat/completions'));
                            assert.deepEqual(service.request.headers, { 'content-type': 'application/json' });
                            if (test.models) assert.equal(service.request.body_base64, null);
                            else assert.deepEqual(JSON.parse(Buffer.from(service.request.body_base64, 'base64')), test.payload || payload);
                            if (test.openResponse) return cp.store(JSON.stringify(test.openResponse));
                        } else if (service.type === 'http_close') {
                            assert.equal(eof, false, 'EOF already released the handle');
                            assert.equal(service.handle, test.closeHandle || 'stream');
                            assert.equal(phases.filter(phase => phase === 'http_close').length, 1, 'close exactly once');
                            return cp.store(JSON.stringify(test.closeError
                                ? { Err: { code: 'close_failed', message: 'close failed' } } : { Ok: null }));
                        } else {
                            assert.equal(service.type, 'http_read');
                            assert.equal(service.handle, 'stream');
                            assert.equal(eof, false, 'never read after EOF');
                            if (test.beforeRead) assert.deepEqual(events, test.beforeRead[reads]);
                            reads++;
                        }
                        assert.ok(chunks.length, 'read after terminal event would block');
                        const next = chunks.shift();
                        const chunk = typeof next === 'string' || Buffer.isBuffer(next) ? { data: next } : next;
                        if (chunk.response) return cp.store(JSON.stringify(chunk.response));
                        eof = chunk.done || false;
                        return cp.store(JSON.stringify({ Ok: {
                            handle: chunk.handle || 'stream', status: chunk.status || 200, headers: chunk.headers || {}, done: eof,
                            data_base64: chunk.base64 === undefined ? Buffer.from(chunk.data || '').toString('base64') : chunk.base64,
                        } }));
                    },
                    ene_emit(cp, offset) { events.push(cp.read(offset).json()); },
                },
            },
        });
        try {
            const capabilities = await plugin.call('ene_initialize', '');
            assert.deepEqual(capabilities.json(), { streaming_text: true, model_list: true, tool_calls: true });
            const invoke = () => plugin.call('ene_invoke', JSON.stringify(test.models
                ? { type: 'list_models', endpoint: endpoint + '/', options: {} }
                : { type: 'generate', request: test.request || request }));
            if (test.error) await assert.rejects(invoke, test.error);
            else await invoke();
            if (!test.allowUnused) assert.equal(chunks.length, 0, 'consume the selected transport chunks');
            assert.equal(phases.filter(phase => phase === 'http_close').length, test.closes === undefined ? 1 : test.closes);
            if (test.phases) assert.deepEqual(phases, test.phases);
            if (test.checkEvents) test.checkEvents(events);
            else assert.deepEqual(events, test.expected || (test.error ? [] : [delta, finished]));
            console.log(`PASS: ${test.name}`);
        } catch (error) {
            error.message = `${test.name}: ${error.message}`;
            throw error;
        } finally {
            await plugin.close();
        }
    }
    console.log(`PASS: ${selected.length} actual-Wasm provider fixtures`);
})().catch(error => { console.error(error); process.exitCode = 1; });
