import { describe, expect, test } from 'bun:test'
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import ReactMarkdown from 'react-markdown'
import { markdownPlugins } from './markdown-plugins'

function render(source: string) {
  return renderToStaticMarkup(createElement(ReactMarkdown, { remarkPlugins: markdownPlugins }, source))
}

describe('CJK Markdown', () => {
  test.each([
    ['**中文。**接着', '<p><strong>中文。</strong>接着</p>'],
    ['**日本語。**続き', '<p><strong>日本語。</strong>続き</p>'],
    ['**한국어(parenthesis)**가', '<p><strong>한국어(parenthesis)</strong>가</p>'],
    ['前**「重要」**後', '<p>前<strong>「重要」</strong>後</p>'],
    ['前*「斜体」*後', '<p>前<em>「斜体」</em>後</p>'],
    ['前~~「削除」~~後', '<p>前<del>「削除」</del>後</p>'],
  ])('renders %s', (source, html) => {
    expect(render(source)).toBe(html)
  })

  test('preserves code, escaped markers, and English emphasis', () => {
    expect(render('`前**「代码」**後`')).toBe('<p><code>前**「代码」**後</code></p>')
    expect(render('\\*\\*「文字」\\*\\*後')).toBe('<p>**「文字」**後</p>')
    expect(render('a**"word"**b foo_bar_baz')).toBe('<p>a**&quot;word&quot;**b foo_bar_baz</p>')
    expect(render('**bold** and *italic*')).toBe('<p><strong>bold</strong> and <em>italic</em></p>')
  })

  test('supports CJK emphasis inside GFM tables and task lists', () => {
    expect(render('| 内容 |\n| --- |\n| 前**「重要」**後 |')).toContain('前<strong>「重要」</strong>後')
    expect(render('- [x] 前~~「完了」~~後')).toContain('前<del>「完了」</del>後')
  })
})
