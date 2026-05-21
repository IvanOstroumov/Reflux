import { useState } from 'react'

export default function TokenDisplay({ code, compact = false }) {
  const [copied, setCopied] = useState(false)

  function copy() {
    navigator.clipboard?.writeText(code)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  if (compact) {
    return (
      <button className="token-compact" onClick={copy} title="Copy invite code">
        {code} {copied ? '✓' : '⎘'}
      </button>
    )
  }

  return (
    <div className="token-block">
      <div className="token-label">INVITE CODE</div>
      <div className="token-row">
        <span className="token-text">{code}</span>
        <button className="token-copy" onClick={copy} title="Copy">
          {copied ? (
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
              <polyline points="20 6 9 17 4 12"/>
            </svg>
          ) : (
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <rect x="9" y="9" width="13" height="13" rx="2"/>
              <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>
            </svg>
          )}
        </button>
      </div>
      <div className="token-hint">Share this code with the viewer</div>
    </div>
  )
}
