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

/** 行内解析：先挖行内代码（其中的 ** 不再解释），再扫加粗/斜体。 */
export function parseInline(raw: string): InlineToken[] {
  const tokens: InlineToken[] = []
  let rest = raw
  while (rest.length > 0) {
    const codeMatch = rest.match(CODE_RE)
    const boldMatch = rest.match(BOLD_RE)
    const italicMatch = rest.match(ITALIC_RE)
    // 取三者中最早出现的标记。
    const candidates: Array<{ at: number; push: () => void }> = []
    if (codeMatch) candidates.push({ at: codeMatch.index!, push: () => { tokens.push({ kind: 'code', text: codeMatch[1] }); rest = rest.slice(codeMatch.index! + codeMatch[0].length) } })
    if (boldMatch) candidates.push({ at: boldMatch.index!, push: () => { tokens.push({ kind: 'bold', text: boldMatch[1] }); rest = rest.slice(boldMatch.index! + boldMatch[0].length) } })
    if (italicMatch) candidates.push({ at: italicMatch.index!, push: () => { tokens.push({ kind: 'italic', text: italicMatch[1] }); rest = rest.slice(italicMatch.index! + italicMatch[0].length) } })
    if (candidates.length === 0) break
    const first = candidates.reduce((a, b) => (a.at <= b.at ? a : b))
    if (first.at > 0) tokens.push({ kind: 'text', text: rest.slice(0, first.at) })
    first.push()
  }
  if (rest.length > 0) tokens.push({ kind: 'text', text: rest })
  return tokens
}

/** 块级解析：逐行状态机。连续的非空段落行合并为一段。 */
export function parseBlocks(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const blocks: Block[] = []
  let paragraph: string[] = []
  let list: { ordered: boolean; items: string[] } | null = null
  let quote: string[] | null = null
  let code: { text: string[] } | null = null

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
      if (/^\s*```/.test(line)) {
        blocks.push({ kind: 'code', text: code.text.join('\n') })
        code = null
      } else {
        code.text.push(line)
      }
      continue
    }
    if (/^\s*```/.test(line)) {
      flushAll()
      code = { text: [] }
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
