import { describe, expect, it } from 'vitest'
import { createRoot } from 'react-dom/client'
import { act } from 'react'
import { MarkdownResult, parseBlocks, parseInline } from '../markdown'

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

  it('renders ordered and unordered lists', () => {
    const html = renderToHtml('- a\n- b\n\n1. x\n2. y')
    expect(html).toContain('<ul')
    expect(html).toContain('<ol')
  })
})
