import { describe, expect, test } from 'bun:test';

import { hasLinkDefinition, IncrementalMarkdown, parseBlocks } from './parse';

/** Strip mdast positions so tail-relative parses compare equal. */
function shape(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(shape);
  if (value && typeof value === 'object') {
    const out: Record<string, unknown> = {};
    for (const [key, entry] of Object.entries(value)) {
      if (key === 'position') continue;
      out[key] = shape(entry);
    }
    return out;
  }
  return value;
}

const CORPUS =
  '# Title\n\nIntro with **bold**, *em*, `code`, a [link](https://example.com), ' +
  'emoji 🎉 and ~~strike~~.\n\n- one\n- two with `tick`\n\n' +
  '1. first\n2. second\n\n> quoted\n> more\n\n' +
  '```ts\nconst x = 1;\nconst y = 2;\n```\n\n' +
  '| a | b |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |\n\n---\n\nClosing paragraph.\n';

describe('IncrementalMarkdown', () => {
  test.each([
    ['**中文。**接着', 'strong', '中文。'],
    ['**日本語。**続き', 'strong', '日本語。'],
    ['**한국어(parenthesis)**가', 'strong', '한국어(parenthesis)'],
    ['前*「斜体」*後', 'emphasis', '「斜体」'],
    ['前~~「删除」~~後', 'delete', '「删除」'],
  ])('parses CJK emphasis in %s', (source, type, value) => {
    const paragraph = parseBlocks(source)[0]!.node;
    if (paragraph.type !== 'paragraph') throw new Error('Expected a paragraph');
    expect(shape(paragraph.children.filter((node) => node.type === type))).toEqual([
      { type, children: [{ type: 'text', value }] },
    ]);
  });

  test('streamed CJK emphasis preserves block offsets and the settled prefix', () => {
    const source = '# 前**「标题」**後\n\n**日本語。**続き\n\n- **한국어(parenthesis)**가\n\n| 内容 |\n| --- |\n| 前~~「删除」~~後 |\n\n前*「斜体」*後';
    const incremental = new IncrementalMarkdown();
    for (const ch of source) {
      incremental.append(ch);
      const full = parseBlocks(incremental.text);
      expect(incremental.blocks.map((block) => [block.start, block.end, block.source]))
        .toEqual(full.map((block) => [block.start, block.end, block.source]));
      expect(shape(incremental.blocks.map((block) => block.node)))
        .toEqual(shape(full.map((block) => block.node)));
    }
  });

  test('chunked appends converge on the full parse', () => {
    for (const step of [1, 3, 7, 16]) {
      const incremental = new IncrementalMarkdown();
      for (let end = step; end < CORPUS.length + step; end += step) {
        incremental.setText(CORPUS.slice(0, Math.min(end, CORPUS.length)));
      }
      const full = parseBlocks(CORPUS);
      expect(incremental.text).toBe(CORPUS);
      expect(incremental.blocks.map((block) => [block.start, block.end, block.source]))
        .toEqual(full.map((block) => [block.start, block.end, block.source]));
      expect(shape(incremental.blocks.map((block) => block.node)))
        .toEqual(shape(full.map((block) => block.node)));
    }
  });

  test('non-append changes fall back to a full reparse', () => {
    const incremental = new IncrementalMarkdown();
    incremental.setText('hello **world**');
    incremental.setText('hello');
    expect(incremental.blocks).toHaveLength(1);
    expect(incremental.blocks[0]!.source).toBe('hello');
  });

  test('block offsets are UTF-16 code units', () => {
    const blocks = parseBlocks('🎉🎉\n\nafter');
    expect(blocks[1]!.start).toBe(6);
    expect(blocks[1]!.source).toBe('after');
  });

  test('settled blocks keep their identity across appends', () => {
    const incremental = new IncrementalMarkdown();
    incremental.setText('first\n\nsecond\n\nthird\n\nfour');
    const settledNode = incremental.blocks[0]!.node;
    incremental.setText('first\n\nsecond\n\nthird\n\nfourth and more');
    expect(incremental.blocks[0]!.node).toBe(settledNode);
  });

  test('display tail mends hanging emphasis while streaming', () => {
    const incremental = new IncrementalMarkdown();
    incremental.setText('done.\n\nnow **bold');
    const tail = incremental.displayTail();
    expect(tail).toHaveLength(1);
    const paragraph = tail![0]!.node;
    expect(paragraph.type).toBe('paragraph');
    const children = (paragraph as { children: Array<{ type: string }> }).children;
    expect(children.some((child) => child.type === 'strong')).toBe(true);
    expect(tail![0]!.start).toBe(7);
  });

  test('display tail leaves code blocks literal', () => {
    const incremental = new IncrementalMarkdown();
    incremental.setText('intro\n\n```ts\nconst a = "**not bold');
    expect(incremental.displayTail()).toBeNull();
  });

  test('settled text needs no display tail', () => {
    const incremental = new IncrementalMarkdown();
    incremental.setText('all **closed** here');
    expect(incremental.displayTail()).toBeNull();
  });

  test('a table absorbs rows streamed across appends', () => {
    const incremental = new IncrementalMarkdown();
    incremental.setText('| a | b |\n| - | - |\n');
    incremental.setText('| a | b |\n| - | - |\n| 1 ');
    incremental.setText('| a | b |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |\n');
    expect(incremental.blocks).toHaveLength(1);
    expect(incremental.blocks[0]!.node.type).toBe('table');
  });

  test('link definitions force full reparses', () => {
    expect(hasLinkDefinition('plain')).toBe(false);
    expect(hasLinkDefinition('[label]: https://example.com')).toBe(true);
    expect(hasLinkDefinition('  [^1]: footnote')).toBe(true);
    expect(hasLinkDefinition('[]: empty')).toBe(false);
    const incremental = new IncrementalMarkdown();
    incremental.setText('See [docs].\n\n');
    incremental.setText('See [docs].\n\n[docs]: https://example.com\n');
    const paragraph = incremental.blocks[0]!.node as {
      children: Array<{ type: string }>;
    };
    expect(paragraph.children.some((child) => child.type === 'linkReference')).toBe(true);
  });
});
