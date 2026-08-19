#!/usr/bin/env node
import { readFileSync, existsSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const PLUGIN_ROOT = process.env.ZCODE_PLUGIN_ROOT || __dirname.replace(/\/hooks$/, '');

const skillPath = join(PLUGIN_ROOT, 'skills', 'using-superpowers', 'SKILL.md');
if (!existsSync(skillPath)) {
  process.stderr.write(`[superpowers] SKILL.md not found at ${skillPath}\n`);
  process.stdout.write('{}\n');
  process.exit(0);
}

const raw = readFileSync(skillPath, 'utf8');
const contentMatch = raw.match(/^---\n([\s\S]*?)\n---\n([\s\S]*)$/);
const content = contentMatch ? contentMatch[2].trim() : raw.trim();

const escapeJson = (s) =>
  s.replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n').replace(/\r/g, '\\r').replace(/\t/g, '\\t');

const toolMapping = `**Tool Mapping for ZCode:**
- Ask user questions → \`AskUserQuestion\` tool
- Create/track todos → \`TodoWrite\` tool
- Dispatch subagent → \`Agent\` tool
- Invoke a skill → \`Skill\` tool
- Read files → \`Read\` tool
- Create/edit/delete files → \`Write\`, \`Edit\` tools
- Run shell commands → \`Bash\` tool
- Search file contents → \`Grep\` tool
- Find files by path/pattern → \`Glob\` tool
- Fetch a URL → \`WebFetch\` tool
- Search the web → \`WebSearch\` tool`;

const bootstrap = `<EXTREMELY_IMPORTANT>
You have superpowers.

**IMPORTANT: The using-superpowers skill content is included below. It is ALREADY LOADED — you are currently following it. Do NOT use the skill tool to load "using-superpowers" again — that would be redundant.**

${content}

${toolMapping}
</EXTREMELY_IMPORTANT>`;

let input = {};
try {
  const rawInput = process.stdin.read();
  if (rawInput?.trim()) input = JSON.parse(rawInput.trim());
} catch {}

const eventName = input.hook_event_name || input.hookEventName || 'SessionStart';

process.stdout.write(JSON.stringify({
  hookSpecificOutput: {
    hookEventName: eventName,
    additionalContext: escapeJson(bootstrap),
  },
}));
