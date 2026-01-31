// Background service worker for context menu functionality

const STORAGE_KEYS = {
    BACKEND_URL: 'backendUrl',
    AUTH_TOKEN: 'authToken'
};

// Track current tab for toast notifications
let currentTabId = null;

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
    currentTabId = tab?.id;

    if (!currentTabId) {
        showNotification('error', 'Error', 'Could not get current tab.');
        return;
    }

    // Load configuration
    let config;
    try {
        config = await chrome.storage.local.get([STORAGE_KEYS.BACKEND_URL, STORAGE_KEYS.AUTH_TOKEN]);
    } catch (error) {
        showNotification('error', 'Error', `Error loading configuration: ${error.message}`);
        return;
    }

    const backendUrl = config[STORAGE_KEYS.BACKEND_URL];
    const authToken = config[STORAGE_KEYS.AUTH_TOKEN];

    if (!backendUrl || !authToken) {
        showNotification('error', 'Configuration Required', 'Backend URL or Auth Token not set. Please configure in extension options.');
        chrome.runtime.openOptionsPage();
        return;
    }

    // If it's a link click, we need to fetch and process that URL
    if (info.linkUrl) {
        await sendUrlToBackend(info.linkUrl, backendUrl, authToken);
    } else {
        // For page context, extract content from current tab
        await sendCurrentPageToBackend(currentTabId, tab, backendUrl, authToken);
    }
});

async function sendUrlToBackend(url, backendUrl, authToken) {
    showNotification('info', 'Sending to Supernote', truncateUrl(url));

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
            showNotification('success', 'Sent to Supernote', responseData.message || 'Article queued!');
        } else {
            const errorMsg = responseData.detail || response.statusText || 'Unknown error';
            showNotification('error', 'Failed to Send', errorMsg);
        }
    } catch (error) {
        showNotification('error', 'Network Error', error.message);
    }
}

async function sendCurrentPageToBackend(tabId, tab, backendUrl, authToken) {
    showNotification('info', 'Processing', 'Extracting page content...');

    let extractedHtmlResponse;
    try {
        extractedHtmlResponse = await chrome.tabs.sendMessage(tabId, { action: 'extract' });
        if (!extractedHtmlResponse || !extractedHtmlResponse.success || typeof extractedHtmlResponse.content === 'undefined') {
            const errorDetail = extractedHtmlResponse
                ? (extractedHtmlResponse.error || 'Content script indicated failure.')
                : 'No response from content script.';
            showNotification('error', 'Extraction Failed', errorDetail);
            return;
        }
    } catch (error) {
        showNotification('error', 'Extraction Error', `${error.message}. Try reloading the page.`);
        return;
    }

    const payload = {
        url: tab.url,
        html_content: extractedHtmlResponse.content,
        source_identifier: tab.title || 'Chrome Extension Article',
    };

    try {
        showNotification('info', 'Sending', 'Uploading to Supernote...');
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
            showNotification('success', 'Sent to Supernote', responseData.message || 'Article queued!');
        } else {
            const errorMsg = responseData.detail || response.statusText || 'Unknown error';
            showNotification('error', 'Failed to Send', errorMsg);
        }
    } catch (error) {
        showNotification('error', 'Network Error', error.message);
    }
}

async function showNotification(type, title, message) {
    if (currentTabId) {
        try {
            await chrome.tabs.sendMessage(currentTabId, {
                action: 'showToast',
                type: type,
                title: title,
                message: message
            });
        } catch (error) {
            // Fallback to Chrome notification if content script not available
            chrome.notifications.create({
                type: 'basic',
                iconUrl: 'icon128.png',
                title: `Supernote: ${title}`,
                message: message
            });
        }
    } else {
        // Fallback to Chrome notification
        chrome.notifications.create({
            type: 'basic',
            iconUrl: 'icon128.png',
            title: `Supernote: ${title}`,
            message: message
        });
    }
}

function truncateUrl(url, maxLength = 50) {
    if (url.length <= maxLength) return url;
    return url.substring(0, maxLength - 3) + '...';
}
