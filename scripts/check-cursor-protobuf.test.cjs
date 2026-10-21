'use strict';

const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { test } = require('node:test');

const checkerPath = path.resolve(process.env.CURSOR_PROTOBUF_CHECKER || path.join(__dirname, 'check-cursor-protobuf.cjs'));
const typeName = 'agent.v1.ConversationStateStructure';
const validFields = [
  { no: 1, name: 'root_prompt_messages_json', localName: 'rootPromptMessagesJson', kind: 'scalar', T: 12, repeated: true },
  { no: 8, name: 'turns', localName: 'turns', kind: 'scalar', T: 12, repeated: true },
  { no: 10, name: 'mode', localName: 'mode', kind: 'enum', T: { typeName: 'agent.v1.AgentMode' }, repeated: false },
  { no: 26, name: 'conversation_started_timestamp_ms', localName: 'conversationStartedTimestampMs', kind: 'scalar', T: 4, repeated: false },
  { no: 27, name: 'conversation_started_time_zone', localName: 'conversationStartedTimeZone', kind: 'scalar', T: 9, repeated: false },
];

function fixture(t, exportedTypes) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'omni-cursor-protobuf-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));

  const binary = path.join(root, 'cursor-agent');
  fs.writeFileSync(binary, 'synthetic executable path; the checker reads only index.js');
  const declarations = exportedTypes.map((item, index) => `
    class Message${index} {
      static typeName = ${JSON.stringify(item.typeName)};
      static fields = { list: () => ${JSON.stringify(item.fields)} };
      static fromBinary(bytes) {
        const state = JSON.parse(Buffer.from(bytes).toString('utf8'));
        return {
          conversationStartedTimestampMs: BigInt(state.timestamp),
          conversationStartedTimeZone: state.timeZone,
          mode: state.mode,
          turns: state.turns,
          rootPromptMessagesJson: state.rootPromptMessagesJson.map((id) => Buffer.from(id, 'hex')),
        };
      }
    }
    exports[${JSON.stringify(item.exportName)}] = Message${index};`).join('\n');
  const mainEntry = 'var __webpack_exports__=__webpack_require__("./src/main.tsx")})();';
  const isolatedPrelude = `(function() {
    const modules = {
      "../proto/dist/generated/agent/v1/agent_pb.js": function(module) {
        const exports = {};
        ${declarations}
        module.exports = exports;
      },
    };
    const cache = {};
    function __webpack_require__(id) {
      if (cache[id]) return cache[id].exports;
      const module = { exports: {} };
      cache[id] = module;
      modules[id](module);
      return module.exports;
    }
    __webpack_require__.f = {};
    __webpack_require__.p="";
    ${mainEntry}`;
  fs.writeFileSync(path.join(root, 'index.js'), isolatedPrelude);

  const promptBytes = Buffer.from(JSON.stringify({ role: 'system', content: 'synthetic schema fixture' }));
  const promptId = crypto.createHash('sha256').update(promptBytes).digest('hex');
  const state = Buffer.from(JSON.stringify({
    timestamp: '1',
    timeZone: 'UTC',
    mode: 1,
    turns: [],
    rootPromptMessagesJson: [promptId],
  }));
  const labels = [
    'cursor-cli-root', 'cursor-cli-rewind-1', 'cursor-cli-rewind-2',
    'cursor-ide-root', 'cursor-ide-rewind-1', 'cursor-ide-rewind-2',
  ];
  const entries = labels.map((label) => ({
    label,
    kind: label.endsWith('-root') ? 'root' : 'rewind',
    bytes: state.toString('base64'),
    expectedTurns: 0,
    expectedTexts: [],
    promptBlobs: [{ id: promptId, bytes: promptBytes.toString('base64') }],
  }));
  return { binary, entries };
}

function runChecker(binary, entries) {
  return spawnSync(process.execPath, [checkerPath, binary], {
    encoding: 'utf8',
    input: JSON.stringify(entries),
    timeout: 10_000,
  });
}

function exportedType(exportName, fields = validFields, name = typeName) {
  return { exportName, fields, typeName: name };
}

test('resolves ConversationStateStructure when Cursor renames its generated export', (t) => {
  const input = fixture(t, [exportedType('Y9z')]);
  const result = runChecker(input.binary, input.entries);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), {
    decoded: 6,
    providers: 2,
    roots: 2,
    rewindAnchors: 4,
    schema: 'installed-cursor-agent',
  });
});

test('continues to accept the older generated export alias', (t) => {
  const input = fixture(t, [exportedType('Y9')]);
  const result = runChecker(input.binary, input.entries);
  assert.equal(result.status, 0, result.stderr);
});

test('rejects an absent ConversationStateStructure descriptor', (t) => {
  const input = fixture(t, [exportedType('Y9z', validFields, 'agent.v1.OtherMessage')]);
  const result = runChecker(input.binary, input.entries);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /validation_failed reason=cursor_schema_load_failed/);
});

test('rejects multiple distinct classes claiming the conversation type', (t) => {
  const input = fixture(t, [exportedType('Y9'), exportedType('Y9z')]);
  const result = runChecker(input.binary, input.entries);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /validation_failed reason=cursor_schema_load_failed/);
});

test('rejects a conversation descriptor whose required wire contract changed', (t) => {
  const incompatibleFields = validFields.map((field) => field.no === 26 ? { ...field, T: 5 } : field);
  const input = fixture(t, [exportedType('Y9z', incompatibleFields)]);
  const result = runChecker(input.binary, input.entries);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /validation_failed reason=cursor_schema_load_failed/);
});
