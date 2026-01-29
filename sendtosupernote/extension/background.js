// Background service worker for context menu functionality

const STORAGE_KEYS = {
    BACKEND_URL: 'backendUrl',
    AUTH_TOKEN: 'authToken'
};

// Create context menu on install
chrome.runtime.onInstalled.addListener(() => {
    chrome.contextMenus.create({
        id: 'sendToSupernote',
        title: 'Send to Supernote',
        contexts: ['page', 'link']
    });
});

// Handle context menu clicks
chrome.contextMenus.onClicked.addListener(async (info, tab) => {
    if (info.menuItemId !== 'sendToSupernote') return;

    // If clicked on a link, use the link URL; otherwise use the current page
    const targetUrl = info.linkUrl || info.pageUrl;
    const targetTabId = tab?.id;

    if (!targetTabId) {
        showNotification('Error', 'Could not get current tab.');
        return;
    }

    // Load configuration
    let config;
    try {
        config = await chrome.storage.local.get([STORAGE_KEYS.BACKEND_URL, STORAGE_KEYS.AUTH_TOKEN]);
    } catch (error) {
        showNotification('Error', `Error loading configuration: ${error.message}`);
        return;
    }

    const backendUrl = config[STORAGE_KEYS.BACKEND_URL];
    const authToken = config[STORAGE_KEYS.AUTH_TOKEN];

    if (!backendUrl || !authToken) {
        showNotification('Configuration Required', 'Backend URL or Auth Token not set. Please configure in extension options.');
        chrome.runtime.openOptionsPage();
        return;
    }

    // If it's a link click, we need to fetch and process that URL
    if (info.linkUrl) {
        await sendUrlToBackend(info.linkUrl, backendUrl, authToken);
    } else {
        // For page context, extract content from current tab
        await sendCurrentPageToBackend(targetTabId, tab, backendUrl, authToken);
    }
});

async function sendUrlToBackend(url, backendUrl, authToken) {
    showNotification('Sending...', `Sending ${truncateUrl(url)} to Supernote`);

    const payload = {
        url: url,
        source_identifier: 'Chrome Extension (Context Menu Link)'
    };

    try {
        const response = await fetch(`${backendUrl}/api/queue_article`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                'Authorization': `Bearer ${authToken}`,
            },
            body: JSON.stringify(payload),
        });

        const responseData = await response.json();

        if (response.ok) {
            showNotification('Success', responseData.message || 'Article queued!');
        } else {
            const errorMsg = responseData.detail || response.statusText || 'Unknown error';
            showNotification('Error', errorMsg);
        }
    } catch (error) {
        showNotification('Error', `Network error: ${error.message}`);
    }
}

async function sendCurrentPageToBackend(tabId, tab, backendUrl, authToken) {
    showNotification('Processing...', 'Extracting page content...');

    let extractedHtmlResponse;
    try {
        extractedHtmlResponse = await chrome.tabs.sendMessage(tabId, { action: 'extract' });
        if (!extractedHtmlResponse || !extractedHtmlResponse.success || typeof extractedHtmlResponse.content === 'undefined') {
            const errorDetail = extractedHtmlResponse
                ? (extractedHtmlResponse.error || 'Content script indicated failure.')
                : 'No response from content script.';
            showNotification('Error', `Could not extract HTML: ${errorDetail}`);
            return;
        }
    } catch (error) {
        showNotification('Error', `Error extracting content: ${error.message}. Try reloading the page.`);
        return;
    }

    const payload = {
        url: tab.url,
        html_content: extractedHtmlResponse.content,
        source_identifier: tab.title || 'Chrome Extension Article',
    };

    try {
        showNotification('Sending...', 'Sending to backend...');
        const response = await fetch(`${backendUrl}/api/queue_article`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                'Authorization': `Bearer ${authToken}`,
            },
            body: JSON.stringify(payload),
        });

        const responseData = await response.json();

        if (response.ok) {
            showNotification('Success', responseData.message || 'Article queued!');
        } else {
            const errorMsg = responseData.detail || response.statusText || 'Unknown error';
            showNotification('Error', errorMsg);
        }
    } catch (error) {
        showNotification('Error', `Network error: ${error.message}`);
    }
}

function showNotification(title, message) {
    chrome.notifications.create({
        type: 'basic',
        iconUrl: 'icon128.png',
        title: `Send to Supernote: ${title}`,
        message: message
    });
}

function truncateUrl(url, maxLength = 50) {
    if (url.length <= maxLength) return url;
    return url.substring(0, maxLength - 3) + '...';
}
