import { describe, expect, it } from 'vitest'
import { createRoot } from 'react-dom/client'
import { act } from 'react'
import { MarkdownResult, parseBlocks, parseInline } from '../markdown'

const LF = String.fromCharCode(10)

function renderToHtml(source: string): string {
  const container = document.createElement('div')
  const root = createRoot(container)
  act(() => {
    root.render(<MarkdownResult source={source} />)
  })
  return container.innerHTML
}

describe('inline parsing', () => {
  it('splits bold / italic / code tokens', () => {
    const tokens = parseInline('plain **bold** and *italic* and `code` end')
    expect(tokens).toEqual([
      { kind: 'text', text: 'plain ' },
      { kind: 'bold', text: 'bold' },
      { kind: 'text', text: ' and ' },
      { kind: 'italic', text: 'italic' },
      { kind: 'text', text: ' and ' },
      { kind: 'code', text: 'code' },
      { kind: 'text', text: ' end' },
    ])
  })

  it('does not interpret markers inside inline code', () => {
    const tokens = parseInline('`a **b** c` tail')
    expect(tokens).toEqual([
      { kind: 'code', text: 'a **b** c' },
      { kind: 'text', text: ' tail' },
    ])
  })

  it('keeps unmatched markers as plain text', () => {
    expect(parseInline('2 * 3 = 6')).toEqual([{ kind: 'text', text: '2 * 3 = 6' }])
  })
})

describe('block parsing', () => {
  it('parses headings, lists, quote, code fence and hr', () => {
    const blocks = parseBlocks([
      '# 标题',
      '',
      '- 甲',
      '- 乙',
      '',
      '1. 一',
      '2. 二',
      '',
      '> 引用一',
      '> 引用二',
      '',
      '---',
      '',
      '```rust',
      'fn main() {}',
      '```',
      '',
      '普通段落 **加粗**。',
    ].join('\n'))
    expect(blocks.map((b) => b.kind)).toEqual([
      'heading', 'list', 'list', 'quote', 'hr', 'code', 'paragraph',
    ])
    const [heading, , list2] = blocks as Array<{ kind: string; level?: number; ordered?: boolean }>
    expect(heading.level).toBe(1)
    expect(list2.ordered).toBe(true)
  })

  it('merges consecutive paragraph lines into one block', () => {
    const blocks = parseBlocks('第一行\n第二行\n\n第二段')
    expect(blocks).toHaveLength(2)
  })
})

describe('rendering (escape safety)', () => {
  it('escapes html-looking content as text nodes', () => {
    const html = renderToHtml('<script>alert(1)</script> **bold**')
    expect(html).not.toContain('<script>')
    expect(html).toContain('&lt;script&gt;')
    expect(html).toContain('<strong')
    expect(html).toContain('bold</strong>')
  })

  it('renders headings as h2-h5 (demoted one level)', () => {
    const html = renderToHtml('## 二级')
    expect(html).toContain('<h3')
    expect(html).not.toContain('<h1')
  })

  it('renders code fence content verbatim inside pre', () => {
    const html = renderToHtml('```\n**not bold** <b>\n```')
    expect(html).toContain('<pre')
    expect(html).toContain('**not bold**')
    expect(html).not.toContain('<b>')
  })

  it('handles nested and mismatched inline markers gracefully (single-pass scanner)', () => {
    // 加粗内含斜体：bold 原样（v1 不递归），不成对星号保持文本。
    const tokens = parseInline('**bold *inner* end** and 2 x 3 = 6')
    expect(tokens.some((token) => token.kind === 'bold' && token.text === 'bold *inner* end')).toBe(true)
    expect(tokens.some((token) => token.kind === 'italic')).toBe(false)
  })

  it('four-backtick fence wraps an inner three-backtick sample', () => {
    const blocks = parseBlocks(['````', 'markdown 示例：', '```js', 'code', '```', '结束', '````'].join(LF))
    const codeBlock = blocks.find((b) => b.kind === 'code') as { kind: string; text: string } | undefined
    expect(codeBlock).toBeTruthy()
    expect(codeBlock!.text).toContain('```js')
    expect(codeBlock!.text).toContain('结束')
  })

  it('parses a large document without quadratic blowup', () => {
    const line = '要点 **加粗** 与 `code` 以及普通文字。'
    const start = Date.now()
    parseBlocks(Array(2000).fill(line).join(LF))
    const elapsed = Date.now() - start
    expect(elapsed).toBeLessThan(500)
  })

  it('renders ordered and unordered lists', () => {
    const html = renderToHtml('- a\n- b\n\n1. x\n2. y')
    expect(html).toContain('<ul')
    expect(html).toContain('<ol')
  })
})
