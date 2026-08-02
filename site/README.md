# pirs project site

Static landing page for [GitHub Pages](https://xmonader.github.io/pirs/).

| File | Role |
|------|------|
| `index.html` | Landing page |
| `styles.css` | Layout & theme |
| `script.js` | Header scroll + mobile nav |

## Local preview

```bash
# from repo root
python3 -m http.server 8080 --directory site
# open http://localhost:8080
```

## Deploy

Push to `main` (or merge a PR). The workflow
[`.github/workflows/pages.yml`](../.github/workflows/pages.yml) publishes
`site/` to GitHub Pages.

**One-time setup** (repo admin):

1. **Settings → Pages → Build and deployment → Source:** GitHub Actions  
2. Ensure the `pages` workflow has permission to deploy (default GITHUB_TOKEN is enough with `pages: write` in the workflow).

URL: **https://xmonader.github.io/pirs/**

Edit copy in `index.html`; keep scores and claims aligned with
`qa/bench-swebench-5x5/LEADERBOARD.md` and `docs/SWE-QA.md`.
