const sendButton = document.getElementById('sendBtn');
const statusMessage = document.getElementById('status');

// Keys for chrome.storage.local (must match options.js)
const STORAGE_KEYS = {
    BACKEND_URL: 'backendUrl',
    AUTH_TOKEN: 'authToken'
};

function displayStatus(message, isError = false) {
    statusMessage.textContent = message;
    statusMessage.style.color = isError ? 'red' : '#007700'; // Dark green for success
}

async function loadConfiguration() {
    return new Promise((resolve, reject) => {
        chrome.storage.local.get([STORAGE_KEYS.BACKEND_URL, STORAGE_KEYS.AUTH_TOKEN], (result) => {
            if (chrome.runtime.lastError) {
                return reject(chrome.runtime.lastError);
            }
            resolve(result);
        });
    });
}

sendButton.addEventListener('click', async () => {
    displayStatus('Processing...');
    let config;
    try {
        config = await loadConfiguration();
    } catch (error) {
        displayStatus(`Error loading configuration: ${error.message}`, true);
        return;
    }

    const backendUrl = config[STORAGE_KEYS.BACKEND_URL];
    const authToken = config[STORAGE_KEYS.AUTH_TOKEN];

    if (!backendUrl || !authToken) {
        displayStatus('Backend URL or Auth Token not set. Please configure in options.', true);
        // Optionally open options page
        // chrome.runtime.openOptionsPage(); 
        return;
    }

    let activeTab;
    try {
        [activeTab] = await chrome.tabs.query({ active: true, currentWindow: true });
        if (!activeTab || !activeTab.id) {
            displayStatus('Could not get active tab.', true);
            return;
        }
    } catch (error) {
        displayStatus(`Error getting active tab: ${error.message}`, true);
        return;
    }
    
    let extractedHtmlResponse;
    try {
        // Ensure content script is ready or inject if necessary (advanced)
        extractedHtmlResponse = await chrome.tabs.sendMessage(activeTab.id, { action: 'extract' });
        // Check if the response indicates success AND has content
        if (!extractedHtmlResponse || !extractedHtmlResponse.success || typeof extractedHtmlResponse.content === 'undefined') {
            const errorDetail = extractedHtmlResponse ? (extractedHtmlResponse.error || 'Content script indicated failure but no error message.') : 'No response from content script.';
            displayStatus(`Could not extract HTML: ${errorDetail}`, true);
            return;
        }
    } catch (error) {
        displayStatus(`Error extracting content: ${error.message}. Try reloading the page.`, true);
        // This can happen if content script isn't injected or page is protected
        return;
    }

    const payload = {
        url: activeTab.url,
        html_content: extractedHtmlResponse.content,
        source_identifier: activeTab.title || "Chrome Extension Article", // Use tab title if available
        // target_path: Defaulted by backend or future option
        // pdf_font_size: Defaulted by backend or future option
    };

    try {
        displayStatus('Sending to backend...');
        const response = await fetch(`${backendUrl}/api/queue_article`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                'Authorization': `Bearer ${authToken}`,
            },
            body: JSON.stringify(payload),
        });

        const responseData = await response.json(); // Always try to parse JSON for backend messages

        if (response.ok) {
            displayStatus(`Success: ${responseData.message || 'Article queued!'} (ID: ${responseData.task_id})`);
        } else {
            let errorMsg = `Error: ${responseData.detail || response.statusText || 'Unknown error'}`;
            if (response.status === 401) {
                errorMsg += ' Token might be invalid. Please re-login via options.';
            }
            displayStatus(errorMsg, true);
        }
    } catch (error) {
        displayStatus(`Network or other error: ${error.message}. Check backend URL.`, true);
    }
}); 