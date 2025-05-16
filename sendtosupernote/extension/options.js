const backendUrlInput = document.getElementById('backendUrl');
const emailInput = document.getElementById('email');
const passwordInput = document.getElementById('password');
const loginSaveButton = document.getElementById('loginSaveButton');
const statusMessage = document.getElementById('statusMessage');

// Keys for chrome.storage.local
const STORAGE_KEYS = {
    BACKEND_URL: 'backendUrl',
    EMAIL: 'email',
    AUTH_TOKEN: 'authToken'
};

function displayMessage(message, isError = false) {
    statusMessage.textContent = message;
    statusMessage.style.color = isError ? 'red' : 'green';
    if (message) {
        statusMessage.style.border = `1px solid ${isError ? 'red' : 'green'}`;
        statusMessage.style.backgroundColor = isError ? '#ffebee' : '#e8f5e9';
    } else {
        statusMessage.style.border = 'none';
        statusMessage.style.backgroundColor = 'transparent';
    }
}

async function loadOptions() {
    chrome.storage.local.get([STORAGE_KEYS.BACKEND_URL, STORAGE_KEYS.EMAIL, STORAGE_KEYS.AUTH_TOKEN], (data) => {
        if (chrome.runtime.lastError) {
            displayMessage('Error loading settings: ' + chrome.runtime.lastError.message, true);
            return;
        }
        if (data[STORAGE_KEYS.BACKEND_URL]) {
            backendUrlInput.value = data[STORAGE_KEYS.BACKEND_URL];
        }
        if (data[STORAGE_KEYS.EMAIL]) {
            emailInput.value = data[STORAGE_KEYS.EMAIL];
        }
        if (data[STORAGE_KEYS.AUTH_TOKEN]) {
            displayMessage('Token found. Ready to send articles.', false);
            passwordInput.value = ''; // Clear password field if token exists
        } else {
            displayMessage('Please login to enable sending articles.', true);
        }
    });
}

async function handleLoginAndSave() {
    const backendUrl = backendUrlInput.value.trim();
    const email = emailInput.value.trim();
    const password = passwordInput.value.trim();

    if (!backendUrl || !email) {
        displayMessage('Backend URL and Email are required.', true);
        return;
    }

    // If password is provided, attempt login. Otherwise, just save URL and Email.
    if (password) {
        displayMessage('Attempting login...', false);
        try {
            const response = await fetch(`${backendUrl}/api/auth/login`, {
                method: 'POST',
                headers: {
                    'Content-Type': 'application/json',
                },
                body: JSON.stringify({ supernote_email: email, supernote_password: password }),
            });

            const responseData = await response.json();

            if (response.ok && responseData.access_token) {
                chrome.storage.local.set({
                    [STORAGE_KEYS.BACKEND_URL]: backendUrl,
                    [STORAGE_KEYS.EMAIL]: email,
                    [STORAGE_KEYS.AUTH_TOKEN]: responseData.access_token
                }, () => {
                    if (chrome.runtime.lastError) {
                        displayMessage('Error saving token: ' + chrome.runtime.lastError.message, true);
                    } else {
                        displayMessage('Login successful! Token and settings saved.', false);
                        passwordInput.value = ''; // Clear password after successful login
                    }
                });
            } else {
                const errorDetail = responseData.detail || `Login failed with status: ${response.status}`;
                displayMessage(`Login failed: ${errorDetail}`, true);
                // Clear potentially invalid token if login fails
                chrome.storage.local.remove(STORAGE_KEYS.AUTH_TOKEN);
            }
        } catch (error) {
            displayMessage(`Login error: ${error.message}. Check backend URL and ensure server is running.`, true);
            chrome.storage.local.remove(STORAGE_KEYS.AUTH_TOKEN);
        }
    } else {
        // No password, just save Backend URL and Email if they are potentially changed
        // This also covers the case where user wants to update URL/email *after* being logged in.
        chrome.storage.local.set({
            [STORAGE_KEYS.BACKEND_URL]: backendUrl,
            [STORAGE_KEYS.EMAIL]: email
            // Don't overwrite or remove existing token here unless explicitly logging out
        }, () => {
            if (chrome.runtime.lastError) {
                displayMessage('Error saving settings: ' + chrome.runtime.lastError.message, true);
            } else {
                // Check if already logged in to not overwrite good login message
                chrome.storage.local.get(STORAGE_KEYS.AUTH_TOKEN, data => {
                    if (!data[STORAGE_KEYS.AUTH_TOKEN]) {
                         displayMessage('Settings (Backend URL, Email) saved. Please enter password to login.', false);
                    } else {
                         displayMessage('Settings (Backend URL, Email) updated. Token remains active.', false);
                    }
                });
            }
        });
    }
}

loginSaveButton.addEventListener('click', handleLoginAndSave);
document.addEventListener('DOMContentLoaded', loadOptions); 