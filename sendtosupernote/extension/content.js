// importScripts('https://unpkg.com/@mozilla/readability/dist/Readability.js'); // Removed: Prefer loading via manifest.json for V3

// Toast notification system
const TOAST_STYLES = `
  .supernote-toast-container {
    position: fixed !important;
    top: 20px !important;
    right: 20px !important;
    left: auto !important;
    bottom: auto !important;
    z-index: 2147483647 !important;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif !important;
    pointer-events: none !important;
  }
  .supernote-toast {
    display: flex !important;
    align-items: center !important;
    gap: 10px !important;
    padding: 14px 18px !important;
    margin-bottom: 10px !important;
    border-radius: 8px !important;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3) !important;
    font-size: 14px !important;
    line-height: 1.4 !important;
    max-width: 360px !important;
    min-width: 280px !important;
    animation: supernote-slide-in 0.3s ease-out !important;
    transition: opacity 0.3s ease-out, transform 0.3s ease-out !important;
    pointer-events: auto !important;
    opacity: 1 !important;
    visibility: visible !important;
  }
  .supernote-toast.hiding {
    opacity: 0 !important;
    transform: translateX(100%) !important;
  }
  .supernote-toast-success {
    background: #065f46 !important;
    color: white !important;
  }
  .supernote-toast-error {
    background: #991b1b !important;
    color: white !important;
  }
  .supernote-toast-info {
    background: #1e40af !important;
    color: white !important;
  }
  .supernote-toast-icon {
    font-size: 18px !important;
    flex-shrink: 0 !important;
  }
  .supernote-toast-content {
    flex: 1 !important;
  }
  .supernote-toast-title {
    font-weight: 600 !important;
    margin-bottom: 2px !important;
  }
  .supernote-toast-message {
    opacity: 0.9 !important;
    font-size: 13px !important;
  }
  @keyframes supernote-slide-in {
    from {
      opacity: 0;
      transform: translateX(100%);
    }
    to {
      opacity: 1;
      transform: translateX(0);
    }
  }
`;

function ensureToastContainer() {
  let container = document.querySelector('.supernote-toast-container');
  if (!container) {
    // Inject styles
    const styleEl = document.createElement('style');
    styleEl.textContent = TOAST_STYLES;
    document.head.appendChild(styleEl);

    // Create container
    container = document.createElement('div');
    container.className = 'supernote-toast-container';
    document.body.appendChild(container);
  }
  return container;
}

function showToast(type, title, message, duration = 4000) {
  const container = ensureToastContainer();

  const icons = {
    success: '✓',
    error: '✕',
    info: '↑'
  };

  const toast = document.createElement('div');
  toast.className = `supernote-toast supernote-toast-${type}`;
  toast.innerHTML = `
    <span class="supernote-toast-icon">${icons[type] || '•'}</span>
    <div class="supernote-toast-content">
      <div class="supernote-toast-title">${title}</div>
      <div class="supernote-toast-message">${message}</div>
    </div>
  `;

  container.appendChild(toast);

  // Auto-dismiss
  setTimeout(() => {
    toast.classList.add('hiding');
    setTimeout(() => toast.remove(), 300);
  }, duration);
}

chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  console.log('[Supernote] Message received:', request.action);

  if (request.action === 'showToast') {
    console.log('[Supernote] Showing toast:', request.type, request.title, request.message);
    showToast(request.type, request.title, request.message, request.duration);
    sendResponse({ success: true });
    return true;
  }

  if (request.action === 'extract') {
    try {
      if (typeof Readability === 'undefined') {
        console.error('Readability.js is not loaded. Ensure it is listed before content.js in manifest.json\'s content_scripts and the file exists in the extension package.');
        sendResponse({ success: false, error: 'Readability library not available. Check extension setup.' });
        return true; // Indicates asynchronous response
      }

      const documentClone = document.cloneNode(true);
      const article = new Readability(documentClone).parse();

      if (article && article.content) {
        sendResponse({ success: true, content: article.content });
      } else {
        const errorMessage = article && article.title ? `Readability could not extract content (title: ${article.title}).` : 'Readability could not extract content from this page.';
        console.warn(errorMessage);
        sendResponse({ success: false, error: errorMessage });
      }
    } catch (e) {
      console.error('Error during Readability extraction:', e);
      sendResponse({ success: false, error: `Extraction failed: ${e.message}` });
    }
    return true; // Keep the message channel open for the asynchronous response
  }
}); 