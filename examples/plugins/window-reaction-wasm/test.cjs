// Uses the same Extism JS host as the OpenRouter sample test.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const createPlugin = require('@extism/extism');

(async () => {
    const manifest = JSON.parse(readFileSync(join(__dirname, 'manifest.json'), 'utf8'));
    const subscription = manifest.entrypoints[0].window_events[0];
    const events = [];
    const plugin = await createPlugin({ wasm: [{ data: readFileSync(process.argv[2]) }] }, {
        useWasi: false,
        functions: { 'extism:host/user': {
            ene_emit(cp, offset) { events.push(cp.read(offset).json()); },
            ene_service() { throw new Error('This example needs no host service calls'); },
        } },
    });
    const operation = {
        type: 'character',
        target: { device_id: 'desktop-a', character_id: 'ene-2' },
        event: { type: 'window', event: {
            sequence: 7, kind: subscription.events[0], window: {
                id: { device_id: 'desktop-a', generation: 3, id: 'window-42' },
                application: subscription.applications[0],
            },
        } },
    };
    try {
        await plugin.call('ene_initialize');
        await plugin.call('ene_invoke', JSON.stringify(operation));
        assert.deepEqual(events, [{ type: 'character', target: operation.target,
            action: { type: 'expression', name: 'happy', duration_ms: 1200 } }]);
        for (const change of [
            input => { input.event.event.window.application = 'Other.exe'; },
            input => { input.event.event.kind = 'closed'; },
            input => { input.event.event.window.id.device_id = 'desktop-b'; },
            input => { input.event = { type: 'activated' }; },
        ]) {
            const input = structuredClone(operation);
            change(input);
            await plugin.call('ene_invoke', JSON.stringify(input));
            assert.equal(events.length, 1, 'unrelated event must not trigger a reaction');
        }
        delete operation.target;
        await assert.rejects(() => plugin.call('ene_invoke', JSON.stringify(operation)));
        assert.equal(events.length, 1);
    } finally {
        await plugin.close();
    }
    console.log('PASS: window event -> targeted character action through the Wasm SDK');
})().catch(error => { console.error(error); process.exitCode = 1; });
