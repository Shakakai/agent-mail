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
        // Position inline as a fallback so a stale/missing stylesheet can
        // never leave the button floating mid-code; CSS refines the look.
        btn.style.position = 'absolute';
        btn.style.top = '0.45rem';
        btn.style.right = '0.45rem';
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

// Homepage mesh banner: rotate the HUD status line like a live log tail.
(function () {
    const el = document.getElementById('mesh-status');
    if (!el) return;
    const LINES = [
        '→ msg 01H4X… delivered :: research',
        '← ack 84ms :: aws-vm',
        '→ attach report.csv (12 KiB) :: human',
        '→ thread #41 reply :: cf-worker',
        '↻ queued → retry in 30s :: cron-01',
        '→ allow add 9c18…d403 (human approved)',
        '← inbox poll :: 0 unread :: manager',
        '✓ allowlist check :: OK :: home-pc',
    ];
    let i = 0;
    setInterval(() => {
        i = (i + 1) % LINES.length;
        el.textContent = LINES[i];
    }, 2800);
})();

// Tabbed install boxes (curl / pinned / source) with an always-visible Copy.
document.querySelectorAll('[data-install-switcher]').forEach((box) => {
    const tabs = box.querySelectorAll('[data-install-tab]');
    const panels = box.querySelectorAll('[data-install-panel]');
    const copyBtn = box.querySelector('[data-install-copy]');

    tabs.forEach((tab) => tab.addEventListener('click', () => {
        tabs.forEach((t) => {
            t.classList.toggle('is-active', t === tab);
            t.setAttribute('aria-selected', String(t === tab));
        });
        panels.forEach((p) => {
            p.hidden = p.dataset.installPanel !== tab.dataset.installTab;
        });
    }));

    if (copyBtn) {
        copyBtn.addEventListener('click', async () => {
            const visible = Array.from(panels).find((p) => !p.hidden);
            if (!visible) return;
            const text = visible.textContent.replace(/^\s*\$\s*/, '').trim();
            try {
                if (navigator.clipboard && window.isSecureContext) {
                    await navigator.clipboard.writeText(text);
                } else {
                    const ta = document.createElement('textarea');
                    ta.value = text;
                    ta.style.position = 'fixed';
                    ta.style.opacity = '0';
                    document.body.appendChild(ta);
                    ta.select();
                    document.execCommand('copy');
                    ta.remove();
                }
                copyBtn.textContent = 'Copied';
                copyBtn.classList.add('copied');
            } catch {
                copyBtn.textContent = 'Failed';
            }
            setTimeout(() => {
                copyBtn.textContent = 'Copy';
                copyBtn.classList.remove('copied');
            }, 1600);
        });
    }
});

// Fill [data-latest-version] slots with the current production release tag.
(function () {
    const slots = document.querySelectorAll('[data-latest-version]');
    if (!slots.length) return;
    fetch('https://api.github.com/repos/Shakakai/agent-mail/releases/latest')
        .then((r) => (r.ok ? r.json() : Promise.reject(new Error(r.status))))
        .then((rel) => slots.forEach((s) => { s.textContent = rel.tag_name; }))
        .catch(() => {});
})();
