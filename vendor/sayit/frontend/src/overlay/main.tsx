import { startWebviewKeyboardFallback } from '../services/webviewKeyboardFallback'
import React from 'react'
import ReactDOM from 'react-dom/client'
import Overlay from './Overlay'
import '../index.css'

// Transparent background for overlay window
const style = document.createElement('style')
style.textContent = [
  'html, body, #root { background: transparent !important; }',
  // 高度链：result 阅读卡靠 h-full 限高形成卡内滚动；缺了它长正文会撑高
  // body 并被 overflow:hidden 裁掉底部（Codex 交叉审查 2026-09-17）。
  'html, body, #root { height: 100%; }',
].join(String.fromCharCode(10))
document.head.appendChild(style)

void startWebviewKeyboardFallback()

// overlay-ping 健康检测监听已移除：上游本就没有任何 native 侧发射端
// （连同 overlay_pong 命令一起是未完成机制），监听只会空转。

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <Overlay />
  </React.StrictMode>
)
