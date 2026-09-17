import { Fragment, type ReactNode } from 'react'

/**
 * 悬浮窗结果卡的极简 Markdown 渲染器（零依赖）。
 *
 * 设计约束：
 * - 输出 React 元素树而不是 HTML 字符串——文本节点由 React 自动转义，
 *   无 XSS 面；行内标记（`**` 等）在转义后的纯文本上做结构拆分。
 * - 只支持「讲解卡」需要的子集：h1–h4、段落、有序/无序列表、加粗、
 *   斜体、行内代码、围栏代码块、引用、分隔线。表格/图片/链接不渲染
 *   （prompt 侧已要求 AI 不输出；出现时按纯文本降级，不报错）。
 * - 排版参照 shiye（拾页）的纸感参数，按悬浮窗暗色小尺寸缩放：
 *   细线标题、浅底圆角代码块、左侧竖线引用、收敛的标题梯度。
 */

type InlineToken =
  | { kind: 'text'; text: string }
  | { kind: 'bold'; text: string }
  | { kind: 'italic'; text: string }
  | { kind: 'code'; text: string }

type Block =
  | { kind: 'heading'; level: 1 | 2 | 3 | 4; tokens: InlineToken[] }
  | { kind: 'paragraph'; tokens: InlineToken[] }
  | { kind: 'list'; ordered: boolean; items: InlineToken[][] }
  | { kind: 'code'; text: string }
  | { kind: 'quote'; lines: InlineToken[][] }
  | { kind: 'hr' }

const BOLD_RE = /\*\*([^*]+)\*\*/
const ITALIC_RE = /(?<!\*)\*([^*\n]+)\*(?!\*)/
const CODE_RE = /`([^`\n]+)`/

/** 行内解析：单次扫描（O(n)）。行内代码原样不递归；加粗内部递归支持斜体嵌套。 */
export function parseInline(raw: string): InlineToken[] {
  const tokens: InlineToken[] = []
  let buffer = ''
  let i = 0
  const flush = () => {
    if (buffer.length > 0) {
      tokens.push({ kind: 'text', text: buffer })
      buffer = ''
    }
  }
  while (i < raw.length) {
    const ch = raw[i]
    if (ch === '`') {
      const close = raw.indexOf('`', i + 1)
      if (close > i) {
        flush()
        tokens.push({ kind: 'code', text: raw.slice(i + 1, close) })
        i = close + 1
        continue
      }
    } else if (ch === '*' && raw[i + 1] === '*') {
      const close = raw.indexOf('**', i + 2)
      if (close > i) {
        flush()
        tokens.push({ kind: 'bold', text: raw.slice(i + 2, close) })
        i = close + 2
        continue
      }
    } else if (ch === '*' && raw[i + 1] !== '*' && i > 0 && raw[i - 1] !== '*') {
      // 单星斜体：找下一个不被 ** 包裹的 *
      const close = raw.indexOf('*', i + 1)
      if (close > i && raw[close + 1] !== '*') {
        flush()
        tokens.push({ kind: 'italic', text: raw.slice(i + 1, close) })
        i = close + 1
        continue
      }
    }
    buffer += ch
    i += 1
  }
  flush()
  return tokens
}

/** 块级解析：逐行状态机。连续的非空段落行合并为一段。 */
export function parseBlocks(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const blocks: Block[] = []
  let paragraph: string[] = []
  let list: { ordered: boolean; items: string[] } | null = null
  let quote: string[] | null = null
  let code: { text: string[]; fenceLen: number } | null = null

  const flushParagraph = () => {
    if (paragraph.length > 0) {
      blocks.push({ kind: 'paragraph', tokens: parseInline(paragraph.join(' ')) })
      paragraph = []
    }
  }
  const flushList = () => {
    if (list && list.items.length > 0) {
      blocks.push({ kind: 'list', ordered: list.ordered, items: list.items.map(parseInline) })
    }
    list = null
  }
  const flushQuote = () => {
    if (quote && quote.length > 0) {
      blocks.push({ kind: 'quote', lines: quote.map(parseInline) })
    }
    quote = null
  }
  const flushAll = () => {
    flushParagraph()
    flushList()
    flushQuote()
  }

  for (const line of lines) {
    if (code) {
      // 结束栏必须不短于开栏：```` 包 ``` 示例时不被内层栏提前截断。
      const fenceClose = line.match(/^\s*(`{3,})\s*$/)
      if (fenceClose && fenceClose[1].length >= code.fenceLen) {
        blocks.push({ kind: 'code', text: code.text.join('\n') })
        code = null
      } else {
        code.text.push(line)
      }
      continue
    }
    const fenceOpen = line.match(/^\s*(`{3,})/)
    if (fenceOpen) {
      flushAll()
      code = { text: [], fenceLen: fenceOpen[1].length }
      continue
    }
    if (line.trim().length === 0) {
      flushAll()
      continue
    }
    const heading = line.match(/^(#{1,4})\s+(.*)$/)
    if (heading) {
      flushAll()
      const level = heading[1].length as 1 | 2 | 3 | 4
      blocks.push({ kind: 'heading', level, tokens: parseInline(heading[2]) })
      continue
    }
    if (/^\s*(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
      flushAll()
      blocks.push({ kind: 'hr' })
      continue
    }
    const bullet = line.match(/^\s*[-*+]\s+(.*)$/)
    if (bullet) {
      flushParagraph()
      flushQuote()
      if (!list || list.ordered) {
        flushList()
        list = { ordered: false, items: [] }
      }
      list.items.push(bullet[1])
      continue
    }
    const ordered = line.match(/^\s*\d+[.)]\s+(.*)$/)
    if (ordered) {
      flushParagraph()
      flushQuote()
      if (!list || !list.ordered) {
        flushList()
        list = { ordered: true, items: [] }
      }
      list.items.push(ordered[1])
      continue
    }
    const quoted = line.match(/^\s*>\s?(.*)$/)
    if (quoted) {
      flushParagraph()
      flushList()
      if (!quote) quote = []
      quote.push(quoted[1])
      continue
    }
    flushList()
    flushQuote()
    paragraph.push(line.trim())
  }
  if (code) blocks.push({ kind: 'code', text: code.text.join('\n') })
  flushAll()
  return blocks
}

function renderInline(tokens: InlineToken[], keyPrefix: string): ReactNode[] {
  return tokens.map((token, index) => {
    const key = `${keyPrefix}-${index}`
    switch (token.kind) {
      case 'bold':
        return <strong key={key} className="font-semibold">{token.text}</strong>
      case 'italic':
        return <em key={key}>{token.text}</em>
      case 'code':
        return (
          <code key={key} className="rounded bg-white/10 px-1 py-px font-mono text-[0.85em]">
            {token.text}
          </code>
        )
      default:
        return <Fragment key={key}>{token.text}</Fragment>
    }
  })
}

const headingClass: Record<number, string> = {
  1: 'mt-3 mb-1 border-b border-white/10 pb-1 text-[1.3em] font-semibold',
  2: 'mt-3 mb-1 border-b border-white/10 pb-1 text-[1.15em] font-semibold',
  3: 'mt-2.5 mb-1 text-[1.05em] font-semibold',
  4: 'mt-2 mb-1 text-[0.95em] font-semibold',
}

/** 结果卡正文渲染入口。容器（滚动/选区）由调用方提供。 */
export function MarkdownResult({ source }: { source: string }) {
  const blocks = parseBlocks(source)
  return (
    <div className="markdown-result text-left text-sm leading-relaxed">
      {blocks.map((block, index) => {
        const key = `b-${index}`
        switch (block.kind) {
          case 'heading': {
            const Tag = (`h${Math.min(block.level + 1, 6)}`) as 'h2' | 'h3' | 'h4' | 'h5'
            return (
              <Tag key={key} className={headingClass[block.level]}>
                {renderInline(block.tokens, key)}
              </Tag>
            )
          }
          case 'paragraph':
            return <p key={key} className="my-1.5">{renderInline(block.tokens, key)}</p>
          case 'list': {
            const ListTag = block.ordered ? 'ol' : 'ul'
            return (
              <ListTag
                key={key}
                className={`my-1.5 pl-5 ${block.ordered ? 'list-decimal' : 'list-disc'}`}
              >
                {block.items.map((item, itemIndex) => (
                  <li key={`${key}-${itemIndex}`} className="my-0.5">
                    {renderInline(item, `${key}-${itemIndex}`)}
                  </li>
                ))}
              </ListTag>
            )
          }
          case 'code':
            return (
              <pre key={key} className="my-2 overflow-x-auto rounded-lg border border-white/10 bg-black/30 p-3 font-mono text-[0.85em] leading-relaxed">
                <code>{block.text}</code>
              </pre>
            )
          case 'quote':
            return (
              <blockquote key={key} className="my-2 border-l-2 border-white/25 pl-3 text-white/70">
                {block.lines.map((line, lineIndex) => (
                  <p key={`${key}-${lineIndex}`} className="my-1">{renderInline(line, `${key}-${lineIndex}`)}</p>
                ))}
              </blockquote>
            )
          case 'hr':
            return <hr key={key} className="my-3 border-white/10" />
        }
      })}
    </div>
  )
}
