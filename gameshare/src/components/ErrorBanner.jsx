export default function ErrorBanner({ message, onDismiss }) {
  if (!message) return null
  return (
    <div className="error-banner">
      <span className="error-icon">⚠</span>
      <span className="error-msg">{message}</span>
      <button className="error-dismiss" onClick={onDismiss}>✕</button>
    </div>
  )
}
