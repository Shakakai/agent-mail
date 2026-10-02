// agent-mail docs: copy-to-clipboard buttons for every code example.
// No dependencies. Attached to all <pre> blocks, including ASCII diagrams.
document.addEventListener('DOMContentLoaded', () => {
    // Text without the button itself, preserving line breaks.
    function preText(pre) {
        const clone = pre.cloneNode(true);
        clone.querySelectorAll('.copy-btn').forEach((b) => b.remove());
        // innerText keeps newlines; trailing whitespace from markup is noise.
        return clone.innerText.replace(/\n+$/, '');
    }

    async function copyText(text) {
        if (navigator.clipboard && window.isSecureContext) {
            await navigator.clipboard.writeText(text);
            return;
        }
        // Fallback for non-secure contexts (e.g. raw IP access).
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed';
        ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        ta.remove();
    }

    document.querySelectorAll('pre').forEach((pre) => {
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'copy-btn';
        btn.textContent = 'copy';
        btn.setAttribute('aria-label', 'Copy to clipboard');
        btn.addEventListener('click', async () => {
            try {
                await copyText(preText(pre));
                btn.textContent = 'copied!';
                btn.classList.add('copied');
            } catch {
                btn.textContent = 'failed';
            }
            setTimeout(() => {
                btn.textContent = 'copy';
                btn.classList.remove('copied');
            }, 1600);
        });
        pre.appendChild(btn);
    });
});
