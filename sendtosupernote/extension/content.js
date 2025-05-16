// importScripts('https://unpkg.com/@mozilla/readability/dist/Readability.js'); // Removed: Prefer loading via manifest.json for V3

chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
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