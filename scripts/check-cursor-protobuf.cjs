#!/usr/bin/env node
'use strict';

// Validate synthetic Cursor native state with the protobuf implementation from
// the installed Cursor Agent bundle. This deliberately loads only the generated
// schema module. It removes the bundle's main-entry tail and disables dynamic
// module loading before evaluating the webpack runtime.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const vm = require('node:vm');

const mainEntry = 'var __webpack_exports__=__webpack_require__("./src/main.tsx")})();';
const isolatedEntry = '__webpack_require__.p="";__webpack_require__.b=undefined;__webpack_require__.f.require=()=>{throw new Error("dynamic module loading disabled")};globalThis.__cursorRequire=__webpack_require__})();';
const maxInputBytes = 8 * 1024 * 1024;
const maxBundleBytes = 64 * 1024 * 1024;

function reject(reason, index) {
  const suffix = Number.isInteger(index) ? ` index=${index}` : '';
  process.stderr.write(`validation_failed${suffix} reason=${reason}\n`);
  process.exit(1);
}

function loadConversationStateStructure(binary) {
  const executable = fs.realpathSync(binary);
  const bundlePath = path.join(path.dirname(executable), 'index.js');
  const bundleStat = fs.statSync(bundlePath);
  if (!bundleStat.isFile() || bundleStat.size > maxBundleBytes) throw new Error('invalid installed bundle');
  const source = fs.readFileSync(bundlePath, 'utf8');
  const entryOffset = source.lastIndexOf(mainEntry);
  const startupOffset = source.lastIndexOf('__webpack_require__.p=');
  if (entryOffset < 0 || entryOffset !== source.indexOf(mainEntry) || !source.endsWith(mainEntry)) {
    throw new Error('unsupported installed Cursor entry shape');
  }
  if (startupOffset < 0 || startupOffset >= entryOffset) {
    throw new Error('unsupported installed Cursor runtime shape');
  }
  const isolatedSource = source.slice(0, startupOffset) + isolatedEntry;
  const sandbox = {
    Buffer,
    TextEncoder,
    TextDecoder,
    process: Object.freeze({
      env: Object.freeze({}),
      platform: process.platform,
      arch: process.arch,
      versions: Object.freeze({ node: process.versions.node }),
    }),
  };
  vm.createContext(sandbox, { codeGeneration: { strings: false, wasm: false } });
  vm.runInContext(isolatedSource, sandbox, { filename: 'cursor-agent-schema-only.js', timeout: 10000 });
  if (typeof sandbox.__cursorRequire !== 'function') throw new Error('isolated Cursor loader unavailable');
  const agentProto = sandbox.__cursorRequire('../proto/dist/generated/agent/v1/agent_pb.js');
  if (typeof agentProto.Y9 !== 'function') throw new Error('ConversationStateStructure class unavailable');
  return { Message: agentProto.Y9, Uint8Array: vm.runInContext('Uint8Array', sandbox) };
}

function decodeBase64(value, index) {
  if (typeof value !== 'string' || value.length === 0 || value.length > maxInputBytes * 2) {
    reject('invalid_base64', index);
  }
  if (!/^[A-Za-z0-9+/_-]*={0,2}$/.test(value) || value.replace(/=+$/, '').length % 4 === 1) {
    reject('invalid_base64', index);
  }
  const normalized = value.replace(/-/g, '+').replace(/_/g, '/');
  const bytes = Buffer.from(normalized, 'base64');
  if (bytes.toString('base64').replace(/=+$/, '') !== normalized.replace(/=+$/, '')) {
    reject('invalid_base64', index);
  }
  return bytes;
}

function promptText(prompt, index) {
  if (!prompt || typeof prompt !== 'object' || !['system', 'user', 'assistant'].includes(prompt.role)) {
    reject('invalid_prompt_message', index);
  }
  if (typeof prompt.content === 'string') return prompt.content;
  if (!Array.isArray(prompt.content)) reject('invalid_prompt_content', index);
  const text = prompt.content.map((part) => {
    if (!part || typeof part !== 'object' || typeof part.text !== 'string') {
      reject('invalid_prompt_text_part', index);
    }
    return part.text;
  }).join('');
  return text;
}

let entries;
try {
  const input = fs.readFileSync(0, 'utf8');
  if (Buffer.byteLength(input, 'utf8') > maxInputBytes) reject('input_too_large');
  entries = JSON.parse(input);
} catch {
  reject('invalid_json');
}
if (!Array.isArray(entries) || entries.length !== 6) reject('invalid_entry_count');
if (process.argv.length !== 3) reject('cursor_binary_required');

let cursor;
try {
  cursor = loadConversationStateStructure(process.argv[2]);
} catch (error) {
  if (error instanceof Error && error.message.startsWith('unsupported installed Cursor')) {
    reject('unsupported_cursor_bundle_shape');
  }
  reject('cursor_schema_load_failed');
}

const expectedLabels = new Set([
  'cursor-cli-root', 'cursor-cli-rewind-1', 'cursor-cli-rewind-2',
  'cursor-ide-root', 'cursor-ide-rewind-1', 'cursor-ide-rewind-2',
]);
const seenLabels = new Set();
for (const [index, entry] of entries.entries()) {
  if (!entry || typeof entry !== 'object' || Array.isArray(entry)) reject('invalid_entry', index);
  if (!expectedLabels.has(entry.label) || seenLabels.has(entry.label)) reject('invalid_label', index);
  if (entry.kind !== (entry.label.endsWith('-root') ? 'root' : 'rewind')) reject('invalid_state_kind', index);
  seenLabels.add(entry.label);
  const expectedTurns = entry.expectedTurns;
  const expectedTexts = entry.expectedTexts;
  if (!Number.isInteger(expectedTurns) || expectedTurns < 0 || expectedTurns > 1024) reject('invalid_turn_count', index);
  if (!Array.isArray(expectedTexts) || expectedTexts.length > 2049 || expectedTexts.some((text) => typeof text !== 'string')) {
    reject('invalid_expected_texts', index);
  }
  const bytes = decodeBase64(entry.bytes, index);
  if (bytes.length > maxInputBytes) reject('blob_too_large', index);
  let message;
  try {
    message = cursor.Message.fromBinary(new cursor.Uint8Array(bytes));
  } catch {
    reject('protobuf_decode_failed', index);
  }
  if (typeof message.conversationStartedTimestampMs !== 'bigint' || message.conversationStartedTimestampMs <= 0n) {
    reject('timestamp_missing_or_invalid', index);
  }
  if (message.conversationStartedTimeZone !== 'UTC') reject('timezone_not_utc', index);
  if (message.mode !== 1) reject('mode_missing_or_invalid', index);
  if (!Array.isArray(message.turns) || message.turns.length !== expectedTurns) reject('turn_count_mismatch', index);
  if (!Array.isArray(message.rootPromptMessagesJson)) reject('prompt_list_missing', index);
  if (!Array.isArray(entry.promptBlobs)) reject('prompt_blob_map_missing', index);
  const promptBlobs = new Map();
  for (const promptBlob of entry.promptBlobs) {
    if (!promptBlob || typeof promptBlob.id !== 'string' || promptBlobs.has(promptBlob.id)) reject('invalid_prompt_blob', index);
    promptBlobs.set(promptBlob.id, decodeBase64(promptBlob.bytes, index));
  }
  const prompts = message.rootPromptMessagesJson.map((reference, promptIndex) => {
    const id = Buffer.from(reference).toString('hex');
    if (id.length !== 64) reject('invalid_prompt_blob_reference', index);
    const promptBytes = promptBlobs.get(id);
    if (!promptBytes) reject('prompt_blob_missing', index);
    const digest = crypto.createHash('sha256').update(promptBytes).digest('hex');
    if (digest !== id) reject('prompt_blob_hash_mismatch', index);
    try {
      return JSON.parse(promptBytes.toString('utf8'));
    } catch {
      reject('prompt_json_invalid', index);
    }
  });
  if (prompts.length !== expectedTexts.length + 1 || prompts[0]?.role !== 'system') {
    reject('prompt_root_shape_mismatch', index);
  }
  if (prompts.slice(1).some((prompt) => prompt?.role === 'system')) reject('unexpected_system_prompt', index);
  const promptTexts = prompts.filter((prompt) => prompt?.role !== 'system').map((prompt, promptIndex) => promptText(prompt, promptIndex));
  if (promptTexts.length !== expectedTexts.length) reject('prompt_count_mismatch', index);
  if (expectedTexts.some((text, textIndex) => promptTexts[textIndex] !== text)) reject('prompt_text_mismatch', index);
  if (prompts.slice(1).some((prompt, promptIndex) => prompt?.role !== (promptIndex % 2 === 0 ? 'user' : 'assistant'))) {
    reject('prompt_role_order_mismatch', index);
  }
}
if (seenLabels.size !== expectedLabels.size) reject('entry_labels_incomplete');
process.stdout.write(JSON.stringify({ decoded: entries.length, providers: 2, roots: 2, rewindAnchors: 4, schema: 'installed-cursor-agent' }) + '\n');
